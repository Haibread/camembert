//! Flat view + pattern aggregation engine (decisions
//! `docs/design/flat-view-decisions.md`).
//!
//! Two read-only reports over the scan tree:
//!
//! - **flat top-N files**: the largest regular files of the whole scan,
//!   out of the hierarchy;
//! - **pattern breakdown**: a fixed set of named groups (basename globs —
//!   `node_modules/`, `*.log`, …) with their totals, plus an implicit
//!   *rest*.
//!
//! # D1 — the groups are a disjoint partition (first match wins, outermost
//! wins)
//!
//! Every counted byte lands in **at most one** group, plus *rest*; the
//! columns sum to the root subtree aggregate. Precedence, exactly as D1:
//!
//! 1. **Directory coverage is outermost.** A directory whose name matches a
//!    dir-pattern claims its *whole subtree* for that group; a nested match
//!    (a `node_modules` inside a `node_modules`, a `*.log` file inside a
//!    claimed directory) does **not** re-claim — it stays in the outer
//!    group.
//! 2. **Among patterns matching the same name, list order wins** (presets
//!    first, then `camembert.toml` `[patterns]` in file order; a user
//!    pattern reusing a preset's label replaces it *in place*, keeping the
//!    preset's position — see [`PatternSet::push`]).
//!
//! Because coverage is a single `Option<GroupId>` per directory (not a
//! set), there is **no ≤64-pattern cap** the overlapping design would have
//! needed, and the per-name memo is `O(unique names)` regardless of the
//! pattern count (see the memo notes below).
//!
//! # D2 — dual engine: live provisional accumulator + authoritative fold
//!
//! - During the scan the **owner** feeds an [`Accumulator`] at node-insert
//!   time (O(1) amortised per node: a memo lookup + a counter add, plus a
//!   heap compare that almost always fails fast). It publishes a
//!   provisional [`FlatSummary`] (`provisional = true`) alongside the view
//!   snapshots, on the same arc-swap cadence. Hardlink attribution is
//!   **first-seen** here (extras, as the registry flags them, contribute 0
//!   and never rank in top-N) — the same provisional caveat the TUI already
//!   shows for live totals.
//! - At scan end, after canonical hardlink re-attribution
//!   ([`crate::scan::ScanOutcome::finalize_hardlinks`]), the exact
//!   [`fold`] runs one streamed pass over the frozen arena and is the
//!   **authoritative** summary (`provisional = false`). It is also what
//!   recomputes after each deletion.
//!
//! The two paths implement the *same* partition logic, so on an identical
//! tree state they agree exactly. The only legitimate divergence is
//! hardlink attribution: the accumulator counts each inode's first-seen
//! link, the post-finalize fold counts its canonical (smallest-path) link.
//! When those two links live in different groups the bytes move — the fold
//! is authoritative. The integration test in
//! `camembert-core/tests/flat_agreement.rs` asserts both (accumulator ==
//! pre-finalize fold exactly; post-finalize fold reflects canonical).
//!
//! # ncdu import: fold-only
//!
//! The ncdu importer ([`crate::ncdu`]) builds an arena but runs **no**
//! accumulator: import is non-interactive (there is no browse-during-scan
//! to badge provisional), so the fold over the finished tree is the single
//! source of truth. Nothing to accumulate incrementally there.
//!
//! # Memo representation and its honest memory bound
//!
//! Glob verdicts are memoized per **interned name id** (names dedup in the
//! interner, so a repeated `package.json` is globbed once). The verdict of
//! a disjoint partition is a single `Option<GroupId>` per name, so the memo
//! is a dense `Vec` indexed by name id — two of them (a name can be used
//! both as a directory and as a file), each 2 bytes/name:
//! `2 x 2 x unique_names` bytes, e.g. ~40 MB on a 10 M-unique-name tree.
//! Bounded by `unique_names <= node_count` and cache-friendly (a plain
//! index, no hashing).
//!
//! A lazy `HashMap<NameId, Option<GroupId>>` was considered (the dossier's
//! suggestion): it would only win if a large fraction of interned names
//! were never visited, but the fold visits every live node — hence
//! essentially every name — so the map would grow to the same entry count
//! at ~4-5x the per-entry cost (hashbrown control bytes + u32 key vs a
//! 2-byte dense slot). Dense wins here; the map is the wrong tool. The one
//! trade-off — the memo saves nothing on all-unique-name trees
//! (git-object stores, Maildir), where each name is globbed exactly once
//! anyway — costs only the bounded dense allocation, never re-globs, and
//! never grows unbounded. Documented, deliberate.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use tracing::warn;

use crate::size::Size;
use crate::tree::{DirId, Kind, NodeFlags, NodeId, Tree};

/// A group index into a [`PatternSet`] (one group per pattern). `u16` caps
/// the pattern count at [`MAX_GROUPS`], far above any realistic config; the
/// disjoint partition needs no bitmask, so there is no tighter limit.
pub type GroupId = u16;

/// Memo sentinel: this name has not been globbed yet.
const MEMO_UNCOMPUTED: u16 = u16::MAX;
/// Memo sentinel: this name matched no pattern of the relevant kind.
const MEMO_NONE: u16 = u16::MAX - 1;
/// Largest number of patterns a [`PatternSet`] can hold (group ids must be
/// distinguishable from the two memo sentinels).
pub const MAX_GROUPS: usize = (u16::MAX - 2) as usize;

/// Default top-N cap (D4: user-configurable via `flat_cap`, default 1000).
pub const DEFAULT_FLAT_CAP: usize = 1000;

/// Flat-view configuration handed to the scan at start (D2): the compiled
/// patterns and the top-N cap. The frontend builds this from `camembert.toml`
/// (presets + `[patterns]`, `flat_cap`) before launching the scan.
#[derive(Debug, Clone)]
pub struct FlatConfig {
    pub patterns: PatternSet,
    pub cap: usize,
}

impl Default for FlatConfig {
    fn default() -> Self {
        Self {
            patterns: PatternSet::presets(),
            cap: DEFAULT_FLAT_CAP,
        }
    }
}

