//! Freeable phase 2 slice 1 — the mark-time incremental selection oracle
//! (D4, `docs/design/freeable2-decisions.md`; the confirm-modal contract
//! redesigned per its condensed trail, attack-a finding [1]).
//!
//! The core `camembert_core::fiemap` module maps one file's extents and
//! [`correlate`]s a whole selection into an honest byte-bucket
//! [`OracleReport`]. This module is the UI-side wiring around it:
//!
//! - runs the (potentially slow — open+FIEMAP+backref-resolution per
//!   file) mapping work **incrementally, at mark time** rather than
//!   synchronously when the confirm modal opens — marking is the intent
//!   signal, so by the time `D` is pressed the answer is usually already
//!   there;
//! - keeps a per-`(dev, ino)` cache of what has already been mapped, so
//!   repeated marks of the same inode (a re-mark, or two hardlinks of one
//!   file marked separately) never re-pay the ioctl (attack-a finding 8);
//! - assembles the current selection's [`OracleReport`] from whatever has
//!   landed so far, and lets the confirm modal show a "computing…" state
//!   with a live spinner that **updates in place** once every marked
//!   node's job has landed — the redesigned contract attack-a finding [1]
//!   demanded in place of the original "advisory, never blocking" escape
//!   hatch, which evaporated the modal's flagship number exactly on the
//!   large selections that motivate a careful delete.
//!
//! Isolation (phase-1 D8, phase-2 D6): nothing here touches [`Node`] or
//! `tree.rs` beyond read-only walks of the already-frozen arena through
//! [`ScanOutcome::tree`]; no dump/diff impact. `--no-fiemap`/`NO_FIEMAP`
//! disables the whole module at the root — [`OracleRuntime::new`] with
//! `disabled: true` never spawns a job, so it makes zero FIEMAP calls
//! (attack-b finding 6, same contract the core module already documents).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;

use tracing::{debug, warn};

use camembert_core::confidence::{ReclaimFacts, Verdict};
use camembert_core::fiemap::{
    FileMap, FsTier, LinkStatus, OracleInput, OracleReport, correlate, map_file,
    shared_bit_reliable, tier_of,
};
use camembert_core::scan::{HardlinkGroup, ScanOutcome, path_on_compressed_mount};
use camembert_core::size::HumanSize;
use camembert_core::tree::{NodeId, Tree};

use super::read_outcome;

/// Cap on files walked into per mark's job (D4): beyond this, the walk
/// keeps counting bytes/files into the truncated bucket instead of
/// building a path for every one of them — a data-directory mark with
/// millions of files stays cheap while staying honest about what it
/// skipped (never silently dropped, see [`MarkFiles`]).
const MAX_FILES_PER_MARK: usize = 50_000;

/// Thread name for every spawned mark job (visible in `ps -T` / panics).
const THREAD_NAME: &str = "camembert-oracle";

// ---------------------------------------------------------------------------
// Pure data
// ---------------------------------------------------------------------------

/// One file discovered under a mark, with a fresh post-scan stat (D4 job
/// step (b)). No path: the oracle only ever needs identity + size once the
/// stat has run — dropping it here keeps [`MarkFiles`] cheap to hold for
/// the whole session.
#[derive(Debug, Clone, Copy)]
pub struct SelFile {
    pub node: NodeId,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
    /// Fresh `st_blocks * 512` — extent sharing is volatile, scan-time
    /// sizes may be stale (mirrors [`OracleInput::disk`]'s doc).
    pub disk: u64,
}

/// Result of one mark's background job.
#[derive(Debug, Clone, Default)]
pub struct MarkFiles {
    pub files: Vec<SelFile>,
    /// Files beyond the 50k cap, or vanished between scan time and the
    /// job's fresh stat: counted, never estimated (D2/D4 honesty — see
    /// [`OracleView::truncated_files`]).
    pub truncated_files: u64,
    /// Scan-time `disk` sum of the truncated files (both the cap overflow
    /// and the vanished ones — the last known size is the only honest
    /// number left to report for something no longer there to stat).
    pub truncated_disk: u64,
}

/// What the confirm modal shows for the D4 selection oracle
/// ([`super::state::ConfirmState::oracle`]).
#[derive(Debug, Clone)]
pub enum OracleSlot {
    /// `--no-fiemap`/`NO_FIEMAP`: no job ever ran, nothing to show — the
    /// modal renders exactly as it did before phase 2.
    Disabled,
    /// At least one marked node's job has not landed yet.
    Pending,
    /// Every marked node's job has landed; the report is current as of
    /// assembly (D3: not tracked past this — external writes are
    /// acknowledged, not watched).
    Ready(OracleView),
}

/// The D4-quantified answer plus the caveats the UI wording must carry
/// (D2: the honesty contract is load-bearing — see [`ready_wording`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleView {
    pub report: OracleReport,
    /// Any selected file's device has a `compress` mount option: physical
    /// reclaim can be smaller than `report`'s allocated-logical bytes say.
    pub compressed: bool,
    /// Kernel < 6.1: `report.exclusive` may overstate (`SHARED` false
    /// negatives under concurrent COW, see
    /// [`camembert_core::fiemap::shared_bit_reliable`]).
    pub old_kernel: bool,
    pub truncated_files: u64,
    pub truncated_disk: u64,
}

