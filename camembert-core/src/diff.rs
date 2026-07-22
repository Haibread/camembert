//! Streaming diff of two ordered camembert dumps (spec §4/§7).
//!
//! The two dumps are consumed as a **merge-join over their directory
//! blocks**, never loaded whole (the format's raison d'être — see
//! `docs/design/dump-format-research.md` §3): tier-2 ordering makes each
//! dump a totally ordered stream of blocks under the component-wise
//! raw-byte path comparator, so one cursor per side and a bounded window
//! suffice. Within a matched block pair, entries are merge-joined by raw
//! name bytes (tier-1 ordering).
//!
//! Memory bounds: one block per side, the two top-N heaps, and a stack of
//! "pending one-sided entries" frames along the current ancestor chain
//! (needed to tell a *file replaced by a directory* — which surfaces as a
//! removed entry plus an added block — apart from a plain removal). The
//! frame stack is bounded by tree depth × directory fanout.
//!
//! Directory deltas come from the `d`-line subtree totals, which already
//! carry canonical hardlink attribution (§8) — the differ never re-derives
//! them. Per-entry deltas use the entry's own sizes; a hardlink extra link
//! therefore shows its full size in the *entry* list even though it
//! contributed 0 to every directory total (documented limitation).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io::Read;

use rustc_hash::FxHashMap;
use tracing::debug;

use crate::dump::encode::JsonLine;
use crate::dump::encode_name;
use crate::dump::read::{DirBlock, DumpReader, Entry, ReadError, Totals};

/// Which of the two dumps an error refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Old => "old",
            Side::New => "new",
        })
    }
}

/// Errors that abort a diff.
#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error("{side} dump: {source}")]
    Read {
        side: Side,
        #[source]
        source: ReadError,
    },
    /// The header says `"ordered":false`: the constant-window merge-join
    /// needs tier-2 ordering (spec §7).
    #[error(
        "{side} dump is unordered (header \"ordered\":false): the streaming diff needs tier-2 \
         ordering. Re-create it with `camembert <path> -o dump.cmbt` (the ordered writer); \
         `camembert dump sort` (planned) will upgrade unordered dumps in place"
    )]
    Unordered { side: Side },
    /// No `e` end marker: the dump is a torn prefix (spec §9); diffing it
    /// would report phantom removals for everything past the tear.
    #[error(
        "{side} dump is incomplete (no `e` end marker — dump truncated): refusing to diff, the \
         missing tail would show up as phantom changes"
    )]
    Incomplete { side: Side },
    /// Structurally invalid content (bad root prefix, out-of-order blocks
    /// or entries, missing totals) in an allegedly ordered dump.
    #[error("{side} dump: {msg}")]
    Invalid { side: Side, msg: String },
}

/// Diff tuning.
#[derive(Debug, Clone, Copy)]
pub struct DiffOptions {
    /// Keep the `top` largest directory deltas and entry deltas (by
    /// absolute disk delta).
    pub top: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self { top: 20 }
    }
}

/// How an entry changed between the two dumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Removed,
    Grown,
    Shrunk,
    /// Same sizes, different mtime.
    Touched,
    /// Kind changed (file ↔ symlink/device/…, or file ↔ directory).
    TypeChanged,
}

impl Change {
    /// Stable machine name (JSON output).
    pub fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Removed => "removed",
            Change::Grown => "grown",
            Change::Shrunk => "shrunk",
            Change::Touched => "touched",
            Change::TypeChanged => "typeChanged",
        }
    }
}

/// How a directory changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirChange {
    Added,
    Removed,
    /// Present on both sides with different subtree totals.
    Changed,
}

impl DirChange {
    pub fn as_str(self) -> &'static str {
        match self {
            DirChange::Added => "added",
            DirChange::Removed => "removed",
            DirChange::Changed => "changed",
        }
    }
}

/// One directory's subtree delta (from the `d`-line totals).
#[derive(Debug, Clone)]
pub struct DirDelta {
    /// Full path, raw bytes.
    pub path: Vec<u8>,
    pub change: DirChange,
    pub disk_delta: i64,
    pub apparent_delta: i64,
    pub entry_delta: i64,
}

/// One entry's delta.
#[derive(Debug, Clone)]
pub struct EntryDelta {
    /// Full path, raw bytes.
    pub path: Vec<u8>,
    pub change: Change,
    pub disk_delta: i64,
    pub apparent_delta: i64,
}

/// Classification counters (entries are non-directories; directories are
/// counted separately because their deltas are subtree aggregates).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffCounts {
    pub added: u64,
    pub removed: u64,
    pub grown: u64,
    pub shrunk: u64,
    pub touched: u64,
    pub type_changed: u64,
    pub dirs_added: u64,
    pub dirs_removed: u64,
}