/// Whether a pattern matches directory names or non-directory names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// Directory pattern, written with a trailing `/` in config
    /// (`node_modules/`); claims the whole matched subtree (D1).
    Dir,
    /// File (non-directory) pattern (`*.log`); matches individual entries.
    File,
}

/// One compiled basename glob with its label and kind.
#[derive(Debug, Clone)]
struct CompiledPattern {
    label: String,
    /// Raw glob bytes, trailing `/` already stripped (`*`/`?` are special;
    /// every other byte, including `{`/`}`/`[`/`]`, is literal — D4).
    glob: Vec<u8>,
    kind: PatternKind,
}

/// An ordered set of compiled patterns (presets then user entries), plus
/// the warnings raised while compiling them (the UI surfaces these).
///
/// Build with [`PatternSet::presets`] then [`PatternSet::push`] each user
/// entry; matching is order-sensitive (first match wins, D1).
#[derive(Debug, Clone, Default)]
pub struct PatternSet {
    patterns: Vec<CompiledPattern>,
    warnings: Vec<String>,
}

/// Parse a config spec into `(glob bytes, kind)`. A trailing `/` marks a
/// directory pattern and is stripped. Returns the reason on rejection.
fn compile_spec(spec: &str) -> Result<(Vec<u8>, PatternKind), String> {
    let (glob, kind) = match spec.strip_suffix('/') {
        Some(stripped) => (stripped.as_bytes().to_vec(), PatternKind::Dir),
        None => (spec.as_bytes().to_vec(), PatternKind::File),
    };
    if glob.is_empty() {
        return Err(format!("empty pattern {spec:?}"));
    }
    if glob.contains(&b'/') {
        return Err(format!(
            "pattern {spec:?} contains '/': only basename globs are supported (full-path globs are wave 3)"
        ));
    }
    Ok((glob, kind))
}

impl PatternSet {
    /// The built-in presets, in order (D4). Labels are the glob text
    /// without the trailing `/`.
    pub fn presets() -> Self {
        const PRESETS: [&str; 8] = [
            "node_modules/",
            ".git/",
            "target/",
            "__pycache__/",
            ".cache/",
            ".venv/",
            "*.log",
            "*.tmp",
        ];
        let mut set = Self::default();
        for spec in PRESETS {
            let label = spec.strip_suffix('/').unwrap_or(spec);
            set.push(label, spec);
        }
        set
    }

    /// Compile and add one pattern. Label shadowing (D1/D4): if `label`
    /// already exists the entry is **replaced in place**, keeping its
    /// original position (a user override of a preset does not jump to the
    /// end — position, and therefore precedence, is preserved). An invalid
    /// glob is skipped: a `tracing` warning is emitted and the reason is
    /// pushed to [`PatternSet::warnings`], never fatal (D4).
    pub fn push(&mut self, label: impl Into<String>, spec: &str) {
        let label = label.into();
        let (glob, kind) = match compile_spec(spec) {
            Ok(compiled) => compiled,
            Err(reason) => {
                warn!(%label, spec, reason, "ignoring invalid flat-view pattern");
                self.warnings.push(reason);
                return;
            }
        };
        if self.patterns.len() >= MAX_GROUPS && !self.patterns.iter().any(|p| p.label == label) {
            let reason = format!("too many patterns (>{MAX_GROUPS}); dropping {label:?}");
            warn!(%label, reason, "flat-view pattern limit reached");
            self.warnings.push(reason);
            return;
        }
        let compiled = CompiledPattern {
            label: label.clone(),
            glob,
            kind,
        };
        match self.patterns.iter_mut().find(|p| p.label == label) {
            Some(existing) => *existing = compiled,
            None => self.patterns.push(compiled),
        }
    }

    /// Warnings raised while compiling patterns (invalid globs, overflow).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Number of groups (patterns).
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Label and kind of a group, for the UI.
    pub fn group(&self, id: GroupId) -> Option<(&str, PatternKind)> {
        self.patterns
            .get(id as usize)
            .map(|p| (p.label.as_str(), p.kind))
    }

    /// First pattern of `kind` whose glob matches `name` (list order = D1
    /// precedence), or `None`. `pub(crate)`: the filter engine
    /// ([`crate::query`]) precomputes these verdicts into an immutable
    /// per-name table for its parallel fold.
    pub(crate) fn first_match(&self, name: &[u8], kind: PatternKind) -> Option<GroupId> {
        self.patterns
            .iter()
            .position(|p| p.kind == kind && glob_match(&p.glob, name))
            .map(|i| i as GroupId)
    }
}