/// [`ready_wording`]'s output: the quantified reclaim lines, then the
/// caveat lines (dimmer styling in the modal — same idea as the phase-1
/// hardlink note).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OracleWording {
    pub summary: Vec<String>,
    pub caveats: Vec<String>,
}

/// The D4 wording (module docs, `docs/design/freeable2-decisions.md` D2):
/// `≥`/"up to"/"will not be freed" only, and a 0-exclusive figure on a
/// nonzero selection still renders the line rather than being suppressed
/// — see the `frees ≥ 0 exclusive` case exercised in the tests below.
pub fn ready_wording(view: &OracleView) -> OracleWording {
    let r = &view.report;
    let mut summary = vec![format!("frees ≥ {} exclusive", HumanSize(r.exclusive))];
    if r.shared_within > 0 {
        summary.push(format!(
            "+ up to {} shared only within the marked set",
            HumanSize(r.shared_within)
        ));
    }
    if r.shared_outside > 0 {
        let mut line = format!(
            "{} shared elsewhere will not be freed",
            HumanSize(r.shared_outside)
        );
        if r.hardlink_outside > 0 {
            line.push_str(&format!(
                " — includes {} hardlinked file(s) with links outside the selection",
                r.hardlink_outside
            ));
        }
        summary.push(line);
    }
    if r.unknown > 0 {
        summary.push(format!(
            "{} not estimated (unwritten or unmapped)",
            HumanSize(r.unknown)
        ));
    }
    if r.zfs_files > 0 {
        summary.push(format!(
            "{} file(s) on ZFS — no estimate: ZFS exposes no per-file sharing",
            r.zfs_files
        ));
    }

    let mut caveats = Vec::new();
    if view.compressed {
        caveats.push("compressed mount: physical reclaim may be smaller".to_owned());
    }
    if view.old_kernel {
        caveats
            .push("kernel < 6.1: sharing detection may overstate the exclusive figure".to_owned());
    }
    if view.truncated_files > 0 {
        caveats.push(format!(
            "{} file(s) beyond the 50k mapping cap — counted as not estimated",
            view.truncated_files
        ));
    }

    OracleWording { summary, caveats }
}

/// The Pending-state line, spinner char prefixed (the caller passes a
/// static `…` under `--no-motion`/`NO_MOTION` instead of an animated
/// frame — see `ui::draw_confirm_modal`).
pub fn pending_line(spinner: char) -> String {
    format!("{spinner} estimating actual reclaim… (y deletes without waiting)")
}

impl OracleView {
    /// This view restated as the core's plain [`ReclaimFacts`], the input
    /// to the confirm modal's headline verdict.
    pub fn facts(&self) -> ReclaimFacts {
        ReclaimFacts {
            report: self.report,
            compressed: self.compressed,
            old_kernel: self.old_kernel,
            truncated_files: self.truncated_files,
            truncated_disk: self.truncated_disk,
        }
    }
}

/// The confirm modal's headline [`Verdict`] for the current slot — the
/// single decision-grade judgement above the detailed caveat lines, which
/// keep their own places below ([`ready_wording`], the phase-1 open-file
/// advisory).
///
/// Both figure-less slots grade as `NoFigure` rather than as a weak
/// figure, with distinct reasons: [`OracleSlot::Pending`] is a genuine
/// confidence signal ("we do not know **yet**" — `y` stays live and acts
/// on what is known, per D4), not an error, and it flips to a graded rung
/// in place the moment the last job lands; [`OracleSlot::Disabled`] is
/// `--no-fiemap` showing nothing rather than a fallback (freeable2 D3).
pub fn verdict(slot: &OracleSlot) -> Verdict {
    match slot {
        OracleSlot::Disabled => Verdict::reclaim(None, true),
        OracleSlot::Pending => Verdict::reclaim(None, false),
        OracleSlot::Ready(view) => Verdict::reclaim(Some(&view.facts()), false),
    }
}

// ---------------------------------------------------------------------------
// Assembly (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Assemble the D4 report over the currently marked selection. Dedups by
/// `(dev, ino)` across every mark's [`MarkFiles`] (two marks holding two
/// links of one hardlinked inode contribute one [`OracleInput`], not two)
/// and derives each inode's [`LinkStatus`] against the *whole* selection's
/// node set — not just the mark that happened to discover it — which is
/// exactly what lets two separate marks jointly cover every link of one
/// inode and read as [`LinkStatus::AllLinksSelected`].
///
/// Only meaningful once every marked node's job has landed; the caller
/// ([`build_slot`]) is what decides that.
fn assemble<S: std::hash::BuildHasher>(
    marks: &[NodeId],
    results: &HashMap<NodeId, MarkFiles>,
    known_maps: &HashMap<(u64, u64), Option<FileMap>>,
    dev_tier: &HashMap<u64, FsTier>,
    dev_compressed: &HashMap<u64, bool>,
    hardlink_groups: &HashMap<(u64, u64), HardlinkGroup, S>,
    old_kernel: bool,
) -> OracleView {
    let mut selection_nodes: HashSet<NodeId> = HashSet::new();
    for mark in marks {
        if let Some(mf) = results.get(mark) {
            selection_nodes.extend(mf.files.iter().map(|f| f.node));
        }
    }

    let mut truncated_files = 0u64;
    let mut truncated_disk = 0u64;
    let mut dedup: HashMap<(u64, u64), &SelFile> = HashMap::new();
    for mark in marks {
        let Some(mf) = results.get(mark) else {
            continue;
        };
        truncated_files += mf.truncated_files;
        truncated_disk += mf.truncated_disk;
        for file in &mf.files {
            dedup.entry((file.dev, file.ino)).or_insert(file);
        }
    }

    let mut compressed = false;
    let mut inputs: Vec<OracleInput> = Vec::with_capacity(dedup.len());
    for (&(dev, ino), file) in &dedup {
        let tier = dev_tier.get(&dev).copied().unwrap_or(FsTier::HardlinkOnly);
        if dev_compressed.get(&dev).copied().unwrap_or(false) {
            compressed = true;
        }
        let map = if tier == FsTier::Extent {
            known_maps.get(&(dev, ino)).cloned().flatten()
        } else {
            None
        };
        let links = link_status(dev, ino, file.nlink, &selection_nodes, hardlink_groups);
        inputs.push(OracleInput {
            dev,
            ino,
            disk: file.disk,
            tier,
            links,
            map,
        });
    }

    OracleView {
        report: correlate(&inputs),
        compressed,
        old_kernel,
        truncated_files,
        truncated_disk,
    }
}