impl DiffCounts {
    /// Entries changed in place (everything but added/removed).
    pub fn changed(&self) -> u64 {
        self.grown + self.shrunk + self.touched + self.type_changed
    }
}

/// Result of a diff: totals, counts and the two bounded top lists.
#[derive(Debug, Clone)]
pub struct DiffReport {
    /// Root paths of the two dumps (raw bytes).
    pub old_root: Vec<u8>,
    pub new_root: Vec<u8>,
    /// Whole-tree deltas (new minus old, from the root totals; canonical
    /// hardlink attribution per spec §8).
    pub disk_delta: i64,
    pub apparent_delta: i64,
    pub entry_delta: i64,
    pub counts: DiffCounts,
    /// Top directories by absolute disk delta, sorted signed descending
    /// (biggest growth first, shrinkage negative, ties by path).
    pub top_dirs: Vec<DirDelta>,
    /// Top entries by absolute disk delta, same order.
    pub top_entries: Vec<EntryDelta>,
}

impl DiffReport {
    /// Render the report as JSON Lines (the `--json` machine format):
    /// one `summary` object, then one object per top directory and top
    /// entry. Paths are percent-encoded like dump names (spec §4); u64/i64
    /// values ≥ 2^53 are decimal strings (spec §5).
    pub fn to_json_lines(&self) -> String {
        let mut out = String::new();
        let mut summary = JsonLine::new();
        summary.str("t", "summary");
        summary.str("oldRoot", &encode_name(&self.old_root));
        summary.str("newRoot", &encode_name(&self.new_root));
        summary.i64("diskDelta", self.disk_delta);
        summary.i64("apparentDelta", self.apparent_delta);
        summary.i64("entryDelta", self.entry_delta);
        summary.u64("added", self.counts.added);
        summary.u64("removed", self.counts.removed);
        summary.u64("grown", self.counts.grown);
        summary.u64("shrunk", self.counts.shrunk);
        summary.u64("touched", self.counts.touched);
        summary.u64("typeChanged", self.counts.type_changed);
        summary.u64("dirsAdded", self.counts.dirs_added);
        summary.u64("dirsRemoved", self.counts.dirs_removed);
        out.push_str(&summary.finish());
        for dir in &self.top_dirs {
            let mut line = JsonLine::new();
            line.str("t", "dir");
            line.str("path", &encode_name(&dir.path));
            line.str("change", dir.change.as_str());
            line.i64("diskDelta", dir.disk_delta);
            line.i64("apparentDelta", dir.apparent_delta);
            line.i64("entryDelta", dir.entry_delta);
            out.push_str(&line.finish());
        }
        for entry in &self.top_entries {
            let mut line = JsonLine::new();
            line.str("t", "entry");
            line.str("path", &encode_name(&entry.path));
            line.str("change", entry.change.as_str());
            line.i64("diskDelta", entry.disk_delta);
            line.i64("apparentDelta", entry.apparent_delta);
            out.push_str(&line.finish());
        }
        out
    }
}

/// Diff two ordered dumps (see the module docs for the algorithm and its
/// memory bounds). Both readers are consumed to their end so completeness
/// can be verified.
pub fn diff_dumps<R1: Read, R2: Read>(
    old: DumpReader<R1>,
    new: DumpReader<R2>,
    options: &DiffOptions,
) -> Result<DiffReport, DiffError> {
    for (side, ordered) in [
        (Side::Old, old.header().ordered),
        (Side::New, new.header().ordered),
    ] {
        if !ordered {
            return Err(DiffError::Unordered { side });
        }
    }
    let mut differ = Differ {
        report: DiffReport {
            old_root: old.header().root.clone(),
            new_root: new.header().root.clone(),
            disk_delta: 0,
            apparent_delta: 0,
            entry_delta: 0,
            counts: DiffCounts::default(),
            top_dirs: Vec::new(),
            top_entries: Vec::new(),
        },
        dirs: TopN::new(options.top),
        entries: TopN::new(options.top),
        frames: Vec::new(),
        saw_root: false,
    };
    let mut old = Cursor::new(old, Side::Old)?;
    let mut new = Cursor::new(new, Side::New)?;

    loop {
        match (&old.current, &new.current) {
            (None, None) => break,
            (Some(_), None) => {
                let block = old.take();
                differ.one_sided(block, Side::Old);
                old.advance()?;
            }
            (None, Some(_)) => {
                let block = new.take();
                differ.one_sided(block, Side::New);
                new.advance()?;
            }
            (Some(o), Some(n)) => match o.rel.cmp(&n.rel) {
                Ordering::Less => {
                    let block = old.take();
                    differ.one_sided(block, Side::Old);
                    old.advance()?;
                }
                Ordering::Greater => {
                    let block = new.take();
                    differ.one_sided(block, Side::New);
                    new.advance()?;
                }
                Ordering::Equal => {
                    let (o, n) = (old.take(), new.take());
                    differ.matched(o, n)?;
                    old.advance()?;
                    new.advance()?;
                }
            },
        }
    }
    differ.pop_frames_to(0);

    for (side, complete) in [
        (Side::Old, old.reader.is_complete()),
        (Side::New, new.reader.is_complete()),
    ] {
        if !complete {
            return Err(DiffError::Incomplete { side });
        }
    }
    if !differ.saw_root {
        return Err(DiffError::Invalid {
            side: Side::Old,
            msg: "no root directory block on either side".into(),
        });
    }

    let mut report = differ.report;
    report.top_dirs = differ.dirs.into_vec();
    report.top_dirs.sort_by(|a, b| {
        b.disk_delta
            .cmp(&a.disk_delta)
            .then_with(|| a.path.cmp(&b.path))
    });
    report.top_entries = differ.entries.into_vec();
    report.top_entries.sort_by(|a, b| {
        b.disk_delta
            .cmp(&a.disk_delta)
            .then_with(|| a.path.cmp(&b.path))
    });
    debug!(
        disk_delta = report.disk_delta,
        added = report.counts.added,
        removed = report.counts.removed,
        changed = report.counts.changed(),
        "diff complete"
    );
    Ok(report)
}

