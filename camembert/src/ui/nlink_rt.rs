//! Windows link counts, asked for where they are *consumed* rather than
//! where they are cheap (`docs/design/windows-nlink-dossier.md`, Option D's
//! surviving variant — §6 step 6).
//!
//! # What this buys back
//!
//! The scan stopped calling `NtQueryInformationByName` per file: it costs
//! ~46 µs, it was 95 % of a Windows scan, and it moved no total on any tree
//! measured. What it *did* buy is one narrow, thesis-shaped answer the
//! dossier's own attack (C.2) is right to call a real loss: **"this file has
//! links you cannot see, so deleting it here frees nothing"** — true of ~92 %
//! of files under `C:\Windows`, and of 728 of the 756 files in
//! `System32\drivers`.
//!
//! The insight that makes it affordable is that the candidate set at the
//! point of consumption is *one row*, not 200 000 files. One query is ~46 µs.
//!
//! # Why it still does not run on the UI thread
//!
//! 46 µs is nothing on a local NTFS volume — but a scan root on a UNC/SMB
//! path is not bounded by that measurement, and the render loop owes a frame
//! every 33 ms. So this follows the shape [`super::oracle`] already
//! established for the reclaim oracle: an off-thread job per candidate, a
//! [`LinkState::Pending`] placeholder while it is in flight, and an update
//! in place when it lands. Nothing here ever blocks a frame, on any volume,
//! for any reason.
//!
//! # What is never asked
//!
//! - **Anything, when the scan already knows** (`--links`/`LINKS`, i.e.
//!   [`camembert_core::scan::ScanOutcome::link_counts_known`]). A second
//!   query would be pure waste, and the card reads the scan's own count.
//! - **Directories.** The scan does not read a directory's link count
//!   either; NTFS reports 1 for every directory regardless.
//! - **Anything that is not a plain file** — symlinks, junctions, the WSL
//!   device-node kinds, unknown reparse tags. Those are exactly the entries
//!   `scan::windows::worker::classify` recognised *by their reparse tag*,
//!   which is the scan's own reparse guard as far as the tree records it.
//!   A WOF-compressed or OneDrive-backed file is [`Kind::File`] in the tree
//!   and *is* queried here: the scan skips those only because admitting them
//!   to the hardlink registry would change what `C:\Windows` deduplicates to
//!   (a separate, deliberately unopened question), not because the answer
//!   would be wrong. At the point of consumption there is no registry, so
//!   that reason does not apply and the count is simply true.
//! - **Volumes issuing no file identity.** FAT/exFAT/UDF and some SMB
//!   servers return the all-zero / all-`0xFF` sentinel the scan already
//!   refuses (`is_sentinel_id`). The count is still reported, but the
//!   "how many of them did this scan see" half is withheld rather than
//!   guessed — see [`LinkState::Known::anonymous`].
//!
//! # Memoisation
//!
//! Per node, and — where an inode identity is knowable at all — per inode
//! on top of it. The asymmetry is not a shortcut: on the Windows default the
//! scan retains an inode identity only for inodes it reached by **more than
//! one path** (the registry), so for a lone file there is nothing to key on
//! until the query itself answers. Where the registry does know the inode,
//! two visible links of one file share one query — which is the only case
//! where an inode key saves anything. Both maps are cleared on a deletion
//! epoch, exactly as [`super::oracle::OracleRuntime::on_deletion`] clears
//! its own (there is no in-app deletion on Windows today; the interlock is
//! here so that adding one cannot silently leave a stale answer on screen).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;

use tracing::{debug, warn};

use camembert_core::scan::ScanOutcome;
use camembert_core::tree::{Kind, NodeId};
use camembert_core::winlink::{LinkCount, LinkDir, QueryFailure};

use super::read_outcome;

/// Thread name for every spawned query job (visible in panics / a debugger).
const THREAD_NAME: &str = "camembert-links";

/// Resolved answers kept before the whole cache is dropped. A session that
/// arrows through a hundred thousand rows should not accumulate a map per
/// row; re-querying a row visited that long ago costs one 46 µs call.
const MAX_CACHED: usize = 8192;

// ---------------------------------------------------------------------------
// Pure data
// ---------------------------------------------------------------------------