/// The D4 hardlink rule: freeable only when the selection holds every
/// scanned link of the inode **and** the scan saw every link that exists.
/// Absent from the registry with `nlink > 1` means the inode was
/// single-linked at scan time and gained a link since — unverifiable, so
/// it understates rather than guesses (`LinkStatus::LinksOutside`, module
/// docs on `camembert_core::fiemap::LinkStatus::Single`).
fn link_status<S: std::hash::BuildHasher>(
    dev: u64,
    ino: u64,
    nlink: u32,
    selection_nodes: &HashSet<NodeId>,
    hardlink_groups: &HashMap<(u64, u64), HardlinkGroup, S>,
) -> LinkStatus {
    if nlink <= 1 {
        return LinkStatus::Single;
    }
    match hardlink_groups.get(&(dev, ino)) {
        Some(group)
            if group.nodes.len() as u32 == group.nlink
                && group.nodes.iter().all(|n| selection_nodes.contains(n)) =>
        {
            LinkStatus::AllLinksSelected
        }
        _ => LinkStatus::LinksOutside,
    }
}

/// Decide Pending vs Ready and assemble when Ready (`open_delete_confirm`,
/// and the re-run on every job landing while the modal is open — attack-a
/// finding [1]'s redesigned in-place-update contract).
pub fn build_slot<S: std::hash::BuildHasher>(
    rt: &OracleRuntime,
    marks: &[NodeId],
    hardlink_groups: &HashMap<(u64, u64), HardlinkGroup, S>,
) -> OracleSlot {
    if rt.disabled {
        return OracleSlot::Disabled;
    }
    if !rt.all_landed(marks) {
        return OracleSlot::Pending;
    }
    OracleSlot::Ready(assemble(
        marks,
        &rt.results,
        &rt.known_maps,
        &rt.dev_tier,
        &rt.dev_compressed,
        hardlink_groups,
        rt.old_kernel,
    ))
}

// ---------------------------------------------------------------------------
// Enumeration + the off-thread job
// ---------------------------------------------------------------------------

/// Walk a mark's file nodes under one read-lock pass (generalizes the
/// `delete::hardlink_files_in` DFS idiom from "count hardlinks" to
/// "collect node/path/scan-disk triples", capped at
/// [`MAX_FILES_PER_MARK`]). The lock is held only for this walk — the
/// caller releases it before the slow per-file work.
fn enumerate_mark(
    outcome: &ScanOutcome,
    mark_node: NodeId,
) -> (Vec<(NodeId, PathBuf, u64)>, u64, u64) {
    let tree = outcome.tree();
    let mut collected = Vec::new();
    let mut truncated_files = 0u64;
    let mut truncated_disk = 0u64;

    match tree.dir_of(mark_node) {
        None => record_leaf(
            tree,
            mark_node,
            &mut collected,
            &mut truncated_files,
            &mut truncated_disk,
        ),
        Some(dir) => {
            let mut stack = vec![dir];
            while let Some(d) = stack.pop() {
                for child in tree.children(d) {
                    match tree.dir_of(child) {
                        Some(child_dir) => stack.push(child_dir),
                        None => record_leaf(
                            tree,
                            child,
                            &mut collected,
                            &mut truncated_files,
                            &mut truncated_disk,
                        ),
                    }
                }
            }
        }
    }
    (collected, truncated_files, truncated_disk)
}

fn record_leaf(
    tree: &Tree,
    node: NodeId,
    collected: &mut Vec<(NodeId, PathBuf, u64)>,
    truncated_files: &mut u64,
    truncated_disk: &mut u64,
) {
    let scan_disk = tree.node(node).size().real;
    if collected.len() < MAX_FILES_PER_MARK {
        collected.push((node, tree.path_of_node(node), scan_disk));
    } else {
        *truncated_files += 1;
        *truncated_disk += scan_disk;
    }
}