/// One dump-side cursor: the current block with its root-relative path
/// components, plus ordering verification (an "ordered" dump that lies
/// would silently corrupt the merge-join, so it is checked as we go).
struct Cursor<R: Read> {
    reader: DumpReader<R>,
    side: Side,
    root: Vec<Vec<u8>>,
    current: Option<RelBlock>,
    last_rel: Option<Vec<Vec<u8>>>,
}

/// A directory block plus its root-relative path components (the §4
/// component-wise comparison key).
struct RelBlock {
    rel: Vec<Vec<u8>>,
    block: DirBlock,
}

fn components(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&b| b == b'/').map(<[u8]>::to_vec).collect()
}

impl<R: Read> Cursor<R> {
    fn new(mut reader: DumpReader<R>, side: Side) -> Result<Self, DiffError> {
        let root = components(&reader.header().root);
        let current = Self::read_next(&mut reader, side, &root, None)?;
        if let Some(first) = &current
            && !first.rel.is_empty()
        {
            return Err(DiffError::Invalid {
                side,
                msg: "first directory block is not the root".into(),
            });
        }
        let last_rel = current.as_ref().map(|block| block.rel.clone());
        Ok(Self {
            reader,
            side,
            root,
            current,
            last_rel,
        })
    }

    fn take(&mut self) -> RelBlock {
        self.current.take().expect("caller checked current")
    }

    fn advance(&mut self) -> Result<(), DiffError> {
        let prev = self.last_rel.take();
        self.current = Self::read_next(&mut self.reader, self.side, &self.root, prev)?;
        self.last_rel = self.current.as_ref().map(|block| block.rel.clone());
        Ok(())
    }

    fn read_next(
        reader: &mut DumpReader<R>,
        side: Side,
        root: &[Vec<u8>],
        last: Option<Vec<Vec<u8>>>,
    ) -> Result<Option<RelBlock>, DiffError> {
        let Some(block) = reader
            .next_block()
            .map_err(|source| DiffError::Read { side, source })?
        else {
            return Ok(None);
        };
        let comps = components(&block.path);
        if comps.len() < root.len() || comps[..root.len()] != *root {
            return Err(DiffError::Invalid {
                side,
                msg: format!(
                    "directory block {:?} is not under the header root",
                    String::from_utf8_lossy(&block.path)
                ),
            });
        }
        let rel = comps[root.len()..].to_vec();
        if let Some(last) = last
            && last >= rel
        {
            return Err(DiffError::Invalid {
                side,
                msg: format!(
                    "directory blocks out of order near {:?} — dump claims ordered but is not",
                    String::from_utf8_lossy(&block.path)
                ),
            });
        }
        Ok(Some(RelBlock { rel, block }))
    }
}

/// An entry seen on one side of a matched block pair, waiting to learn
/// whether the other side has a *directory* of the same name (file ↔ dir
/// type change) before being classified as plain added/removed.
struct PendingEntry {
    apparent: u64,
    disk: u64,
    /// Full path (block path + name).
    path: Vec<u8>,
}

/// Pending one-sided entries of one matched directory, kept while the
/// merge is inside that directory's subtree.
struct Frame {
    rel: Vec<Vec<u8>>,
    pending_old: FxHashMap<Vec<u8>, PendingEntry>,
    pending_new: FxHashMap<Vec<u8>, PendingEntry>,
}

struct Differ {
    report: DiffReport,
    dirs: TopN<DirDelta>,
    entries: TopN<EntryDelta>,
    frames: Vec<Frame>,
    saw_root: bool,
}