/// Byte-level glob match with only `*` (zero or more bytes) and `?`
/// (exactly one byte) special; every other byte — including `{`, `}`, `[`,
/// `]` — is matched literally (D4). Classic two-pointer with backtracking
/// on the last `*`, so it is linear in practice and never allocates.
///
/// Basenames contain no `/`, so no path-boundary logic is needed: `b*`
/// matches `bb`, `b` does not match `bb`, `*.log` matches `x.log` but not
/// `x.log.txt` nor `foolog`.
///
/// `pub(crate)`: the query language ([`crate::query`]) speaks exactly this
/// dialect for its glob and ancestor terms (one glob dialect per binary).
pub(crate) fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut star_n): (Option<usize>, usize) = (None, 0);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            star_n = n;
            p += 1;
        } else if let Some(sp) = star {
            // Backtrack: let the last `*` swallow one more byte.
            p = sp + 1;
            star_n += 1;
            n = star_n;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Per-name glob memo: two dense `Vec`s indexed by interned name id (one
/// for dir-pattern verdicts, one for file-pattern verdicts). See the module
/// docs for why dense beats a lazy map here.
struct NameMemo {
    dir: Vec<u16>,
    file: Vec<u16>,
}

impl NameMemo {
    fn with_capacity(names: usize) -> Self {
        Self {
            dir: vec![MEMO_UNCOMPUTED; names],
            file: vec![MEMO_UNCOMPUTED; names],
        }
    }

    /// Memoized verdict for `name` (interned id `name_id`) against
    /// `patterns` of `kind`.
    fn lookup(
        &mut self,
        patterns: &PatternSet,
        name: &[u8],
        name_id: u32,
        kind: PatternKind,
    ) -> Option<GroupId> {
        let slots = match kind {
            PatternKind::Dir => &mut self.dir,
            PatternKind::File => &mut self.file,
        };
        let idx = name_id as usize;
        if idx >= slots.len() {
            slots.resize(idx + 1, MEMO_UNCOMPUTED);
        }
        match slots[idx] {
            MEMO_UNCOMPUTED => {
                let verdict = patterns.first_match(name, kind);
                slots[idx] = verdict.unwrap_or(MEMO_NONE);
                verdict
            }
            MEMO_NONE => None,
            group => Some(group),
        }
    }
}

/// Running byte/entry totals for one group (or the rest bucket).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Bucket {
    apparent: u64,
    disk: u64,
    entries: u64,
}

impl Bucket {
    fn add(&mut self, size: Size) {
        self.apparent += size.apparent;
        self.disk += size.real;
        self.entries += 1;
    }
}

/// Totals of one pattern group in a [`FlatSummary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTotal {
    /// The group's label (config key / preset name).
    pub label: String,
    pub kind: PatternKind,
    /// Σ apparent bytes (`st_size`) of the group's entries.
    pub apparent: u64,
    /// Σ disk bytes (`st_blocks * 512`) — the default metric.
    pub disk: u64,
    /// Entry count (inodes; hardlink extras excluded, like `tn`).
    pub entries: u64,
}

/// The "rest" bucket: everything matched by no group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestTotal {
    pub apparent: u64,
    pub disk: u64,
    pub entries: u64,
}

/// One entry of the flat top-N list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopFile {
    pub node: NodeId,
    /// Denormalized basename, **lossy-decoded for display only** (non-UTF-8
    /// bytes become `U+FFFD`). Carried on the entry so the live provisional
    /// view can render names without sharing the scan arena with the UI
    /// thread; the authoritative `node` is what any lookup/jump uses. Plays
    /// no role in ordering.
    pub name: Box<str>,
    /// Disk bytes (`st_blocks * 512`) — the ranking key.
    pub disk: u64,
    /// The file is a link of an `nlink > 1` inode (the `⛓` badge); only the
    /// canonical / counted owner is ever listed (extras contribute 0).
    pub hardlink: bool,
}

/// The result of a flat-view computation: pattern group totals, the rest
/// bucket, and the top-N files, tagged with provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatSummary {
    /// One entry per pattern, in [`PatternSet`] order (empty groups
    /// included, so indices are stable for the UI).
    pub groups: Vec<GroupTotal>,
    /// Everything in no group.
    pub rest: RestTotal,
    /// Top-N files, ordered by disk bytes descending then [`NodeId`]
    /// ascending (deterministic; stable across recomputes).
    pub top_files: Vec<TopFile>,
    /// `true` when more files than the cap were eligible: the list is a
    /// prefix, not the whole scan.
    pub truncated: bool,
    /// `true` for a live accumulator snapshot (first-seen hardlinks),
    /// `false` for the authoritative [`fold`].
    pub provisional: bool,
    /// The deletion epoch this summary was computed against (the UI bumps
    /// it on each successful deletion and recomputes on a render-time
    /// mismatch; provisional snapshots carry the scan-time epoch, `0`).
    pub epoch: u64,
}

/// A file ranked for the top-N min-heap. `Ord` is the **keep-priority**:
/// larger disk ranks higher; on a tie the smaller [`NodeId`] ranks higher
/// (deterministic tiebreak — D2 / attack finding 5). `name` is denormalized
/// for display and plays **no** role in ordering. The heap holds
/// `Reverse<RankedFile>` so its root is the most-evictable entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RankedFile {
    disk: u64,
    node: NodeId,
    hardlink: bool,
    name: Box<str>,
}

impl Ord for RankedFile {
    fn cmp(&self, other: &Self) -> Ordering {
        self.disk
            .cmp(&other.disk)
            // Smaller node id = higher keep-priority (kept over a tie).
            .then_with(|| other.node.index().cmp(&self.node.index()))
            // `node` is unique per file, so this last key is never reached;
            // it only makes `Ord` total and consistent with `Eq`.
            .then_with(|| self.hardlink.cmp(&other.hardlink))
    }
}