/// What the selection card knows about one row's links.
///
/// Four states, and the two failure ones are deliberately distinct: "this
/// build or volume cannot answer" is a different fact from "this file said
/// no", and the scan already draws that line
/// ([`camembert_core::winlink::is_unsupported_status`]). Neither is ever
/// rendered as an absence — a blank card would read as "no links", which is
/// the one thing an unknown count must never look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// The query is in flight (or was just requested).
    Pending,
    /// The filesystem answered.
    Known {
        /// Links that exist on the volume, as of the query.
        total: u32,
        /// Paths this scan reached for the same inode. `1` unless the
        /// scan's hardlink registry holds the row's node, which under the
        /// Windows default means "reached by more than one path".
        seen: u32,
        /// The volume issues no file identity, so `seen` cannot be trusted
        /// to describe the same inode as `total` and is not shown.
        anonymous: bool,
    },
    /// This build or volume does not implement `FileStatInformation`.
    Unsupported,
    /// This particular file (or its directory) said no.
    Failed(QueryFailure),
}

/// The selection-card line(s) for `state`, in the same shape the ambient
/// floor's card lines use: a figure line first, at most one caveat under it.
///
/// One line, always — a row whose links are unknown says so rather than
/// falling silent, and the height of the card therefore does not depend on
/// whether a query happened to succeed.
pub fn card_lines(state: Option<LinkState>, spinner: char) -> Vec<String> {
    let Some(state) = state else {
        return Vec::new();
    };
    vec![match state {
        LinkState::Pending => format!("{spinner} counting links…"),
        LinkState::Known { total: 1, .. } => "1 link · nothing else points at this file".to_owned(),
        LinkState::Known {
            total,
            anonymous: true,
            ..
        } => format!("{total} links · this volume issues no file id, so none can be matched here"),
        LinkState::Known { total, seen, .. } if seen >= total => {
            format!("{total} links · all inside this scan")
        }
        LinkState::Known { total, seen, .. } => format!(
            "{total} links · {} outside this scan — deleting this frees nothing",
            total - seen
        ),
        LinkState::Unsupported => "links unknown · not reported on this volume".to_owned(),
        LinkState::Failed(why) => format!("links unknown · {}", why.label()),
    }]
}

/// Whether `node` is worth a query at all: a plain file, in a scan that did
/// not already obtain link counts. See the module docs for every exclusion
/// and its reason.
fn is_candidate(outcome: &ScanOutcome, node: NodeId) -> bool {
    !outcome.link_counts_known && outcome.node(node).kind() == Kind::File
}

/// How many paths this scan reached for `node`'s inode.
///
/// The scan's own identity, never a file id matched across two APIs: a
/// node absent from the registry was reached exactly once, which is what
/// "not registered" means under either link policy. Returns the inode key
/// too, when there is one, so the caller can memoise by it.
fn scan_reach(outcome: &ScanOutcome, node: NodeId) -> (u32, Option<(u64, u64)>) {
    let links = outcome.hardlink_links();
    let Some(mine) = links.iter().find(|link| link.node == node) else {
        return (1, None);
    };
    let key = (mine.dev, mine.ino);
    let seen = links
        .iter()
        .filter(|link| (link.dev, link.ino) == key)
        .count();
    (u32::try_from(seen).unwrap_or(u32::MAX), Some(key))
}

/// The all-zero / all-`0xFF` "this volume issues no unique id" sentinels,
/// in the 64-bit shape `FileStatInformation` returns them.
fn is_anonymous_id(file_id: i64) -> bool {
    file_id == 0 || file_id == -1
}

// ---------------------------------------------------------------------------
// The off-thread job
// ---------------------------------------------------------------------------

/// One query's result, tagged so [`LinkRuntime::poll`] can drop it when it
/// no longer applies (same (node, serial, epoch) guard the oracle uses).
struct JobMessage {
    epoch: u64,
    node: NodeId,
    serial: u64,
    state: LinkState,
    inode: Option<(u64, u64)>,
}

/// Everything the job needs off the frozen arena, read under one lock pass
/// and then released before any syscall runs.
struct JobInput {
    dir: PathBuf,
    name: OsString,
    seen: u32,
    inode: Option<(u64, u64)>,
}