impl Differ {
    /// Flush and pop frames until at most `keep` remain.
    fn pop_frames_to(&mut self, keep: usize) {
        while self.frames.len() > keep {
            let frame = self.frames.pop().expect("len checked");
            for (_, pending) in frame.pending_old {
                self.classify_pending(pending, Side::Old);
            }
            for (_, pending) in frame.pending_new {
                self.classify_pending(pending, Side::New);
            }
        }
    }

    /// Pop frames that are not ancestors of `rel` (the merge left their
    /// subtrees; their pending entries can no longer be type changes).
    fn leave_to(&mut self, rel: &[Vec<u8>]) {
        let keep = self
            .frames
            .iter()
            .take_while(|f| f.rel.len() <= rel.len() && rel[..f.rel.len()] == *f.rel)
            .count();
        self.pop_frames_to(keep);
    }

    fn classify_pending(&mut self, pending: PendingEntry, side: Side) {
        let (change, sign) = match side {
            Side::Old => (Change::Removed, -1i64),
            Side::New => (Change::Added, 1i64),
        };
        match side {
            Side::Old => self.report.counts.removed += 1,
            Side::New => self.report.counts.added += 1,
        }
        self.push_entry(EntryDelta {
            path: pending.path,
            change,
            disk_delta: sign * pending.disk as i64,
            apparent_delta: sign * pending.apparent as i64,
        });
    }

    fn push_entry(&mut self, delta: EntryDelta) {
        self.entries.push(delta.disk_delta.unsigned_abs(), delta);
    }

    fn push_dir(&mut self, delta: DirDelta) {
        self.dirs.push(delta.disk_delta.unsigned_abs(), delta);
    }

    fn totals_of(block: &DirBlock) -> Totals {
        // Ordered dumps carry totals on every d line (§6.2); a missing set
        // (foreign writer) degrades to the block's own inode, which keeps
        // the diff usable instead of failing the whole run.
        block.totals.unwrap_or(Totals {
            ta: block.apparent,
            td: block.disk,
            tn: 1,
            te: 0,
        })
    }

    /// A directory block present on one side only: an added or removed
    /// subtree. Its entries classify immediately (their whole directory is
    /// one-sided — no type change possible for them), and the block itself
    /// may resolve a pending opposite-side entry into a file ↔ directory
    /// type change.
    fn one_sided(&mut self, block: RelBlock, side: Side) {
        self.leave_to(&block.rel);
        let RelBlock { rel, block } = block;
        let totals = Self::totals_of(&block);
        let sign = match side {
            Side::Old => -1i64,
            Side::New => 1i64,
        };

        // file ↔ dir: the parent's opposite-side pending map holds the
        // file half, if any.
        if let (Some(name), Some(frame)) = (rel.last(), self.frames.last_mut())
            && frame.rel.len() == rel.len() - 1
        {
            let opposite = match side {
                Side::Old => &mut frame.pending_new,
                Side::New => &mut frame.pending_old,
            };
            if let Some(pending) = opposite.remove(name.as_slice()) {
                self.report.counts.type_changed += 1;
                self.push_entry(EntryDelta {
                    path: block.path.clone(),
                    change: Change::TypeChanged,
                    disk_delta: sign * totals.td as i64 - sign * pending.disk as i64,
                    apparent_delta: sign * totals.ta as i64 - sign * pending.apparent as i64,
                });
            }
        }

        match side {
            Side::Old => self.report.counts.dirs_removed += 1,
            Side::New => self.report.counts.dirs_added += 1,
        }
        self.push_dir(DirDelta {
            path: block.path.clone(),
            change: match side {
                Side::Old => DirChange::Removed,
                Side::New => DirChange::Added,
            },
            disk_delta: sign * totals.td as i64,
            apparent_delta: sign * totals.ta as i64,
            entry_delta: sign * totals.tn as i64,
        });

        for entry in &block.entries {
            match side {
                Side::Old => self.report.counts.removed += 1,
                Side::New => self.report.counts.added += 1,
            }
            self.push_entry(EntryDelta {
                path: join_path(&block.path, &entry.name),
                change: match side {
                    Side::Old => Change::Removed,
                    Side::New => Change::Added,
                },
                disk_delta: sign * entry.disk as i64,
                apparent_delta: sign * entry.apparent as i64,
            });
        }
    }