impl PartialOrd for RankedFile {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Lossy-decode a basename for display on a [`TopFile`].
fn lossy_basename(name: &[u8]) -> Box<str> {
    String::from_utf8_lossy(name).into_owned().into_boxed_str()
}

/// Whether `(disk, node)` outranks `other` in keep-priority (disk desc,
/// then node id asc) — the heap-eviction decision, computed without
/// materializing a [`RankedFile`] (so the basename is cloned only on an
/// actual insert).
fn outranks(disk: u64, node: NodeId, other: &RankedFile) -> bool {
    disk > other.disk || (disk == other.disk && node.index() < other.node.index())
}

/// Bounded top-N min-heap plus the count of eligible files (for the
/// `truncated` flag). Shared by [`fold`], [`Accumulator`] and the filter
/// engine's fold ([`crate::query`] — hence the `pub(crate)` surface; the
/// keep-priority and tiebreak stay identical everywhere).
pub(crate) struct TopHeap {
    heap: BinaryHeap<Reverse<RankedFile>>,
    cap: usize,
    considered: u64,
}

impl TopHeap {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            heap: BinaryHeap::new(),
            cap,
            considered: 0,
        }
    }

    /// Offer a file to the heap. The basename is cloned **only** when the
    /// entry is actually inserted or replaces the current minimum (never
    /// per candidate), so at most `cap` names are ever held.
    pub(crate) fn offer(&mut self, disk: u64, node: NodeId, hardlink: bool, name: &[u8]) {
        self.considered += 1;
        if self.cap == 0 {
            return;
        }
        let insert = if self.heap.len() < self.cap {
            true
        } else {
            // Keep the higher-priority of the two (deterministic tiebreak).
            match self.heap.peek() {
                Some(Reverse(min)) => outranks(disk, node, min),
                None => false,
            }
        };
        if !insert {
            return;
        }
        if self.heap.len() >= self.cap {
            self.heap.pop();
        }
        self.heap.push(Reverse(RankedFile {
            disk,
            node,
            hardlink,
            name: lossy_basename(name),
        }));
    }

    /// Merge another heap of the same cap into this one (the filter fold's
    /// per-thread heaps). Because every globally-top-`cap` file is in its
    /// own thread's top-`cap`, keeping the top `cap` of the union yields
    /// exactly the sequential result; `considered` sums, so `truncated`
    /// agrees too. Merge order does not affect the outcome (the
    /// keep-priority is a total order).
    pub(crate) fn merge(&mut self, other: TopHeap) {
        debug_assert_eq!(self.cap, other.cap, "merging heaps of different caps");
        self.considered += other.considered;
        if self.cap == 0 {
            return;
        }
        for Reverse(entry) in other.heap {
            let insert = if self.heap.len() < self.cap {
                true
            } else {
                match self.heap.peek() {
                    Some(Reverse(min)) => outranks(entry.disk, entry.node, min),
                    None => false,
                }
            };
            if !insert {
                continue;
            }
            if self.heap.len() >= self.cap {
                self.heap.pop();
            }
            self.heap.push(Reverse(entry));
        }
    }

    pub(crate) fn truncated(&self) -> bool {
        self.considered > self.cap as u64
    }

    /// Drain (by clone) into the output order: disk desc, node asc.
    pub(crate) fn to_sorted(&self) -> Vec<TopFile> {
        let mut ranked: Vec<RankedFile> = self.heap.iter().map(|Reverse(e)| e.clone()).collect();
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        ranked
            .into_iter()
            .map(|e| TopFile {
                node: e.node,
                name: e.name,
                disk: e.disk,
                hardlink: e.hardlink,
            })
            .collect()
    }
}

/// Coverage verdict for one directory: the group whose claimed subtree it
/// falls under (outermost match), or `None` if uncovered.
type Coverage = Option<GroupId>;

/// The authoritative frozen-arena fold (D2). One streamed pass implementing
/// D1 exactly:
///
/// - a per-`DirId` coverage vector is filled in `DirId` order (topological:
///   a parent's index is always smaller than its children's, so the
///   parent's coverage is known first);
/// - each live node's own bytes are attributed to exactly one group or the
///   rest: a node inside a claimed subtree goes to the claiming group;
///   otherwise a directory that matches a dir-pattern starts (and joins)
///   its own group, a file that matches a file-pattern joins that group,
///   and everything else is rest;
/// - `HARDLINK_EXTRA` nodes contribute 0 and never rank in top-N;
///   tombstoned rows are skipped ([`Tree::children`] filters them); error
///   placeholders are counted in their covering group or the rest, exactly
///   as the subtree aggregates count them (in `tn`/`te`);
/// - the top-N list is a bounded min-heap.
///
/// `epoch` is stamped onto the result unchanged (the caller's deletion
/// epoch). Invariant, checked with a debug assertion: `Σ groups + rest ==`
/// the root subtree aggregate (post-canonical-hardlinks).
pub fn fold(tree: &Tree, patterns: &PatternSet, cap: usize, epoch: u64) -> FlatSummary {
    debug_assert!(
        patterns.len() <= MAX_GROUPS,
        "too many patterns for the memo encoding"
    );
    let mut memo = NameMemo::with_capacity(tree.name_count());
    let mut groups = vec![Bucket::default(); patterns.len()];
    let mut rest = Bucket::default();
    let mut top = TopHeap::new(cap);

    // Pass 1: coverage over the dir table in topological order.
    let dir_count = tree.dir_count();
    let mut coverage: Vec<Coverage> = vec![None; dir_count];
    let mut root: Option<DirId> = None;
    for d in tree.dir_ids() {
        let meta = tree.dir(d);
        let cov = match meta.parent {
            Some(parent) => {
                debug_assert!(parent.index() < d.index(), "dir table is not topological");
                match coverage[parent.index()] {
                    Some(g) => Some(g),
                    // Uncovered parent: this dir may start its own group.
                    None => dir_match(&mut memo, patterns, tree, meta.node),
                }
            }
            None => {
                root = Some(d);
                dir_match(&mut memo, patterns, tree, meta.node)
            }
        };
        coverage[d.index()] = cov;
    }
    let root = root.expect("every tree has a root directory");

    // The root node is nobody's child, so account its own inode explicitly
    // (mirrors `add_dir` seeding the root into the aggregates).
    let root_node = tree.dir(root).node;
    bucket(&mut groups, &mut rest, coverage[root.index()]).add(tree.node(root_node).size());

    // Pass 2: every live non-root node, visited once as its parent's child.
    for d in tree.dir_ids() {
        let cov = coverage[d.index()];
        for child in tree.children(d) {
            let node = tree.node(child);
            let flags = node.flags();
            if flags.contains(NodeFlags::HARDLINK_EXTRA) {
                continue; // contributes 0, excluded from top-N (D2).
            }
            let kind = node.kind();
            let size = node.size();
            let group = if kind.is_dir() {
                match tree.dir_of(child) {
                    // Scanned dir: its coverage already folds cov-or-own.
                    Some(dd) => coverage[dd.index()],
                    // Excluded mount (no DirMeta): own inode only, still an
                    // outermost dir match if uncovered (attack finding 9).
                    None => match cov {
                        Some(g) => Some(g),
                        None => dir_match(&mut memo, patterns, tree, child),
                    },
                }
            } else {
                match cov {
                    Some(g) => Some(g),
                    None => file_match(&mut memo, patterns, tree, child),
                }
            };
            bucket(&mut groups, &mut rest, group).add(size);
            if kind == Kind::File {
                top.offer(size.real, child, tree.is_hardlink(child), tree.name(child));
            }
        }
    }

    let summary = finish(patterns, &groups, &rest, &top, false, epoch);
    debug_assert_invariant(&summary, tree, root);
    summary
}