fn job_input(outcome: &ScanOutcome, node: NodeId) -> Option<JobInput> {
    let path = outcome.tree().path_of_node(node);
    let dir = path.parent()?.to_path_buf();
    let name = path.file_name()?.to_os_string();
    let (seen, inode) = scan_reach(outcome, node);
    Some(JobInput {
        dir,
        name,
        seen,
        inode,
    })
}

/// Open the row's directory, query the row's name, and translate the answer
/// into the card's terms. Every failure path lands on a *stated* unknown.
fn run_job(outcome: &RwLock<ScanOutcome>, node: NodeId) -> (LinkState, Option<(u64, u64)>) {
    let input = {
        let guard = read_outcome(outcome);
        job_input(&guard, node)
    };
    let Some(input) = input else {
        return (LinkState::Failed(QueryFailure::Other), None);
    };

    let handle = match LinkDir::open(&input.dir) {
        Ok(handle) => handle,
        Err(err) => {
            debug!(dir = %input.dir.display(), %err, "link-count lookup: directory would not open");
            // The likeliest failure a user actually meets, and it deserves
            // its real reason rather than a generic one: a per-file ACL does
            // not refuse `FileStatInformation` (measured), so an
            // access-denied answer here almost always comes from the
            // *directory*.
            return (
                LinkState::Failed(QueryFailure::of_win32(err.raw_os_error())),
                input.inode,
            );
        }
    };
    let state = match handle.links_of(&input.name) {
        LinkCount::Known { links, file_id, .. } => LinkState::Known {
            total: links,
            seen: input.seen,
            anonymous: is_anonymous_id(file_id),
        },
        LinkCount::Unsupported => LinkState::Unsupported,
        LinkCount::Failed(why) => LinkState::Failed(why),
    };
    (state, input.inode)
}