    /// A directory present on both sides: subtree delta from the totals,
    /// then a merge-join of the two sorted entry lists.
    fn matched(&mut self, old: RelBlock, new: RelBlock) -> Result<(), DiffError> {
        self.leave_to(&new.rel);
        let (ot, nt) = (Self::totals_of(&old.block), Self::totals_of(&new.block));
        if old.rel.is_empty() {
            self.saw_root = true;
            self.report.disk_delta = nt.td as i64 - ot.td as i64;
            self.report.apparent_delta = nt.ta as i64 - ot.ta as i64;
            self.report.entry_delta = nt.tn as i64 - ot.tn as i64;
        }
        let (dtd, dta, dtn) = (
            nt.td as i64 - ot.td as i64,
            nt.ta as i64 - ot.ta as i64,
            nt.tn as i64 - ot.tn as i64,
        );
        if dtd != 0 || dta != 0 || dtn != 0 {
            self.push_dir(DirDelta {
                path: new.block.path.clone(),
                change: DirChange::Changed,
                disk_delta: dtd,
                apparent_delta: dta,
                entry_delta: dtn,
            });
        }

        let mut frame = Frame {
            rel: new.rel,
            pending_old: FxHashMap::default(),
            pending_new: FxHashMap::default(),
        };
        let path = &new.block.path;
        let (mut oi, mut ni) = (0usize, 0usize);
        let (oe, ne) = (&old.block.entries, &new.block.entries);
        verify_entry_order(oe, Side::Old, &old.block.path)?;
        verify_entry_order(ne, Side::New, path)?;
        while oi < oe.len() || ni < ne.len() {
            let advance = match (oe.get(oi), ne.get(ni)) {
                (Some(o), Some(n)) => o.name.cmp(&n.name),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => unreachable!("loop condition"),
            };
            match advance {
                Ordering::Less => {
                    let o = &oe[oi];
                    frame.pending_old.insert(
                        o.name.clone(),
                        PendingEntry {
                            apparent: o.apparent,
                            disk: o.disk,
                            path: join_path(&old.block.path, &o.name),
                        },
                    );
                    oi += 1;
                }
                Ordering::Greater => {
                    let n = &ne[ni];
                    frame.pending_new.insert(
                        n.name.clone(),
                        PendingEntry {
                            apparent: n.apparent,
                            disk: n.disk,
                            path: join_path(path, &n.name),
                        },
                    );
                    ni += 1;
                }
                Ordering::Equal => {
                    self.classify_pair(&oe[oi], &ne[ni], path);
                    oi += 1;
                    ni += 1;
                }
            }
        }
        if !frame.pending_old.is_empty() || !frame.pending_new.is_empty() {
            self.frames.push(frame);
        }
        Ok(())
    }

    /// Same name on both sides of a matched block.
    fn classify_pair(&mut self, old: &Entry, new: &Entry, dir_path: &[u8]) {
        let dd = new.disk as i64 - old.disk as i64;
        let da = new.apparent as i64 - old.apparent as i64;
        let change = if old.kind != new.kind {
            Change::TypeChanged
        } else if dd != 0 || da != 0 {
            if (dd, da) > (0, 0) {
                Change::Grown
            } else {
                Change::Shrunk
            }
        } else if old.mtime != new.mtime {
            Change::Touched
        } else {
            return; // identical
        };
        match change {
            Change::Grown => self.report.counts.grown += 1,
            Change::Shrunk => self.report.counts.shrunk += 1,
            Change::Touched => self.report.counts.touched += 1,
            Change::TypeChanged => self.report.counts.type_changed += 1,
            Change::Added | Change::Removed => unreachable!("pair classification"),
        }
        self.push_entry(EntryDelta {
            path: join_path(dir_path, &new.name),
            change,
            disk_delta: dd,
            apparent_delta: da,
        });
    }
}