/// One mark's off-thread work (D4 steps (a)/(b)): enumerate under the read
/// lock, drop it, then per file — fresh `symlink_metadata` (vanished ->
/// truncated), a per-device [`tier_of`]/[`path_on_compressed_mount`] probe
/// (memoized within this job), and [`map_file`] on [`FsTier::Extent`]
/// inodes not already in `known_maps` (the incremental cache: repeated
/// marks of the same inode skip the ioctl entirely, attack-a finding 8).
/// [`run_job`]'s findings: the mark's files, plus newly-discovered
/// per-inode maps and per-device tier/compressed facts to merge into
/// [`OracleRuntime`]'s caches.
type JobFindings = (
    MarkFiles,
    Vec<((u64, u64), Option<FileMap>)>,
    Vec<(u64, FsTier)>,
    Vec<(u64, bool)>,
);

fn run_job(
    outcome: &RwLock<ScanOutcome>,
    mark_node: NodeId,
    known_maps: &HashMap<(u64, u64), Option<FileMap>>,
) -> JobFindings {
    use std::os::unix::fs::MetadataExt;

    let (leaves, mut truncated_files, mut truncated_disk) = {
        let guard = read_outcome(outcome);
        enumerate_mark(&guard, mark_node)
    };

    let mut sel_files = Vec::with_capacity(leaves.len());
    let mut new_maps = Vec::new();
    let mut dev_tier: HashMap<u64, FsTier> = HashMap::new();
    let mut dev_compressed: HashMap<u64, bool> = HashMap::new();

    for (node, path, scan_disk) in leaves {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                // Vanished since the scan: honestly unknown, never guessed.
                truncated_files += 1;
                truncated_disk += scan_disk;
                continue;
            }
        };
        let dev = meta.dev();
        let ino = meta.ino();
        let nlink = meta.nlink() as u32;
        let disk = meta.blocks() * 512;

        let tier = *dev_tier.entry(dev).or_insert_with(|| tier_of(&path));
        dev_compressed
            .entry(dev)
            .or_insert_with(|| path_on_compressed_mount(&path));

        if !known_maps.contains_key(&(dev, ino)) && tier == FsTier::Extent && meta.is_file() {
            let map = map_file(&path).ok();
            new_maps.push(((dev, ino), map));
        }

        sel_files.push(SelFile {
            node,
            dev,
            ino,
            nlink,
            disk,
        });
    }

    (
        MarkFiles {
            files: sel_files,
            truncated_files,
            truncated_disk,
        },
        new_maps,
        dev_tier.into_iter().collect(),
        dev_compressed.into_iter().collect(),
    )
}

/// One completed (or superseded/stale) job's result, tagged with the
/// deletion epoch and a per-node serial so [`OracleRuntime::poll`] can
/// drop it when it no longer applies (`spawn_filter_fold`'s
/// (fingerprint, epoch) guard, applied here as (node, serial, epoch)).
struct JobMessage {
    epoch: u64,
    mark_node: NodeId,
    serial: u64,
    files: MarkFiles,
    new_maps: Vec<((u64, u64), Option<FileMap>)>,
    new_dev_tiers: Vec<(u64, FsTier)>,
    new_dev_compressed: Vec<(u64, bool)>,
}

fn spawn_mark_job(
    outcome: Arc<RwLock<ScanOutcome>>,
    mark_node: NodeId,
    epoch: u64,
    serial: u64,
    known_maps: Arc<HashMap<(u64, u64), Option<FileMap>>>,
    tx: Sender<JobMessage>,
) {
    let spawned = thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            let (files, new_maps, new_dev_tiers, new_dev_compressed) =
                run_job(&outcome, mark_node, &known_maps);
            // The receiver may already be gone (session moved on); a failed
            // send just means nobody is listening anymore.
            let _ = tx.send(JobMessage {
                epoch,
                mark_node,
                serial,
                files,
                new_maps,
                new_dev_tiers,
                new_dev_compressed,
            });
        });
    if let Err(err) = spawned {
        warn!(%err, "failed to spawn the oracle job thread; selection oracle skipped for this mark");
    }
}

// ---------------------------------------------------------------------------
// Runtime (owned by the event loop, mirrors `sweep_rx`/`PaletteRuntime`)
// ---------------------------------------------------------------------------

/// The oracle's session-long IO/clock-owning runtime: the job channel, the
/// per-`(dev, ino)` map cache and per-device tier/compressed memo (both
/// epoch-valid, cleared on a deletion), the landed-per-mark results, and
/// which marks still have a job in flight. Lives in the event loop exactly
/// like `sweep_rx`/`PaletteRuntime` — [`super::state::UiState`] stays
/// terminal/IO-free and only ever holds the assembled, already-resolved
/// [`OracleSlot`] inside its `ConfirmState`.
pub struct OracleRuntime {
    /// `--no-fiemap`/`NO_FIEMAP`: no job is ever spawned, and
    /// [`build_slot`] always returns [`OracleSlot::Disabled`].
    disabled: bool,
    /// `!shared_bit_reliable()`, computed once — cheap (a single `uname`),
    /// but no reason to repeat it every assembly.
    old_kernel: bool,
    tx: Sender<JobMessage>,
    rx: Receiver<JobMessage>,
    /// Monotonic per-node job identity: guards against a rapid
    /// unmark+remark of the same row resurrecting the *first* job's stale
    /// result after the *second* job's is what should land (see
    /// [`Self::mark_added`]/[`Self::poll`]).
    next_serial: u64,
    /// node -> the serial of the currently-outstanding job for it, absent
    /// once landed (or removed).
    pending: HashMap<NodeId, u64>,
    /// Landed results, current epoch only.
    results: HashMap<NodeId, MarkFiles>,
    known_maps: HashMap<(u64, u64), Option<FileMap>>,
    dev_tier: HashMap<u64, FsTier>,
    dev_compressed: HashMap<u64, bool>,
}