/// Resolve `node`'s dir-pattern verdict through the memo.
fn dir_match(memo: &mut NameMemo, patterns: &PatternSet, tree: &Tree, node: NodeId) -> Coverage {
    memo.lookup(
        patterns,
        tree.name(node),
        tree.node(node).name_ref().0,
        PatternKind::Dir,
    )
}

/// Resolve `node`'s file-pattern verdict through the memo.
fn file_match(memo: &mut NameMemo, patterns: &PatternSet, tree: &Tree, node: NodeId) -> Coverage {
    memo.lookup(
        patterns,
        tree.name(node),
        tree.node(node).name_ref().0,
        PatternKind::File,
    )
}

/// Pick the mutable bucket for `group` (a real group or the rest).
fn bucket<'a>(groups: &'a mut [Bucket], rest: &'a mut Bucket, group: Coverage) -> &'a mut Bucket {
    match group {
        Some(g) => &mut groups[g as usize],
        None => rest,
    }
}

/// Assemble a [`FlatSummary`] from the running buckets and heap.
fn finish(
    patterns: &PatternSet,
    groups: &[Bucket],
    rest: &Bucket,
    top: &TopHeap,
    provisional: bool,
    epoch: u64,
) -> FlatSummary {
    let groups_out = (0..patterns.len())
        .map(|i| {
            let (label, kind) = patterns.group(i as GroupId).expect("group index in range");
            let b = groups[i];
            GroupTotal {
                label: label.to_owned(),
                kind,
                apparent: b.apparent,
                disk: b.disk,
                entries: b.entries,
            }
        })
        .collect();
    FlatSummary {
        groups: groups_out,
        rest: RestTotal {
            apparent: rest.apparent,
            disk: rest.disk,
            entries: rest.entries,
        },
        top_files: top.to_sorted(),
        truncated: top.truncated(),
        provisional,
        epoch,
    }
}

/// Debug-only check of the partition invariant: `Σ groups + rest == root
/// subtree aggregate`.
fn debug_assert_invariant(summary: &FlatSummary, tree: &Tree, root: DirId) {
    if !cfg!(debug_assertions) {
        return;
    }
    let mut apparent = summary.rest.apparent;
    let mut disk = summary.rest.disk;
    let mut entries = summary.rest.entries;
    for g in &summary.groups {
        apparent += g.apparent;
        disk += g.disk;
        entries += g.entries;
    }
    let meta = tree.dir(root);
    debug_assert_eq!(disk, meta.td, "flat fold disk != root subtree aggregate");
    debug_assert_eq!(
        apparent, meta.ta,
        "flat fold apparent != root subtree aggregate"
    );
    debug_assert_eq!(
        entries, meta.tn,
        "flat fold entries != root subtree aggregate"
    );
}

/// Owner-side incremental accumulator (D2). Fed at node-insert time by the
/// scan owner (parent processed before child — topological), it maintains
/// the same disjoint partition as [`fold`] using first-seen hardlink
/// attribution, and snapshots a provisional [`FlatSummary`] on the owner's
/// publication cadence.
///
/// Per-node cost is a memo lookup (a dense-`Vec` index, `O(1)` on a hit; a
/// single glob on the first occurrence of a name) plus a counter add, and —
/// for regular files — one heap compare that fails fast for all but the
/// largest files. No allocation on the common path.
pub struct Accumulator {
    patterns: PatternSet,
    memo: NameMemo,
    /// Coverage indexed by `DirId`, grown as directories are added.
    coverage: Vec<Coverage>,
    groups: Vec<Bucket>,
    rest: Bucket,
    top: TopHeap,
}

impl Accumulator {
    /// Build an accumulator for `patterns` with a top-N `cap`.
    pub(crate) fn new(patterns: PatternSet, cap: usize) -> Self {
        let groups = vec![Bucket::default(); patterns.len()];
        Self {
            patterns,
            memo: NameMemo::with_capacity(0),
            coverage: Vec::new(),
            groups,
            rest: Bucket::default(),
            top: TopHeap::new(cap),
        }
    }

    fn set_coverage(&mut self, dir: DirId, cov: Coverage) {
        let idx = dir.index();
        if idx >= self.coverage.len() {
            self.coverage.resize(idx + 1, None);
        }
        self.coverage[idx] = cov;
    }

    fn coverage_of(&self, dir: DirId) -> Coverage {
        self.coverage.get(dir.index()).copied().flatten()
    }

    fn add(&mut self, group: Coverage, size: Size) {
        match group {
            Some(g) => self.groups[g as usize].add(size),
            None => self.rest.add(size),
        }
    }

    /// Record a scanned directory (root or child): compute and store its
    /// coverage, then account its own inode. Call once per directory that
    /// gets a `DirMeta`, in topological order (the owner guarantees this).
    pub(crate) fn on_dir(
        &mut self,
        dir: DirId,
        parent: Option<DirId>,
        name: &[u8],
        name_id: u32,
        size: Size,
    ) {
        let cov = match parent.and_then(|p| self.coverage_of(p)) {
            Some(g) => Some(g),
            None => self
                .memo
                .lookup(&self.patterns, name, name_id, PatternKind::Dir),
        };
        self.set_coverage(dir, cov);
        self.add(cov, size);
    }