fn join_path(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(dir.len() + 1 + name.len());
    path.extend_from_slice(dir);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

fn verify_entry_order(entries: &[Entry], side: Side, path: &[u8]) -> Result<(), DiffError> {
    if let Some(pair) = entries.windows(2).find(|w| w[0].name >= w[1].name) {
        return Err(DiffError::Invalid {
            side,
            msg: format!(
                "entries out of order in {:?} (near {:?}) — dump claims ordered but is not",
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(&pair[1].name),
            ),
        });
    }
    Ok(())
}

// ---- bounded top-N by |disk delta| ----

/// Min-heap of the `cap` largest items by key, ties broken by insertion
/// order (earlier wins) for determinism.
struct TopN<T> {
    cap: usize,
    seq: u64,
    heap: BinaryHeap<std::cmp::Reverse<Ranked<T>>>,
}

struct Ranked<T> {
    key: u64,
    seq: u64,
    item: T,
}

impl<T> PartialEq for Ranked<T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.seq == other.seq
    }
}
impl<T> Eq for Ranked<T> {}
impl<T> PartialOrd for Ranked<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Ranked<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Later insertions rank lower on equal keys, so the min-heap
        // evicts them first and the earliest seen survive.
        self.key
            .cmp(&other.key)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl<T> TopN<T> {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            seq: 0,
            heap: BinaryHeap::new(),
        }
    }

    fn push(&mut self, key: u64, item: T) {
        if self.cap == 0 {
            return;
        }
        self.seq += 1;
        let ranked = std::cmp::Reverse(Ranked {
            key,
            seq: self.seq,
            item,
        });
        if self.heap.len() < self.cap {
            self.heap.push(ranked);
        } else if self.heap.peek().is_some_and(|min| ranked.0 > min.0) {
            self.heap.pop();
            self.heap.push(ranked);
        }
    }

    fn into_vec(self) -> Vec<T> {
        self.heap.into_iter().map(|r| r.0.item).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compress JSON-lines text as a plain zstd stream and open a reader.
    fn dump(lines: &str) -> DumpReader<std::io::Cursor<Vec<u8>>> {
        let bytes = zstd::stream::encode_all(lines.as_bytes(), 0).expect("zstd");
        DumpReader::new(std::io::Cursor::new(bytes)).expect("valid dump")
    }

    fn header(root: &str, ordered: bool) -> String {
        format!(
            r#"{{"t":"h","format":"camembert-dump","v":1,"minor":0,"ts":0,"root":"{root}","dev":"1","sem":"blocks","ext":false,"ordered":{ordered},"allino":false}}"#
        )
    }

    fn end_line() -> &'static str {
        r#"{"t":"e","entries":0,"dirs":0,"errors":0,"ta":0,"td":0,"elapsed":0.1}"#
    }

    fn run(old: &str, new: &str, top: usize) -> Result<DiffReport, DiffError> {
        diff_dumps(dump(old), dump(new), &DiffOptions { top })
    }

    /// d line with totals; own inode 4096/4096, mtime 0.
    fn d(path: &str, ta: u64, td: u64, tn: u64) -> String {
        format!(
            r#"{{"t":"d","path":"{path}","a":4096,"d":4096,"m":0,"nf":0,"nd":0,"ta":{ta},"td":{td},"tn":{tn},"te":0}}"#
        )
    }

    fn f(name: &str, a: u64, dsk: u64, m: i64) -> String {
        format!(r#"{{"n":"{name}","a":{a},"d":{dsk},"m":{m}}}"#)
    }

    #[test]
    fn identical_dumps_diff_to_zero() {
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 4196, 4608, 2),
            f("x", 100, 512, 5),
            end_line()
        );
        let report = run(&text, &text, 20).expect("diff");
        assert_eq!(report.disk_delta, 0);
        assert_eq!(report.apparent_delta, 0);
        assert_eq!(report.entry_delta, 0);
        assert_eq!(report.counts, DiffCounts::default());
        assert!(report.top_dirs.is_empty());
        assert!(report.top_entries.is_empty());
    }

    #[test]
    fn classifications_cover_all_change_kinds() {
        let old = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 10000, 12000, 6),
            f("gone", 100, 512, 1),
            f("grown", 100, 512, 1),
            f("kindflip", 10, 0, 1),
            f("shrunk", 3000, 3072, 1),
            f("touched", 50, 512, 100),
            end_line()
        );
        let new = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 11000, 13000, 6),
            f("added", 200, 1024, 2),
            f("grown", 5000, 5120, 2),
            r#"{"n":"kindflip","a":10,"d":0,"m":1,"k":"l"}"#,
            f("shrunk", 100, 512, 2),
            f("touched", 50, 512, 999),
            end_line()
        );
        let report = run(&old, &new, 20).expect("diff");
        assert_eq!(report.disk_delta, 1000);
        assert_eq!(report.apparent_delta, 1000);
        assert_eq!(
            report.counts,
            DiffCounts {
                added: 1,
                removed: 1,
                grown: 1,
                shrunk: 1,
                touched: 1,
                type_changed: 1,
                dirs_added: 0,
                dirs_removed: 0,
            }
        );
        let by_path: FxHashMap<&[u8], Change> = report
            .top_entries
            .iter()
            .map(|e| (e.path.as_slice(), e.change))
            .collect();
        assert_eq!(by_path[b"/r/added".as_slice()], Change::Added);
        assert_eq!(by_path[b"/r/gone".as_slice()], Change::Removed);
        assert_eq!(by_path[b"/r/grown".as_slice()], Change::Grown);
        assert_eq!(by_path[b"/r/shrunk".as_slice()], Change::Shrunk);
        assert_eq!(by_path[b"/r/touched".as_slice()], Change::Touched);
        assert_eq!(by_path[b"/r/kindflip".as_slice()], Change::TypeChanged);

        // Entry ordering: biggest growth first, shrinkage negative last.
        let grown = report
            .top_entries
            .iter()
            .position(|e| e.path == b"/r/grown")
            .unwrap();
        let shrunk = report
            .top_entries
            .iter()
            .position(|e| e.path == b"/r/shrunk")
            .unwrap();
        assert!(grown < shrunk);
        assert_eq!(report.top_entries[grown].disk_delta, 5120 - 512);
        assert_eq!(report.top_entries[shrunk].disk_delta, 512 - 3072);

        // The root dir delta is reported as changed.
        assert_eq!(report.top_dirs.len(), 1);
        assert_eq!(report.top_dirs[0].change, DirChange::Changed);
        assert_eq!(report.top_dirs[0].disk_delta, 1000);
    }

    #[test]
    fn added_and_removed_subtrees_count_dirs_and_entries() {
        let old = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 20000, 20000, 5),
            d("/r/olddir", 8000, 8000, 3),
            f("a", 100, 512, 1),
            end_line()
        );
        let new = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 30000, 30000, 6),
            d("/r/newdir", 9000, 9000, 3),
            f("x", 200, 1024, 1),
            f("y", 300, 2048, 1),
            end_line()
        );
        let report = run(&old, &new, 20).expect("diff");
        assert_eq!(report.counts.dirs_added, 1);
        assert_eq!(report.counts.dirs_removed, 1);
        assert_eq!(report.counts.added, 2, "entries of the added subtree");
        assert_eq!(report.counts.removed, 1);
        let olddir = report
            .top_dirs
            .iter()
            .find(|d| d.path == b"/r/olddir")
            .expect("removed dir listed");
        assert_eq!(olddir.change, DirChange::Removed);
        assert_eq!(olddir.disk_delta, -8000);
        assert_eq!(olddir.entry_delta, -3);
        let newdir = report
            .top_dirs
            .iter()
            .find(|d| d.path == b"/r/newdir")
            .expect("added dir listed");
        assert_eq!(newdir.disk_delta, 9000);
    }

    #[test]
    fn file_replaced_by_directory_is_a_type_change() {
        let old = format!(
            "{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 5000, 5000, 2),
            f("thing", 100, 512, 1),
            end_line()
        );
        let new = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 9000, 9000, 3),
            d("/r/thing", 5000, 5000, 2),
            f("inner", 100, 512, 1),
            end_line()
        );
        let report = run(&old, &new, 20).expect("diff");
        assert_eq!(report.counts.type_changed, 1, "file -> dir");
        assert_eq!(report.counts.removed, 0, "not double-counted as removed");
        let tc = report
            .top_entries
            .iter()
            .find(|e| e.change == Change::TypeChanged)
            .expect("type change listed");
        assert_eq!(tc.path, b"/r/thing");
        assert_eq!(tc.disk_delta, 5000 - 512, "subtree td minus old file disk");

        // And the reverse: dir -> file.
        let report = run(&new, &old, 20).expect("reverse diff");
        assert_eq!(report.counts.type_changed, 1, "dir -> file");
        assert_eq!(report.counts.added, 0);
    }

    /// `foo.bar` vs `foo/x`: whole-string byte order would visit
    /// `/r/foo.bar` before `/r/foo/x` ('.' 0x2E < '/' 0x2F), component
    /// order the other way around. The merge must follow component order
    /// or blocks would misalign and report phantom changes.
    #[test]
    fn component_order_edge_case_foo_dot_bar() {
        let text = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 30000, 30000, 5),
            d("/r/foo", 10000, 10000, 2),
            f("x", 100, 512, 1),
            d("/r/foo.bar", 8000, 8000, 2),
            f("y", 100, 512, 1),
            end_line()
        );
        let report = run(&text, &text, 20).expect("self-diff with tricky order");
        assert_eq!(report.counts, DiffCounts::default(), "no phantom changes");
        assert_eq!(report.disk_delta, 0);
    }

    #[test]
    fn different_roots_diff_by_relative_structure() {
        let old = format!(
            "{}\n{}\n{}\n{}\n",
            header("/old", true),
            d("/old", 5000, 5000, 2),
            f("f", 100, 512, 1),
            end_line()
        );
        let new = format!(
            "{}\n{}\n{}\n{}\n",
            header("/new", true),
            d("/new", 5000, 6024, 2),
            f("f", 100, 1536, 1),
            end_line()
        );
        let report = run(&old, &new, 20).expect("diff across roots");
        assert_eq!(report.old_root, b"/old");
        assert_eq!(report.new_root, b"/new");
        assert_eq!(report.disk_delta, 1024);
        assert_eq!(report.counts.grown, 1);
    }

    #[test]
    fn unordered_dump_is_refused_with_guidance() {
        let ordered = format!(
            "{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 0, 0, 1),
            end_line()
        );
        let unordered = format!(
            "{}\n{}\n{}\n",
            header("/r", false),
            d("/r", 0, 0, 1),
            end_line()
        );
        let err = run(&unordered, &ordered, 20).expect_err("must refuse");
        assert!(matches!(err, DiffError::Unordered { side: Side::Old }));
        assert!(err.to_string().contains("camembert dump sort"));
        let err = run(&ordered, &unordered, 20).expect_err("must refuse");
        assert!(matches!(err, DiffError::Unordered { side: Side::New }));
    }

    #[test]
    fn incomplete_dump_is_refused() {
        let complete = format!(
            "{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 0, 0, 1),
            end_line()
        );
        let torn = format!("{}\n{}\n", header("/r", true), d("/r", 0, 0, 1));
        let err = run(&torn, &complete, 20).expect_err("must refuse");
        assert!(matches!(err, DiffError::Incomplete { side: Side::Old }));
    }

    #[test]
    fn lying_ordered_flag_is_detected() {
        let good = format!(
            "{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 0, 0, 3),
            d("/r/b", 0, 0, 1),
            end_line()
        );
        let bad_blocks = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 0, 0, 3),
            d("/r/b", 0, 0, 1),
            d("/r/a", 0, 0, 1),
            end_line()
        );
        let err = run(&bad_blocks, &good, 20).expect_err("must refuse");
        assert!(matches!(
            err,
            DiffError::Invalid {
                side: Side::Old,
                ..
            }
        ));

        let bad_entries = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 0, 0, 3),
            f("zz", 1, 512, 0),
            f("aa", 1, 512, 0),
            end_line()
        );
        let err = run(&bad_entries, &bad_entries, 20).expect_err("must refuse");
        assert!(matches!(err, DiffError::Invalid { .. }));
    }

    #[test]
    fn top_n_is_bounded_and_sorted_by_signed_delta() {
        let mut old_lines = vec![header("/r", true), d("/r", 0, 100_000, 12)];
        let mut new_lines = vec![header("/r", true), d("/r", 0, 200_000, 12)];
        for i in 0..10 {
            // Old: 512 bytes each; new: growing amounts, file7 shrinks.
            old_lines.push(f(&format!("file{i}"), 0, 512, 0));
            let new_disk = if i == 7 { 0 } else { 1024 * (i + 1) };
            new_lines.push(f(&format!("file{i}"), 0, new_disk, 0));
        }
        old_lines.push(end_line().to_owned());
        new_lines.push(end_line().to_owned());
        let (old_text, new_text) = (old_lines.join("\n") + "\n", new_lines.join("\n") + "\n");
        let report = run(&old_text, &new_text, 3).expect("diff");
        assert_eq!(report.top_entries.len(), 3, "bounded at --top");
        let paths: Vec<&[u8]> = report
            .top_entries
            .iter()
            .map(|e| e.path.as_slice())
            .collect();
        // Largest |delta|: file9 (+9728), file8 (+8704), file6 (+6656).
        assert_eq!(paths, [b"/r/file9" as &[u8], b"/r/file8", b"/r/file6"]);
        assert!(
            report.top_entries[0].disk_delta > report.top_entries[2].disk_delta,
            "signed descending"
        );
    }

    #[test]
    fn json_lines_schema_is_stable() {
        let old = format!(
            "{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 5000, 5000, 2),
            f("f%FF", 100, 512, 1),
            end_line()
        );
        let new = format!(
            "{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 5000, 6024, 2),
            f("f%FF", 100, 1536, 1),
            end_line()
        );
        let report = run(&old, &new, 20).expect("diff");
        let json = report.to_json_lines();
        let lines: Vec<serde_json::Value> = json
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JSON line"))
            .collect();
        assert_eq!(lines[0]["t"], "summary");
        assert_eq!(lines[0]["diskDelta"], 1024);
        assert_eq!(lines[0]["grown"], 1);
        assert_eq!(lines[0]["oldRoot"], "/r");
        let entry = lines
            .iter()
            .find(|l| l["t"] == "entry")
            .expect("entry line");
        assert_eq!(entry["change"], "grown");
        assert_eq!(
            entry["path"], "/r/f%FF",
            "non-UTF-8 path stays percent-encoded in JSON"
        );
        let dir = lines.iter().find(|l| l["t"] == "dir").expect("dir line");
        assert_eq!(dir["change"], "changed");
        assert_eq!(dir["diskDelta"], 1024);
    }

    #[test]
    fn hardlink_canonical_totals_come_from_d_lines() {
        // Old counts a 1024-byte inode under /r/a (canonical); new has the
        // same inode canonical under /r/a still, but the extra link moved:
        // per-dir tds are authoritative — the diff just subtracts them.
        let old = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 0, 9216, 4),
            d("/r/a", 0, 5120, 2),
            r#"{"n":"one","a":1000,"d":1024,"m":0,"i":"42","l":2}"#,
            end_line()
        );
        let new = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            header("/r", true),
            d("/r", 0, 9216, 5),
            d("/r/a", 0, 5120, 3),
            r#"{"n":"one","a":1000,"d":1024,"m":0,"i":"42","l":3}"#,
            r#"{"n":"two","a":1000,"d":1024,"m":0,"i":"42","l":3}"#,
            end_line()
        );
        let report = run(&old, &new, 20).expect("diff");
        assert_eq!(report.disk_delta, 0, "extra link adds no disk (d totals)");
        assert_eq!(report.entry_delta, 1, "but one more inode-visible entry");
        assert_eq!(report.counts.added, 1, "the new link is an added entry");
    }
}