impl OracleRuntime {
    pub fn new(disabled: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            disabled,
            // Computing this when disabled would still be harmless (it's
            // a `uname` call), but there's no reason to pay it for a
            // session that will never render an oracle line at all.
            old_kernel: !disabled && !shared_bit_reliable(),
            tx,
            rx,
            next_serial: 0,
            pending: HashMap::new(),
            results: HashMap::new(),
            known_maps: HashMap::new(),
            dev_tier: HashMap::new(),
            dev_compressed: HashMap::new(),
        }
    }

    /// A mark was just added (space on a row, post-scan only — see the
    /// call sites): spawn its job, unless disabled. `epoch` is the current
    /// deletion epoch; a result landing after the epoch has moved is
    /// dropped in [`Self::poll`].
    pub fn mark_added(&mut self, outcome: Arc<RwLock<ScanOutcome>>, node: NodeId, epoch: u64) {
        if self.disabled {
            return;
        }
        self.results.remove(&node);
        self.next_serial += 1;
        let serial = self.next_serial;
        self.pending.insert(node, serial);
        let known = Arc::new(self.known_maps.clone());
        spawn_mark_job(outcome, node, epoch, serial, known, self.tx.clone());
    }

    /// A mark was just removed: drop its result/pending entry. A job still
    /// in flight for it is left to finish — its result is dropped on
    /// arrival by [`Self::poll`] (no pending entry left to match its
    /// serial against). The per-inode/per-device caches are kept; they
    /// stay epoch-valid regardless of which marks reference them.
    pub fn mark_removed(&mut self, node: NodeId) {
        self.results.remove(&node);
        self.pending.remove(&node);
    }

    /// `u`: every mark just cleared at once.
    pub fn clear_marks(&mut self) {
        self.results.clear();
        self.pending.clear();
    }

    /// A deletion just bumped the flat/deletion epoch: the frozen arena
    /// changed, so every cached fact (inode maps, per-device memos,
    /// per-mark results, in-flight job identities) is invalidated. Any
    /// message still in flight is dropped by [`Self::poll`]'s epoch check.
    pub fn on_deletion(&mut self) {
        self.known_maps.clear();
        self.dev_tier.clear();
        self.dev_compressed.clear();
        self.results.clear();
        self.pending.clear();
    }

    /// Drives the spinner and `needs_frequent_polling`: whether any
    /// spawned job hasn't landed yet.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether every currently marked node's job has landed (no pending
    /// entry left for any of them) — [`build_slot`]'s Pending/Ready
    /// switch. A spawn failure (logged, never fatal) never leaves a mark
    /// "pending forever": it simply never entered `pending`, so once every
    /// job that *did* spawn has landed, the selection reads Ready with
    /// that file's bytes silently absent from the report rather than
    /// blocking the modal — an accepted, rare-failure trade-off, exactly
    /// like `spawn_freeable_sweep`'s own "no thread, no data" fallback.
    fn all_landed(&self, marks: &[NodeId]) -> bool {
        !marks.iter().any(|node| self.pending.contains_key(node))
    }

    /// Non-blocking poll of the job channel (the `spawn_freeable_sweep`
    /// `try_recv` idiom). Returns whether anything landed at all (the
    /// caller only needs to re-run [`build_slot`] when it did).
    /// `is_marked` lets a result for a node unmarked since its job was
    /// spawned be dropped instead of resurrecting stale data.
    pub fn poll(&mut self, current_epoch: u64, is_marked: impl Fn(NodeId) -> bool) -> bool {
        let mut landed = false;
        while let Ok(msg) = self.rx.try_recv() {
            landed = true;
            if msg.epoch != current_epoch {
                debug!(node = ?msg.mark_node, "dropped a stale oracle job result (epoch moved)");
                continue;
            }
            for (key, map) in msg.new_maps {
                self.known_maps.entry(key).or_insert(map);
            }
            for (dev, tier) in msg.new_dev_tiers {
                self.dev_tier.entry(dev).or_insert(tier);
            }
            for (dev, compressed) in msg.new_dev_compressed {
                self.dev_compressed.entry(dev).or_insert(compressed);
            }
            if self.pending.get(&msg.mark_node) != Some(&msg.serial) {
                // A newer job for this node is in flight (rapid
                // unmark+remark), or it was unmarked outright.
                debug!(node = ?msg.mark_node, "dropped a superseded oracle job result");
                continue;
            }
            self.pending.remove(&msg.mark_node);
            if is_marked(msg.mark_node) {
                self.results.insert(msg.mark_node, msg.files);
            }
        }
        landed
    }

    #[cfg(test)]
    fn inject(&self, msg_epoch: u64, mark_node: NodeId, serial: u64, files: MarkFiles) {
        let _ = self.tx.send(JobMessage {
            epoch: msg_epoch,
            mark_node,
            serial,
            files,
            new_maps: Vec::new(),
            new_dev_tiers: Vec::new(),
            new_dev_compressed: Vec::new(),
        });
    }

    #[cfg(test)]
    fn pending_serial(&self, node: NodeId) -> Option<u64> {
        self.pending.get(&node).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(raw: u32) -> NodeId {
        NodeId::from_raw(raw)
    }

    fn sel(node: NodeId, dev: u64, ino: u64, nlink: u32, disk: u64) -> SelFile {
        SelFile {
            node,
            dev,
            ino,
            nlink,
            disk,
        }
    }

    fn mark_files(files: Vec<SelFile>) -> MarkFiles {
        MarkFiles {
            files,
            truncated_files: 0,
            truncated_disk: 0,
        }
    }

    fn group(nodes: Vec<NodeId>, nlink: u32) -> HardlinkGroup {
        HardlinkGroup { nodes, nlink }
    }

    // ---- assemble: dedup by inode ---------------------------------------

    #[test]
    fn assemble_dedups_two_marks_sharing_one_inode() {
        let (m1, m2) = (n(1), n(2));
        let mut results = HashMap::new();
        results.insert(m1, mark_files(vec![sel(n(10), 1, 100, 1, 4096)]));
        results.insert(m2, mark_files(vec![sel(n(11), 1, 100, 1, 4096)]));
        // Same (dev=1, ino=100) reachable from two distinct marked nodes
        // (e.g. two hardlinks marked separately) -- inode identity dedups,
        // not node identity.
        let hardlink_groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        let mut dev_tier = HashMap::new();
        dev_tier.insert(1, FsTier::HardlinkOnly);

        let view = assemble(
            &[m1, m2],
            &results,
            &HashMap::new(),
            &dev_tier,
            &HashMap::new(),
            &hardlink_groups,
            false,
        );
        assert_eq!(view.report.inodes, 1, "one physical inode, not two");
        assert_eq!(view.report.exclusive, 4096);
    }

    // ---- link_status ------------------------------------------------------

    #[test]
    fn link_status_single_link_is_never_pinned() {
        let selection: HashSet<NodeId> = HashSet::new();
        let groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        assert_eq!(
            link_status(1, 1, 1, &selection, &groups),
            LinkStatus::Single
        );
    }

    #[test]
    fn link_status_absent_from_registry_with_nlink_above_one_is_links_outside() {
        // The inode was single-linked at scan time (so never entered the
        // registry) and gained a link since -- unverifiable, must
        // understate rather than guess.
        let selection: HashSet<NodeId> = HashSet::new();
        let groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        assert_eq!(
            link_status(1, 1, 2, &selection, &groups),
            LinkStatus::LinksOutside
        );
    }

    #[test]
    fn link_status_all_scanned_links_selected_and_complete() {
        let (a, b) = (n(1), n(2));
        let selection: HashSet<NodeId> = [a, b].into_iter().collect();
        let mut groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        groups.insert((1, 1), group(vec![a, b], 2));
        assert_eq!(
            link_status(1, 1, 2, &selection, &groups),
            LinkStatus::AllLinksSelected
        );
    }

    #[test]
    fn link_status_one_scanned_link_outside_selection_pins_the_inode() {
        let (a, b) = (n(1), n(2));
        let selection: HashSet<NodeId> = [a].into_iter().collect(); // b not selected
        let mut groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        groups.insert((1, 1), group(vec![a, b], 2));
        assert_eq!(
            link_status(1, 1, 2, &selection, &groups),
            LinkStatus::LinksOutside
        );
    }

    #[test]
    fn link_status_scan_did_not_see_every_link_even_if_selected_ones_are_all_in() {
        let a = n(1);
        let selection: HashSet<NodeId> = [a].into_iter().collect();
        let mut groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        // Registry only saw one link, but nlink says 3 exist.
        groups.insert((1, 1), group(vec![a], 3));
        assert_eq!(
            link_status(1, 1, 3, &selection, &groups),
            LinkStatus::LinksOutside
        );
    }

    #[test]
    fn assemble_two_marks_jointly_cover_both_links_of_one_inode() {
        // Mark A holds node 10 (link 1 of inode (1,1)), mark B holds node
        // 11 (link 2) -- together they cover every scanned link, so the
        // inode reads AllLinksSelected and its bytes land in `exclusive`
        // rather than `shared_outside`.
        let (m_a, m_b) = (n(1), n(2));
        let (link1, link2) = (n(10), n(11));
        let mut results = HashMap::new();
        results.insert(m_a, mark_files(vec![sel(link1, 1, 1, 2, 4096)]));
        results.insert(m_b, mark_files(vec![sel(link2, 1, 1, 2, 4096)]));
        let mut hardlink_groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        hardlink_groups.insert((1, 1), group(vec![link1, link2], 2));
        let mut dev_tier = HashMap::new();
        dev_tier.insert(1, FsTier::HardlinkOnly);

        let view = assemble(
            &[m_a, m_b],
            &results,
            &HashMap::new(),
            &dev_tier,
            &HashMap::new(),
            &hardlink_groups,
            false,
        );
        assert_eq!(view.report.inodes, 1);
        assert_eq!(view.report.exclusive, 4096, "both links selected: freeable");
        assert_eq!(view.report.shared_outside, 0);
        assert_eq!(view.report.hardlink_outside, 0);
    }

    #[test]
    fn assemble_marking_only_one_of_two_links_pins_the_inode() {
        let m_a = n(1);
        let (link1, link2) = (n(10), n(11));
        let mut results = HashMap::new();
        results.insert(m_a, mark_files(vec![sel(link1, 1, 1, 2, 4096)]));
        let mut hardlink_groups: HashMap<(u64, u64), HardlinkGroup> = HashMap::new();
        // link2 exists and was scanned but is not in any mark's results.
        hardlink_groups.insert((1, 1), group(vec![link1, link2], 2));
        let mut dev_tier = HashMap::new();
        dev_tier.insert(1, FsTier::HardlinkOnly);

        let view = assemble(
            &[m_a],
            &results,
            &HashMap::new(),
            &dev_tier,
            &HashMap::new(),
            &hardlink_groups,
            false,
        );
        assert_eq!(view.report.exclusive, 0);
        assert_eq!(view.report.shared_outside, 4096);
        assert_eq!(view.report.hardlink_outside, 1);
    }

    // ---- truncation accounting --------------------------------------------

    #[test]
    fn assemble_sums_truncation_across_every_mark() {
        let (m1, m2) = (n(1), n(2));
        let mut results = HashMap::new();
        results.insert(
            m1,
            MarkFiles {
                files: vec![sel(n(10), 1, 1, 1, 100)],
                truncated_files: 5,
                truncated_disk: 5000,
            },
        );
        results.insert(
            m2,
            MarkFiles {
                files: vec![sel(n(11), 1, 2, 1, 200)],
                truncated_files: 3,
                truncated_disk: 3000,
            },
        );
        let mut dev_tier = HashMap::new();
        dev_tier.insert(1, FsTier::HardlinkOnly);
        let view = assemble(
            &[m1, m2],
            &results,
            &HashMap::new(),
            &dev_tier,
            &HashMap::new(),
            &HashMap::<(u64, u64), HardlinkGroup>::new(),
            false,
        );
        assert_eq!(view.truncated_files, 8);
        assert_eq!(view.truncated_disk, 8000);
    }

    // ---- caveat flags -------------------------------------------------------

    #[test]
    fn assemble_flags_compressed_when_any_selected_dev_is_compressed() {
        let m1 = n(1);
        let mut results = HashMap::new();
        results.insert(m1, mark_files(vec![sel(n(10), 1, 1, 1, 100)]));
        let mut dev_tier = HashMap::new();
        dev_tier.insert(1, FsTier::HardlinkOnly);
        let mut dev_compressed = HashMap::new();
        dev_compressed.insert(1, true);

        let view = assemble(
            &[m1],
            &results,
            &HashMap::new(),
            &dev_tier,
            &dev_compressed,
            &HashMap::<(u64, u64), HardlinkGroup>::new(),
            false,
        );
        assert!(view.compressed);
    }

    #[test]
    fn assemble_passes_old_kernel_through_unchanged() {
        let view = assemble(
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::<(u64, u64), HardlinkGroup>::new(),
            true,
        );
        assert!(view.old_kernel);
    }

    // ---- wording (D2 honesty contract) -------------------------------------

    fn view(report: OracleReport) -> OracleView {
        OracleView {
            report,
            compressed: false,
            old_kernel: false,
            truncated_files: 0,
            truncated_disk: 0,
        }
    }

    #[test]
    fn zero_exclusive_on_a_nonzero_selection_still_renders_the_line() {
        // D2: "fully shared" must never be suppressed -- it is the
        // feature's most informative answer, not its null case.
        let report = OracleReport {
            exclusive: 0,
            shared_within: 4096,
            ..Default::default()
        };
        let wording = ready_wording(&view(report));
        assert_eq!(wording.summary[0], "frees ≥ 0 B exclusive");
        assert!(wording.summary[1].contains("up to"));
    }

    #[test]
    fn wording_never_promises_an_exact_figure() {
        let report = OracleReport {
            exclusive: 1024,
            shared_within: 2048,
            shared_outside: 512,
            unknown: 256,
            zfs_files: 1,
            zfs_bytes: 4096,
            hardlink_outside: 1,
            ..Default::default()
        };
        let wording = ready_wording(&view(report));
        let joined = wording.summary.join(" / ");
        assert!(joined.contains("frees ≥"));
        assert!(joined.contains("up to"));
        assert!(joined.contains("will not be freed"));
        assert!(joined.contains("hardlinked file(s) with links outside the selection"));
        assert!(joined.contains("not estimated"));
        assert!(joined.contains("ZFS exposes no per-file sharing"));
        assert!(!joined.to_lowercase().contains("exactly"));
    }

    #[test]
    fn caveats_appear_only_when_flagged() {
        let mut v = view(OracleReport::default());
        assert!(ready_wording(&v).caveats.is_empty());
        v.compressed = true;
        assert_eq!(
            ready_wording(&v).caveats,
            vec!["compressed mount: physical reclaim may be smaller".to_owned()]
        );
        v.old_kernel = true;
        v.truncated_files = 7;
        let caveats = ready_wording(&v).caveats;
        assert_eq!(caveats.len(), 3);
        assert!(caveats[2].contains("7 file(s) beyond the 50k mapping cap"));
    }

    #[test]
    fn pending_line_carries_the_spinner_char() {
        assert_eq!(
            pending_line('⠋'),
            "⠋ estimating actual reclaim… (y deletes without waiting)"
        );
    }

    // ---- OracleRuntime transitions -----------------------------------------

    #[test]
    fn disabled_runtime_never_reports_pending_and_slot_is_always_disabled() {
        let rt = OracleRuntime::new(true);
        assert!(!rt.has_pending());
        let slot = build_slot(&rt, &[n(1)], &HashMap::<(u64, u64), HardlinkGroup>::new());
        assert!(matches!(slot, OracleSlot::Disabled));
    }

    #[test]
    fn a_mark_with_no_landed_job_is_pending() {
        let mut rt = OracleRuntime::new(false);
        // Simulate `mark_added`'s bookkeeping without spawning a thread.
        rt.pending.insert(n(1), 1);
        let slot = build_slot(&rt, &[n(1)], &HashMap::<(u64, u64), HardlinkGroup>::new());
        assert!(matches!(slot, OracleSlot::Pending));
    }

    #[test]
    fn poll_moves_a_mark_from_pending_to_ready_once_its_result_lands() {
        let mut rt = OracleRuntime::new(false);
        rt.pending.insert(n(1), 1);
        rt.inject(0, n(1), 1, mark_files(vec![sel(n(10), 1, 1, 1, 4096)]));

        let landed = rt.poll(0, |node| node == n(1));
        assert!(landed);
        assert_eq!(rt.pending_serial(n(1)), None, "resolved");

        let slot = build_slot(&rt, &[n(1)], &HashMap::<(u64, u64), HardlinkGroup>::new());
        assert!(matches!(slot, OracleSlot::Ready(_)));
    }

    #[test]
    fn poll_drops_a_result_from_a_stale_epoch() {
        let mut rt = OracleRuntime::new(false);
        rt.pending.insert(n(1), 1);
        rt.inject(0, n(1), 1, mark_files(vec![sel(n(10), 1, 1, 1, 4096)]));

        // The deletion epoch already moved to 1 by the time this lands.
        rt.poll(1, |_| true);
        assert_eq!(
            rt.pending_serial(n(1)),
            Some(1),
            "stale result never resolves the pending entry"
        );
    }

    #[test]
    fn poll_drops_a_superseded_result_from_a_rapid_remark() {
        let mut rt = OracleRuntime::new(false);
        // First job (serial 1) spawned, then the row was unmarked and
        // remarked before it landed (serial 2 now current).
        rt.pending.insert(n(1), 2);
        rt.inject(0, n(1), 1, mark_files(vec![sel(n(10), 1, 1, 1, 111)]));

        rt.poll(0, |_| true);
        assert_eq!(
            rt.pending_serial(n(1)),
            Some(2),
            "the outdated job's arrival must not resolve the current one"
        );
        assert!(
            !rt.results.contains_key(&n(1)),
            "stale serial's data never gets stored"
        );
    }

    #[test]
    fn poll_drops_a_result_for_a_node_no_longer_marked() {
        let mut rt = OracleRuntime::new(false);
        rt.pending.insert(n(1), 1);
        rt.inject(0, n(1), 1, mark_files(vec![sel(n(10), 1, 1, 1, 111)]));

        rt.poll(0, |_| false); // unmarked in the meantime
        assert!(!rt.results.contains_key(&n(1)));
    }

    #[test]
    fn mark_removed_drops_the_result_and_pending_entry() {
        let mut rt = OracleRuntime::new(false);
        rt.pending.insert(n(1), 1);
        rt.results.insert(n(1), mark_files(vec![]));
        rt.mark_removed(n(1));
        assert!(rt.pending_serial(n(1)).is_none());
        assert!(!rt.results.contains_key(&n(1)));
    }

    #[test]
    fn on_deletion_clears_every_cache() {
        let mut rt = OracleRuntime::new(false);
        rt.pending.insert(n(1), 1);
        rt.results.insert(n(1), mark_files(vec![]));
        rt.known_maps.insert((1, 1), None);
        rt.dev_tier.insert(1, FsTier::Extent);
        rt.dev_compressed.insert(1, true);

        rt.on_deletion();

        assert!(rt.pending.is_empty());
        assert!(rt.results.is_empty());
        assert!(rt.known_maps.is_empty());
        assert!(rt.dev_tier.is_empty());
        assert!(rt.dev_compressed.is_empty());
    }

    /// The confirm modal's headline verdict for each slot. A pending
    /// oracle must read as "not known yet", never as an error and never
    /// as a graded-but-weak figure — and it must become a graded rung the
    /// moment the report lands, since the modal re-renders in place.
    #[test]
    fn slot_verdicts_grade_pending_and_disabled_as_absences() {
        assert_eq!(
            verdict(&OracleSlot::Pending).line(),
            "no figure — still mapping the selection"
        );
        assert_eq!(
            verdict(&OracleSlot::Disabled).line(),
            "no figure — extent mapping is off (--no-fiemap)"
        );
        let ready = OracleSlot::Ready(OracleView {
            report: OracleReport {
                exclusive: 4096,
                inodes: 1,
                mapped: 1,
                ..OracleReport::default()
            },
            compressed: false,
            old_kernel: false,
            truncated_files: 0,
            truncated_disk: 0,
        });
        assert_eq!(
            verdict(&ready).line(),
            "measured — every marked file accounted for"
        );
    }

    #[test]
    fn clear_marks_empties_results_and_pending_only() {
        let mut rt = OracleRuntime::new(false);
        rt.pending.insert(n(1), 1);
        rt.results.insert(n(2), mark_files(vec![]));
        rt.known_maps.insert((1, 1), None);

        rt.clear_marks();

        assert!(rt.pending.is_empty());
        assert!(rt.results.is_empty());
        assert!(
            !rt.known_maps.is_empty(),
            "the inode cache stays epoch-valid across a plain unmark-all"
        );
    }
}