    /// Record a non-directory entry, or a directory without a `DirMeta` (an
    /// excluded mount point). `is_extra` is the registry's
    /// `HARDLINK_EXTRA` verdict (extras contribute 0 and never rank);
    /// `is_hardlink` is the `⛓` flag for the top-N row.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_leaf(
        &mut self,
        node: NodeId,
        parent: DirId,
        name: &[u8],
        name_id: u32,
        kind: Kind,
        is_extra: bool,
        is_hardlink: bool,
        size: Size,
    ) {
        if is_extra {
            return; // contributes 0, excluded from top-N (first-seen).
        }
        let group = match self.coverage_of(parent) {
            Some(g) => Some(g),
            None if kind.is_dir() => {
                self.memo
                    .lookup(&self.patterns, name, name_id, PatternKind::Dir)
            }
            None => self
                .memo
                .lookup(&self.patterns, name, name_id, PatternKind::File),
        };
        self.add(group, size);
        if kind == Kind::File {
            self.top.offer(size.real, node, is_hardlink, name);
        }
    }

    /// Snapshot the current provisional summary (`provisional = true`).
    /// Cost is `O(groups + heap)` — a clone of the small buckets and the
    /// `<= cap`-entry heap — so it is cheap enough to call on every
    /// publication tick.
    pub(crate) fn snapshot(&self, epoch: u64) -> FlatSummary {
        finish(
            &self.patterns,
            &self.groups,
            &self.rest,
            &self.top,
            true,
            epoch,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star_and_question() {
        assert!(glob_match(b"*.log", b"app.log"));
        assert!(glob_match(b"*.log", b".log"));
        assert!(!glob_match(b"*.log", b"app.log.txt"));
        assert!(!glob_match(b"*.log", b"foolog")); // needs the literal dot
        assert!(glob_match(b"core.*", b"core.1234"));
        assert!(glob_match(b"f?o", b"foo"));
        assert!(glob_match(b"f?o", b"fXo"));
        assert!(!glob_match(b"f?o", b"fo"));
        assert!(!glob_match(b"f?o", b"fooo"));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"**", b"anything"));
    }

    #[test]
    fn glob_boundary_exact_vs_prefix() {
        // `/a/b` vs `/a/bb`-style: exact name must not match a longer one.
        assert!(glob_match(b"b", b"b"));
        assert!(!glob_match(b"b", b"bb"));
        assert!(glob_match(b"b*", b"b"));
        assert!(glob_match(b"b*", b"bb"));
        assert!(glob_match(b"a*b", b"ab"));
        assert!(glob_match(b"a*b", b"axxxb"));
        assert!(!glob_match(b"a*b", b"axxx"));
    }

    #[test]
    fn glob_braces_and_classes_are_literal() {
        // D4: `{}`/`[]` are NOT special — matched literally.
        assert!(glob_match(b"*.{log,tmp}", b"file.{log,tmp}"));
        assert!(!glob_match(b"*.{log,tmp}", b"file.log"));
        assert!(glob_match(b"core.[0-9]", b"core.[0-9]"));
        assert!(!glob_match(b"core.[0-9]", b"core.5"));
    }

    #[test]
    fn compile_detects_kind_and_strips_slash() {
        assert_eq!(
            compile_spec("node_modules/"),
            Ok((b"node_modules".to_vec(), PatternKind::Dir))
        );
        assert_eq!(
            compile_spec("*.log"),
            Ok((b"*.log".to_vec(), PatternKind::File))
        );
        assert!(compile_spec("").is_err());
        assert!(compile_spec("/").is_err()); // empty after stripping slash
        assert!(compile_spec("a/b").is_err()); // not a basename glob
    }

    #[test]
    fn presets_are_ordered_dirs_then_files() {
        let ps = PatternSet::presets();
        assert_eq!(ps.len(), 8);
        assert_eq!(ps.group(0), Some(("node_modules", PatternKind::Dir)));
        assert_eq!(ps.group(6), Some(("*.log", PatternKind::File)));
        assert_eq!(ps.first_match(b"node_modules", PatternKind::Dir), Some(0));
        assert_eq!(ps.first_match(b"app.log", PatternKind::File), Some(6));
        // A dir named like a file pattern does not match the file pattern.
        assert_eq!(ps.first_match(b"app.log", PatternKind::Dir), None);
    }

    #[test]
    fn label_shadowing_replaces_in_place() {
        let mut ps = PatternSet::presets();
        let before = ps.len();
        // Reuse the "*.log" label with a different glob: replaces in place,
        // keeping position 6.
        ps.push("*.log", "*.LOG");
        assert_eq!(ps.len(), before, "shadowing does not grow the set");
        assert_eq!(ps.group(6), Some(("*.log", PatternKind::File)));
        assert!(ps.first_match(b"app.LOG", PatternKind::File).is_some());
        assert_eq!(ps.first_match(b"app.log", PatternKind::File), None);
    }

    #[test]
    fn invalid_pattern_warns_and_is_skipped() {
        let mut ps = PatternSet::default();
        ps.push("bad", "a/b");
        ps.push("empty", "/");
        ps.push("good", "*.rs");
        assert_eq!(ps.len(), 1);
        assert_eq!(ps.warnings().len(), 2);
        assert_eq!(ps.first_match(b"lib.rs", PatternKind::File), Some(0));
    }

    fn ranked(disk: u64, id: u32) -> RankedFile {
        RankedFile {
            disk,
            node: NodeId::from_raw(id),
            hardlink: false,
            name: "n".into(),
        }
    }

    #[test]
    fn ranked_file_keep_priority_and_tiebreak() {
        assert!(ranked(100, 5) > ranked(10, 1), "larger disk ranks higher");
        // Tie on disk: smaller node id ranks higher (kept).
        assert!(
            ranked(50, 2) > ranked(50, 9),
            "smaller node id wins the tie"
        );
        // `name` plays no role in ordering.
        let a = RankedFile {
            disk: 7,
            node: NodeId::from_raw(3),
            hardlink: false,
            name: "aaa".into(),
        };
        let b = RankedFile {
            disk: 7,
            node: NodeId::from_raw(3),
            hardlink: false,
            name: "zzz".into(),
        };
        assert_eq!(a.cmp(&b), Ordering::Equal, "name is not an ordering key");
    }

    #[test]
    fn top_heap_keeps_the_largest_deterministically() {
        let mut top = TopHeap::new(2);
        for (disk, id) in [(10, 1), (30, 2), (20, 3), (30, 4)] {
            top.offer(disk, NodeId::from_raw(id), false, b"file");
        }
        assert!(top.truncated(), "4 offered, cap 2");
        let sorted = top.to_sorted();
        // Two 30-byte files; tie broken by node id asc (2 before 4).
        assert_eq!(sorted.len(), 2);
        assert_eq!((sorted[0].disk, sorted[0].node), (30, NodeId::from_raw(2)));
        assert_eq!((sorted[1].disk, sorted[1].node), (30, NodeId::from_raw(4)));
    }

    #[test]
    fn top_heap_carries_the_lossy_basename() {
        let mut top = TopHeap::new(2);
        top.offer(100, NodeId::from_raw(1), false, b"big.bin");
        top.offer(50, NodeId::from_raw(2), false, b"caf\xe9.log"); // non-UTF-8
        let sorted = top.to_sorted();
        assert_eq!(&*sorted[0].name, "big.bin");
        assert_eq!(
            &*sorted[1].name, "caf\u{fffd}.log",
            "lossy decode for display"
        );
    }

    #[test]
    fn top_heap_zero_cap_ranks_nothing() {
        let mut top = TopHeap::new(0);
        top.offer(5, NodeId::from_raw(1), false, b"file");
        assert!(top.to_sorted().is_empty());
        assert!(top.truncated());
    }

    // --- fold over a hand-built fixture arena ---

    use crate::tree::ChildRun;

    /// A directory-kind own inode (4 KiB block).
    fn dir_size() -> Size {
        Size::new(4096, 8)
    }

    /// Nodes and dirs of the fixture tree, for the fold/accumulator tests.
    struct Fixture {
        tree: Tree,
        root: DirId,
        nm: DirId,
        src: DirId,
        pkg: DirId,
        innernm: DirId,
        nm_node: NodeId,
        src_node: NodeId,
        big: NodeId,
        hard1: NodeId,
        hard2: NodeId,
        pkg_node: NodeId,
        innernm_node: NodeId,
        index_js: NodeId,
        deep_log: NodeId,
        x_js: NodeId,
        main_rs: NodeId,
        app_log: NodeId,
    }

    /// Build:
    /// ```text
    /// root/                                (rest)
    ///   node_modules/                      (group: node_modules)
    ///     pkg/
    ///       index.js
    ///       deep.log                       -> node_modules, NOT *.log (claimed)
    ///     node_modules/                    (nested: dedup, same group)
    ///       x.js
    ///   src/                               (rest)
    ///     main.rs                          (rest)
    ///     app.log                          (group: *.log)
    ///   big.bin                            (rest, top file)
    ///   hard1                              (rest, counted hardlink first)
    ///   hard2                              (HARDLINK_EXTRA: contributes 0)
    /// ```
    /// Aggregates are maintained exactly as the owner does (own inode
    /// propagated up the chain; extras excluded).
    fn fixture() -> Fixture {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"root", dir_size(), 0);
        let root = tree.add_dir(root_node, None, 1);

        // Root's children (contiguous run).
        let nm_node = tree.push_node(
            b"node_modules",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            dir_size(),
            0,
        );
        let src_node = tree.push_node(
            b"src",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            dir_size(),
            0,
        );
        let big = tree.push_node(
            b"big.bin",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(1 << 20, 2048),
            0,
        );
        let hard1 = tree.push_node(
            b"hard1",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(2000, 8),
            0,
        );
        let hard2 = tree.push_node(
            b"hard2",
            Kind::File,
            NodeFlags::HARDLINK_EXTRA,
            root_node,
            Size::new(2000, 8),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: nm_node.index() as u32,
                len: 5,
            },
        );

        let nm = tree.add_dir(nm_node, Some(root), 1);
        tree.apply_delta(root, dir_size().apparent, dir_size().real, 1, 0);
        let src = tree.add_dir(src_node, Some(root), 1);
        tree.apply_delta(root, dir_size().apparent, dir_size().real, 1, 0);
        tree.apply_delta(root, 1 << 20, 2048 * 512, 1, 0); // big
        tree.mark_hardlink_first(hard1);
        tree.apply_delta(root, 2000, 8 * 512, 1, 0); // hard1 (counted)
        // hard2 is HARDLINK_EXTRA: contributes 0, no delta.

        // node_modules' children.
        let pkg_node = tree.push_node(
            b"pkg",
            Kind::Dir,
            NodeFlags::default(),
            nm_node,
            dir_size(),
            0,
        );
        let innernm_node = tree.push_node(
            b"node_modules",
            Kind::Dir,
            NodeFlags::default(),
            nm_node,
            dir_size(),
            0,
        );
        tree.push_run(
            nm,
            ChildRun {
                start: pkg_node.index() as u32,
                len: 2,
            },
        );
        let pkg = tree.add_dir(pkg_node, Some(nm), 1);
        tree.apply_delta(nm, dir_size().apparent, dir_size().real, 1, 0);
        let innernm = tree.add_dir(innernm_node, Some(nm), 1);
        tree.apply_delta(nm, dir_size().apparent, dir_size().real, 1, 0);

        // pkg's children.
        let index_js = tree.push_node(
            b"index.js",
            Kind::File,
            NodeFlags::default(),
            pkg_node,
            Size::new(1000, 2),
            0,
        );
        let deep_log = tree.push_node(
            b"deep.log",
            Kind::File,
            NodeFlags::default(),
            pkg_node,
            Size::new(1000, 2),
            0,
        );
        tree.push_run(
            pkg,
            ChildRun {
                start: index_js.index() as u32,
                len: 2,
            },
        );
        tree.apply_delta(pkg, 1000, 1024, 1, 0);
        tree.apply_delta(pkg, 1000, 1024, 1, 0);

        // inner node_modules' children.
        let x_js = tree.push_node(
            b"x.js",
            Kind::File,
            NodeFlags::default(),
            innernm_node,
            Size::new(1000, 2),
            0,
        );
        tree.push_run(
            innernm,
            ChildRun {
                start: x_js.index() as u32,
                len: 1,
            },
        );
        tree.apply_delta(innernm, 1000, 1024, 1, 0);

        // src's children.
        let main_rs = tree.push_node(
            b"main.rs",
            Kind::File,
            NodeFlags::default(),
            src_node,
            Size::new(3000, 8),
            0,
        );
        let app_log = tree.push_node(
            b"app.log",
            Kind::File,
            NodeFlags::default(),
            src_node,
            Size::new(5000, 16),
            0,
        );
        tree.push_run(
            src,
            ChildRun {
                start: main_rs.index() as u32,
                len: 2,
            },
        );
        tree.apply_delta(src, 3000, 8 * 512, 1, 0);
        tree.apply_delta(src, 5000, 16 * 512, 1, 0);

        Fixture {
            tree,
            root,
            nm,
            src,
            pkg,
            innernm,
            nm_node,
            src_node,
            big,
            hard1,
            hard2,
            pkg_node,
            innernm_node,
            index_js,
            deep_log,
            x_js,
            main_rs,
            app_log,
        }
    }

    #[test]
    fn fold_partitions_the_fixture_disjointly() {
        let f = fixture();
        let ps = PatternSet::presets();
        let summary = fold(&f.tree, &ps, 1000, 7);

        assert!(!summary.provisional);
        assert_eq!(summary.epoch, 7);
        assert!(!summary.truncated);

        // node_modules group: own + pkg + inner nm + index.js + deep.log
        // + x.js. deep.log lands here (claimed subtree), NOT in *.log.
        let nm_group = &summary.groups[0];
        assert_eq!(nm_group.label, "node_modules");
        assert_eq!(nm_group.disk, 4096 * 3 + 1024 * 3);
        assert_eq!(nm_group.entries, 6);

        // *.log group: only app.log (deep.log was claimed by node_modules).
        let log_group = &summary.groups[6];
        assert_eq!(log_group.label, "*.log");
        assert_eq!(log_group.disk, 16 * 512);
        assert_eq!(log_group.entries, 1);

        // Rest: root own + src own + big + hard1 + main.rs.
        assert_eq!(summary.rest.disk, 4096 + 4096 + (1 << 20) + 4096 + 4096);
        assert_eq!(summary.rest.entries, 5);

        // Invariant: groups + rest == root aggregate.
        let total_disk: u64 =
            summary.groups.iter().map(|g| g.disk).sum::<u64>() + summary.rest.disk;
        assert_eq!(total_disk, f.tree.dir(f.root).td);

        // Top files: big first, hard2 (extra) absent, hard1 flagged.
        assert_eq!(summary.top_files[0].node, f.big);
        assert_eq!(
            &*summary.top_files[0].name, "big.bin",
            "basename denormalized"
        );
        assert!(!summary.top_files[0].hardlink);
        assert!(summary.top_files.iter().all(|t| t.node != f.hard2));
        let hard1_row = summary
            .top_files
            .iter()
            .find(|t| t.node == f.hard1)
            .unwrap();
        assert!(hard1_row.hardlink, "counted hardlink carries the badge");
        // 7 eligible files (big, hard1, index, deep, x, main, app).
        assert_eq!(summary.top_files.len(), 7);
    }

    #[test]
    fn accumulator_matches_the_fold_on_the_frozen_tree() {
        let f = fixture();
        let ps = PatternSet::presets();
        let folded = fold(&f.tree, &ps, 1000, 0);

        // Drive the accumulator with the same nodes, in owner order
        // (parent dir before its section's entries).
        let name_id = |n: NodeId| f.tree.node(n).name_ref().0;
        let size = |n: NodeId| f.tree.node(n).size();
        let mut accum = Accumulator::new(ps, 1000);
        accum.on_dir(
            f.root,
            None,
            b"root",
            name_id(f.tree.dir(f.root).node),
            dir_size(),
        );
        // root section
        accum.on_dir(
            f.nm,
            Some(f.root),
            b"node_modules",
            name_id(f.nm_node),
            size(f.nm_node),
        );
        accum.on_dir(
            f.src,
            Some(f.root),
            b"src",
            name_id(f.src_node),
            size(f.src_node),
        );
        accum.on_leaf(
            f.big,
            f.root,
            b"big.bin",
            name_id(f.big),
            Kind::File,
            false,
            false,
            size(f.big),
        );
        accum.on_leaf(
            f.hard1,
            f.root,
            b"hard1",
            name_id(f.hard1),
            Kind::File,
            false,
            true,
            size(f.hard1),
        );
        accum.on_leaf(
            f.hard2,
            f.root,
            b"hard2",
            name_id(f.hard2),
            Kind::File,
            true,
            true,
            size(f.hard2),
        );
        // node_modules section
        accum.on_dir(
            f.pkg,
            Some(f.nm),
            b"pkg",
            name_id(f.pkg_node),
            size(f.pkg_node),
        );
        accum.on_dir(
            f.innernm,
            Some(f.nm),
            b"node_modules",
            name_id(f.innernm_node),
            size(f.innernm_node),
        );
        // pkg section
        accum.on_leaf(
            f.index_js,
            f.pkg,
            b"index.js",
            name_id(f.index_js),
            Kind::File,
            false,
            false,
            size(f.index_js),
        );
        accum.on_leaf(
            f.deep_log,
            f.pkg,
            b"deep.log",
            name_id(f.deep_log),
            Kind::File,
            false,
            false,
            size(f.deep_log),
        );
        // inner nm section
        accum.on_leaf(
            f.x_js,
            f.innernm,
            b"x.js",
            name_id(f.x_js),
            Kind::File,
            false,
            false,
            size(f.x_js),
        );
        // src section
        accum.on_leaf(
            f.main_rs,
            f.src,
            b"main.rs",
            name_id(f.main_rs),
            Kind::File,
            false,
            false,
            size(f.main_rs),
        );
        accum.on_leaf(
            f.app_log,
            f.src,
            b"app.log",
            name_id(f.app_log),
            Kind::File,
            false,
            false,
            size(f.app_log),
        );

        let live = accum.snapshot(0);
        assert!(live.provisional);
        // Same partition, same top-N (no hardlink re-attribution here).
        assert_eq!(live.groups, folded.groups);
        assert_eq!(live.rest, folded.rest);
        assert_eq!(live.top_files, folded.top_files);
        assert_eq!(live.truncated, folded.truncated);
    }
}