fn spawn_job(
    outcome: Arc<RwLock<ScanOutcome>>,
    node: NodeId,
    epoch: u64,
    serial: u64,
    tx: Sender<JobMessage>,
) -> bool {
    let spawned = thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            let (state, inode) = run_job(&outcome, node);
            // The receiver may already be gone (session moved on); a failed
            // send just means nobody is listening anymore.
            let _ = tx.send(JobMessage {
                epoch,
                node,
                serial,
                state,
                inode,
            });
        });
    match spawned {
        Ok(_handle) => true,
        Err(err) => {
            warn!(%err, "failed to spawn the link-count job thread; this row's links stay unknown");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime (owned by the event loop, mirrors `OracleRuntime`)
// ---------------------------------------------------------------------------

/// The link lookup's session-long state: the job channel, the resolved
/// answers, and which nodes still have a query in flight.
///
/// Lives in the event loop exactly like the oracle's runtime does;
/// `ui::state::UiState` only ever holds the already-resolved
/// [`LinkState`] for the row the card is describing.
pub struct LinkRuntime {
    tx: Sender<JobMessage>,
    rx: Receiver<JobMessage>,
    next_serial: u64,
    /// node -> serial of the currently-outstanding query, absent once it
    /// has landed.
    pending: HashMap<NodeId, u64>,
    /// Resolved answers for the current epoch.
    results: HashMap<NodeId, LinkState>,
    /// The same answers keyed by inode, for the inodes the scan retained an
    /// identity for (see the module docs): two visible links of one file
    /// then cost one query, not two.
    by_inode: HashMap<(u64, u64), LinkState>,
    /// node -> inode, for the same set. Populated from landed results only,
    /// so it never costs a pass over the registry on the UI thread.
    inode_of: HashMap<NodeId, (u64, u64)>,
}

impl Default for LinkRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkRuntime {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            next_serial: 0,
            pending: HashMap::new(),
            results: HashMap::new(),
            by_inode: HashMap::new(),
            inode_of: HashMap::new(),
        }
    }

    /// The card's state for `node`, spawning the query if this is the first
    /// time the row has been asked about.
    ///
    /// Returns `None` for a row that must never be queried (see
    /// [`is_candidate`]) — the card then shows no link line at all, which is
    /// correct: a directory has nothing to say here, and saying "unknown"
    /// about it would be noise, not honesty.
    pub fn state_for(
        &mut self,
        lock: &Arc<RwLock<ScanOutcome>>,
        node: NodeId,
        epoch: u64,
    ) -> Option<LinkState> {
        if !is_candidate(&read_outcome(lock), node) {
            return None;
        }
        if let Some(state) = self.results.get(&node) {
            return Some(*state);
        }
        if let Some(state) = self
            .inode_of
            .get(&node)
            .and_then(|inode| self.by_inode.get(inode))
            .copied()
        {
            self.results.insert(node, state);
            return Some(state);
        }
        if self.pending.contains_key(&node) {
            return Some(LinkState::Pending);
        }
        if self.results.len() >= MAX_CACHED {
            debug!("link-count cache full; dropping it rather than growing per visited row");
            self.results.clear();
            self.by_inode.clear();
            self.inode_of.clear();
        }
        self.next_serial += 1;
        let serial = self.next_serial;
        if !spawn_job(Arc::clone(lock), node, epoch, serial, self.tx.clone()) {
            // No thread, no answer — and saying so beats a row that sits on
            // "counting links…" forever.
            let state = LinkState::Failed(QueryFailure::Other);
            self.results.insert(node, state);
            return Some(state);
        }
        self.pending.insert(node, serial);
        Some(LinkState::Pending)
    }

    /// Drives the render loop's frame cadence: a query is in flight, so the
    /// card owes an update within a frame or two of it landing.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Non-blocking poll of the job channel (the `try_recv` idiom every
    /// other off-thread runtime here uses). Returns whether anything landed.
    pub fn poll(&mut self, current_epoch: u64) -> bool {
        let mut landed = false;
        while let Ok(msg) = self.rx.try_recv() {
            landed = true;
            if msg.epoch != current_epoch {
                debug!(node = ?msg.node, "dropped a stale link-count result (epoch moved)");
                continue;
            }
            if self.pending.get(&msg.node) != Some(&msg.serial) {
                debug!(node = ?msg.node, "dropped a superseded link-count result");
                continue;
            }
            self.pending.remove(&msg.node);
            self.results.insert(msg.node, msg.state);
            if let Some(inode) = msg.inode {
                self.inode_of.insert(msg.node, inode);
                self.by_inode.insert(inode, msg.state);
            }
        }
        landed
    }

    /// A deletion just changed the frozen arena: every cached answer
    /// described a tree that no longer exists, and every in-flight job's
    /// result is dropped on arrival by [`Self::poll`]'s epoch check.
    ///
    /// Unreachable today (the Windows TUI has no deletion), and deliberately
    /// present anyway: the epoch is the interlock, and a future deletion
    /// path must inherit it rather than rediscover the need for it.
    #[allow(dead_code)]
    pub fn on_deletion(&mut self) {
        self.pending.clear();
        self.results.clear();
        self.by_inode.clear();
        self.inode_of.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(raw: u32) -> NodeId {
        NodeId::from_raw(raw)
    }

    // ---- wording (the honesty contract) ----------------------------------

    #[test]
    fn no_state_renders_no_line() {
        assert!(card_lines(None, '⠋').is_empty());
    }

    #[test]
    fn a_lone_link_says_so_rather_than_staying_silent() {
        assert_eq!(
            card_lines(
                Some(LinkState::Known {
                    total: 1,
                    seen: 1,
                    anonymous: false
                }),
                '⠋'
            ),
            vec!["1 link · nothing else points at this file".to_owned()]
        );
    }

    /// The `System32\drivers` case the whole feature exists for: three
    /// links, one of them here, two in WinSxS.
    #[test]
    fn links_outside_the_scan_are_counted_and_their_consequence_stated() {
        let line = card_lines(
            Some(LinkState::Known {
                total: 3,
                seen: 1,
                anonymous: false,
            }),
            '⠋',
        );
        assert_eq!(
            line,
            vec!["3 links · 2 outside this scan — deleting this frees nothing".to_owned()]
        );
    }

    #[test]
    fn a_wholly_seen_group_never_claims_anything_is_outside() {
        assert_eq!(
            card_lines(
                Some(LinkState::Known {
                    total: 2,
                    seen: 2,
                    anonymous: false
                }),
                '⠋'
            ),
            vec!["2 links · all inside this scan".to_owned()]
        );
        // A link removed between the scan and the query would make `seen`
        // exceed `total`; that must not underflow into a huge "outside"
        // figure.
        assert_eq!(
            card_lines(
                Some(LinkState::Known {
                    total: 2,
                    seen: 5,
                    anonymous: false
                }),
                '⠋'
            ),
            vec!["2 links · all inside this scan".to_owned()]
        );
    }

    /// A volume with no file identity can report a count but cannot relate
    /// it to what the scan walked, and must not pretend otherwise.
    #[test]
    fn an_anonymous_volume_withholds_the_scan_half() {
        let line = card_lines(
            Some(LinkState::Known {
                total: 4,
                seen: 1,
                anonymous: true,
            }),
            '⠋',
        );
        assert!(line[0].starts_with("4 links · this volume issues no file id"));
        assert!(!line[0].contains("outside this scan"));
    }

    /// Both failure shapes say *unknown*, and they do not say the same
    /// thing: the volume-wide one and the per-file one are different facts.
    #[test]
    fn failures_read_as_unknown_and_stay_distinguishable() {
        let unsupported = card_lines(Some(LinkState::Unsupported), '⠋');
        let denied = card_lines(Some(LinkState::Failed(QueryFailure::Denied)), '⠋');
        let gone = card_lines(Some(LinkState::Failed(QueryFailure::Vanished)), '⠋');
        for line in [&unsupported, &denied, &gone] {
            assert!(line[0].starts_with("links unknown · "), "{line:?}");
        }
        assert_ne!(unsupported, denied);
        assert_ne!(denied, gone);
    }

    #[test]
    fn pending_carries_the_spinner_char() {
        assert_eq!(
            card_lines(Some(LinkState::Pending), '⠙'),
            vec!["⠙ counting links…".to_owned()]
        );
    }

    #[test]
    fn sentinel_file_ids_are_the_two_the_scan_refuses() {
        assert!(is_anonymous_id(0));
        assert!(is_anonymous_id(-1));
        assert!(!is_anonymous_id(0x0001_0000_0000_2A3C));
    }

    // ---- runtime transitions ---------------------------------------------

    #[test]
    fn poll_moves_a_node_from_pending_to_resolved() {
        let mut rt = LinkRuntime::new();
        rt.pending.insert(n(1), 1);
        let state = LinkState::Known {
            total: 3,
            seen: 1,
            anonymous: false,
        };
        rt.tx
            .send(JobMessage {
                epoch: 0,
                node: n(1),
                serial: 1,
                state,
                inode: Some((7, 42)),
            })
            .expect("send");

        assert!(rt.poll(0));
        assert!(!rt.has_pending());
        assert_eq!(rt.results.get(&n(1)), Some(&state));
        assert_eq!(
            rt.by_inode.get(&(7, 42)),
            Some(&state),
            "a known inode identity memoises the answer for its other links"
        );
    }

    #[test]
    fn poll_drops_a_result_from_a_stale_epoch() {
        let mut rt = LinkRuntime::new();
        rt.pending.insert(n(1), 1);
        rt.tx
            .send(JobMessage {
                epoch: 0,
                node: n(1),
                serial: 1,
                state: LinkState::Unsupported,
                inode: None,
            })
            .expect("send");

        rt.poll(1);
        assert!(rt.has_pending(), "a stale result never resolves the query");
        assert!(rt.results.is_empty());
    }

    #[test]
    fn poll_drops_a_superseded_result() {
        let mut rt = LinkRuntime::new();
        rt.pending.insert(n(1), 2);
        rt.tx
            .send(JobMessage {
                epoch: 0,
                node: n(1),
                serial: 1,
                state: LinkState::Unsupported,
                inode: None,
            })
            .expect("send");

        rt.poll(0);
        assert_eq!(rt.pending.get(&n(1)), Some(&2));
        assert!(rt.results.is_empty());
    }

    #[test]
    fn on_deletion_clears_every_cache() {
        let mut rt = LinkRuntime::new();
        rt.pending.insert(n(1), 1);
        rt.results.insert(n(2), LinkState::Unsupported);
        rt.by_inode.insert((1, 1), LinkState::Unsupported);
        rt.inode_of.insert(n(2), (1, 1));

        rt.on_deletion();

        assert!(rt.pending.is_empty());
        assert!(rt.results.is_empty());
        assert!(rt.by_inode.is_empty());
        assert!(rt.inode_of.is_empty());
    }
}
