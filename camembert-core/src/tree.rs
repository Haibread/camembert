//! In-memory scan tree: a flat arena of [`Node`]s plus per-directory
//! [`DirMeta`], mutated by a **single owner** (design decision D1,
//! `docs/design/scan-tree-decisions.md`).
//!
//! - Nodes live in one `Vec<Node>`; directories have a parallel
//!   `Vec<DirMeta>`. Both are addressed by `u32` newtype indices
//!   ([`NodeId`], [`DirId`]).
//! - A directory's children are a **list of contiguous runs** (D2): one run
//!   for the ~99 % of directories whose entries fit a single scan section,
//!   N runs for giants streamed section by section.
//! - Names are interned raw bytes (see [`interner`]).
//! - Aggregation is plain (non-atomic) adds up the ancestor chain, called
//!   by the owner only — `pub(crate)` enforces that at the crate boundary.
//!
//! # Memory budget (honest, DRAM-priced — amendment 7)
//!
//! `Node` is exactly 32 bytes. At the D4 target of 10 M entries that is
//! 320 MB of nodes, plus `DirMeta` (~80 B × ~1 M dirs ≈ 80 MB), the name
//! arena (interned, typically ≪ 100 MB), and the node→dir map for
//! directories. Typical trees land near the re-baselined ~450 MB RSS;
//! unique-name-heavy trees (Maildir) and hardlink-heavy trees are the
//! documented worst cases. The packed 24-byte node stays on the backlog
//! (D4), behind the same accessors.
//!
//! # Packing limits
//!
//! `Node` packs the name reference (26 bits), kind (3 bits) and flags
//! (3 bits) into one `u32`: at most 2^26 (~67 M) **unique** names. The node
//! and dir arenas cap at 2^32 entries. Exceeding either panics with a clear
//! message rather than silently corrupting; lifting the name limit is part
//! of the packed-node follow-up.

mod interner;

pub use interner::{NameInterner, NameRef};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::size::Size;

/// Bits of `Node::name_kind` used for the name reference.
const NAME_BITS: u32 = 26;
const NAME_MASK: u32 = (1 << NAME_BITS) - 1;
const KIND_SHIFT: u32 = NAME_BITS;
const FLAGS_SHIFT: u32 = NAME_BITS + 3;

/// Index of a [`Node`] in the arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Build an id from a raw index. Only meaningful against the arena
    /// that issued the index; intended for frontends building synthetic
    /// rows/snapshots in tests.
    pub fn from_raw(index: u32) -> Self {
        Self(index)
    }
}

/// Index of a [`DirMeta`] in the directory arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirId(pub(crate) u32);

impl DirId {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Build an id from a raw index. Only meaningful against the arena
    /// that issued the index; intended for frontends building synthetic
    /// rows/snapshots in tests.
    pub fn from_raw(index: u32) -> Self {
        Self(index)
    }
}

/// Entry kind, 3 bits in the packed node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Dir = 0,
    File = 1,
    Symlink = 2,
    Block = 3,
    Char = 4,
    Fifo = 5,
    Socket = 6,
    /// `DT_UNKNOWN` with a failed stat: kind could not be determined.
    Other = 7,
}

impl Kind {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Dir,
            1 => Self::File,
            2 => Self::Symlink,
            3 => Self::Block,
            4 => Self::Char,
            5 => Self::Fifo,
            6 => Self::Socket,
            _ => Self::Other,
        }
    }

    pub fn is_dir(self) -> bool {
        self == Self::Dir
    }
}

/// Per-node flags, 3 bits in the packed node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NodeFlags(u8);

impl NodeFlags {
    /// This entry is a later link to an already-counted `(dev, ino)`; it
    /// contributes 0 to subtree aggregates (first-seen attribution, D3).
    pub const HARDLINK_EXTRA: Self = Self(1);
    /// stat (or, for the root of an unreadable dir, open) failed.
    pub const ERROR: Self = Self(1 << 1);
    /// Directory not descended into (mount boundary or kernel
    /// filesystem); the reason lives in [`Tree::excluded_reason`].
    pub const EXCLUDED: Self = Self(1 << 2);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    fn bits(self) -> u8 {
        self.0
    }

    fn from_bits(bits: u8) -> Self {
        Self(bits & 0b111)
    }
}

/// One filesystem entry. Exactly 32 bytes (see module docs).
#[derive(Debug, Clone, Copy)]
pub struct Node {
    /// Bits 0..26: [`NameRef`] index; 26..29: [`Kind`]; 29..32: [`NodeFlags`].
    name_kind: u32,
    /// Parent node (the containing directory's node; the root points to
    /// itself).
    parent: u32,
    apparent: u64,
    disk: u64,
    mtime: i64,
}

const _: () = assert!(std::mem::size_of::<Node>() == 32);

impl Node {
    fn new(
        name: NameRef,
        kind: Kind,
        flags: NodeFlags,
        parent: NodeId,
        size: Size,
        mtime: i64,
    ) -> Self {
        assert!(
            name.0 <= NAME_MASK,
            "interner overflow: more than 2^26 unique names (see tree module docs)"
        );
        Self {
            name_kind: name.0
                | (u32::from(kind as u8) << KIND_SHIFT)
                | (u32::from(flags.bits()) << FLAGS_SHIFT),
            parent: parent.0,
            apparent: size.apparent,
            disk: size.real,
            mtime,
        }
    }

    pub fn name_ref(&self) -> NameRef {
        NameRef(self.name_kind & NAME_MASK)
    }

    pub fn kind(&self) -> Kind {
        Kind::from_bits(((self.name_kind >> KIND_SHIFT) & 0b111) as u8)
    }

    pub fn flags(&self) -> NodeFlags {
        NodeFlags::from_bits((self.name_kind >> FLAGS_SHIFT) as u8)
    }

    pub fn parent(&self) -> NodeId {
        NodeId(self.parent)
    }

    /// The entry's own sizes (apparent + real/disk).
    pub fn size(&self) -> Size {
        Size {
            apparent: self.apparent,
            real: self.disk,
        }
    }

    /// mtime in unix seconds. `i64`, not `u32` — decided (pre-1970 and
    /// post-2106 timestamps exist in the wild).
    pub fn mtime(&self) -> i64 {
        self.mtime
    }
}

/// One contiguous run of children in the node arena (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildRun {
    pub start: u32,
    pub len: u32,
}

/// Scan state of a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirState {
    /// Sections still outstanding (its own or a descendant's).
    Scanning,
    /// Fully integrated, all descendants complete.
    Complete,
    /// The directory itself could not be read (open/getdents failed). The
    /// subtree below it is unknown; it still counts as complete for the
    /// cascade.
    Error,
}

/// Per-directory metadata, parallel to the dir arena.
#[derive(Debug)]
pub struct DirMeta {
    /// The directory's own node in the node arena.
    pub node: NodeId,
    /// Parent directory; `None` for the scan root.
    pub parent: Option<DirId>,
    /// Contiguous child runs, in integration order (D2).
    runs: Vec<ChildRun>,
    /// Subtree aggregate: apparent bytes (includes this dir's own inode).
    pub ta: u64,
    /// Subtree aggregate: disk bytes (`st_blocks * 512`).
    pub td: u64,
    /// Subtree aggregate: inode count (hardlink extras excluded).
    pub tn: u64,
    /// Subtree aggregate: error count (unreadable dirs + failed stats).
    pub te: u32,
    /// Outstanding completion tokens: 1 for the directory itself (dropped
    /// when its last section is integrated) + 1 per discovered child dir
    /// not yet complete.
    pub(crate) pending: u32,
    pub state: DirState,
    /// Device (`st_dev`) of the directory inode.
    pub dev: u64,
}

impl DirMeta {
    pub fn runs(&self) -> &[ChildRun] {
        &self.runs
    }
}

/// What a [`Tree::apply_removal`] subtracted from the ancestor aggregates.
///
/// For a [`NodeFlags::HARDLINK_EXTRA`] link everything is 0 (it never
/// contributed); the entry is still tombstoned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemovalDelta {
    /// Apparent bytes subtracted.
    pub apparent: u64,
    /// Disk bytes subtracted (`st_blocks * 512`).
    pub disk: u64,
    /// Inodes subtracted (hardlink extras excluded, like `tn`).
    pub entries: u64,
    /// Errors subtracted (the removed subtree's unreadables).
    pub errors: u32,
}

/// Why [`Tree::apply_removal`] refused. Nothing was tombstoned or
/// subtracted in any of these cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RemovalError {
    /// The node (or an ancestor subtree containing it) was already
    /// removed — double removal is an error, see [`Tree::apply_removal`].
    #[error("node already removed")]
    AlreadyRemoved,
    /// The scan root itself is never removable.
    #[error("refusing to remove the scan root")]
    IsRoot,
    /// Excluded mount points ([`NodeFlags::EXCLUDED`]) are never
    /// removable: their subtree was not scanned, so neither the aggregates
    /// nor the on-disk contents are known.
    #[error("refusing to remove an excluded mount point")]
    Excluded,
}

/// The arena tree. Single-writer: the `&mut self` scan methods are
/// crate-private and called by the scan owner only (D1). The one public
/// mutation is [`Tree::apply_removal`], for the post-scan phase where the
/// frozen arena has a single owner again (the UI thread).
#[derive(Debug, Default)]
pub struct Tree {
    nodes: Vec<Node>,
    dirs: Vec<DirMeta>,
    names: NameInterner,
    /// Node → directory mapping for dir nodes. Kept out of the packed node
    /// to hold the 32-byte budget; a hash lookup on descend is fine for
    /// navigation-frequency access.
    dir_of: FxHashMap<NodeId, DirId>,
    /// Why an [`NodeFlags::EXCLUDED`] directory was not descended into.
    /// Tiny (one entry per skipped mount point); feeds the dump's `ex`
    /// field and the UI.
    excluded: FxHashMap<NodeId, ExcludedReason>,
    /// Removed nodes ([`Tree::apply_removal`]). A side set rather than a
    /// node flag: [`NodeFlags`] is full (3 bits), deletions are rare, and
    /// a per-row set lookup at iteration time is cheap. Tombstoned rows
    /// are filtered out of [`Tree::children`] — the single filter point;
    /// snapshots and run lists never see them.
    tombstones: FxHashSet<NodeId>,
    /// First-seen (counted) links of `nlink > 1` inodes. Later links carry
    /// [`NodeFlags::HARDLINK_EXTRA`] in the node; the first link cannot
    /// (flags are full), so it lives here. Feeds the deletion dialog's
    /// hardlink warning.
    hardlink_firsts: FxHashSet<NodeId>,
    /// Directories (with metadata) removed so far, so
    /// [`Tree::live_dir_count`] stays honest without shrinking the arena.
    removed_dirs: u64,
}

/// Why a directory entry was recorded but not descended into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcludedReason {
    /// Mount point to another (real) filesystem; `--cross-filesystems`
    /// descends into these.
    OtherFs,
    /// Kernel pseudo-filesystem (`/proc`, `/sys`, cgroups, …): never
    /// descended into, regardless of `--cross-filesystems` — its numbers
    /// are not disk usage (HANDOFF §3: "exclure /proc, /sys").
    KernFs,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn dir_count(&self) -> usize {
        self.dirs.len()
    }

    /// Number of unique interned names. Sizes the flat-view glob memo
    /// ([`crate::flat`]), which indexes verdicts by name id.
    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn dir(&self, id: DirId) -> &DirMeta {
        &self.dirs[id.index()]
    }

    /// Why an [`NodeFlags::EXCLUDED`] node was not descended into.
    pub fn excluded_reason(&self, id: NodeId) -> Option<ExcludedReason> {
        self.excluded.get(&id).copied()
    }

    pub(crate) fn set_excluded(&mut self, id: NodeId, reason: ExcludedReason) {
        self.excluded.insert(id, reason);
    }

    /// Raw name bytes of a node.
    pub fn name(&self, id: NodeId) -> &[u8] {
        self.names.get(self.node(id).name_ref())
    }

    /// Raw bytes of an interned name by its dense id (`0..name_count()`).
    /// The filter engine ([`crate::query`]) builds per-unique-name verdict
    /// tables indexed this way.
    pub(crate) fn name_bytes(&self, name_id: u32) -> &[u8] {
        self.names.get(NameRef(name_id))
    }

    /// The [`DirId`] of a directory node, if it has directory metadata
    /// (excluded other-filesystem dirs do not).
    pub fn dir_of(&self, node: NodeId) -> Option<DirId> {
        self.dir_of.get(&node).copied()
    }

    /// Iterate a directory's children across all its runs, in integration
    /// order (D2: slice-like iteration over the run list). Rows removed by
    /// [`Tree::apply_removal`] are filtered out here — the single filter
    /// point, so snapshots and every other consumer never see tombstones.
    /// The set is empty during the scan; the per-row lookup is noise.
    pub fn children(&self, dir: DirId) -> impl Iterator<Item = NodeId> + '_ {
        self.dirs[dir.index()]
            .runs
            .iter()
            .flat_map(|run| (run.start..run.start + run.len).map(NodeId))
            .filter(move |id| !self.tombstones.contains(id))
    }

    /// All directory ids (arena order), removed ones included (the arena
    /// never shrinks; check the parent chain or use
    /// [`Tree::live_dir_count`] when tombstones matter). Captures no
    /// borrow of the tree.
    pub fn dir_ids(&self) -> impl Iterator<Item = DirId> + use<> {
        (0..u32::try_from(self.dirs.len()).expect("dir arena exceeds u32")).map(DirId)
    }

    /// Directories with metadata that have not been removed.
    pub fn live_dir_count(&self) -> u64 {
        self.dirs.len() as u64 - self.removed_dirs
    }

    /// Full path of a directory: the root's name (the path the scan was
    /// started with) joined with the names up the parent chain.
    pub fn path_of(&self, dir: DirId) -> std::path::PathBuf {
        self.path_of_node(self.dir(dir).node)
    }

    /// Full path of any node (file or directory): the root's name joined
    /// with the names up the node parent chain. This is the tree's record
    /// of where the entry was at scan time — deletion re-verifies it on
    /// disk before acting (see [`crate::delete`]).
    pub fn path_of_node(&self, node: NodeId) -> std::path::PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let mut components: Vec<&[u8]> = Vec::new();
        let mut cur = node;
        loop {
            components.push(self.name(cur));
            let parent = self.node(cur).parent();
            if parent == cur {
                break;
            }
            cur = parent;
        }
        let mut path = std::path::PathBuf::new();
        for component in components.into_iter().rev() {
            path.push(std::ffi::OsStr::from_bytes(component));
        }
        path
    }

    /// Whether a node was removed by [`Tree::apply_removal`] (directly or
    /// as part of a removed subtree).
    pub fn is_removed(&self, id: NodeId) -> bool {
        self.tombstones.contains(&id)
    }

    /// Whether a node is a link of an `nlink > 1` inode — either the
    /// counted first-seen link or a [`NodeFlags::HARDLINK_EXTRA`] later
    /// link. Deleting such an entry frees its space only when the last
    /// link to the inode goes.
    pub fn is_hardlink(&self, id: NodeId) -> bool {
        self.node(id).flags().contains(NodeFlags::HARDLINK_EXTRA)
            || self.hardlink_firsts.contains(&id)
    }

    // ---- post-scan removal (public: the frozen arena's owner — the UI
    // thread once the scan is done — is the single writer again) ----

    /// Remove a node from the tree's accounting after its on-disk entry
    /// was deleted: tombstone it (and, for a directory, its whole
    /// subtree), then propagate the negative aggregate delta up the
    /// ancestor chain (mirror of [`Tree::apply_delta`]).
    ///
    /// Must only be called **post-scan**, when the arena is frozen and
    /// this thread is its sole owner — mutating aggregates while the scan
    /// owner integrates would break the single-writer rule (D1).
    ///
    /// Accounting:
    /// - a [`NodeFlags::HARDLINK_EXTRA`] link contributed 0 to the
    ///   aggregates, so its removal subtracts 0 (deleting one link of an
    ///   `nlink > 1` inode frees nothing unless it is the last link);
    /// - a counted first-seen hardlink subtracts its full size, which is
    ///   optimistic when other links survive elsewhere — the deletion UI
    ///   warns about this; exact freeable math is the future "libérable"
    ///   column;
    /// - a directory subtracts its subtree aggregates (which already
    ///   exclude hardlink extras and previously removed children).
    ///
    /// Double removal is an **error** ([`RemovalError::AlreadyRemoved`]),
    /// not a no-op: a second call for the same node means the caller's
    /// bookkeeping is off, and silently succeeding would hide a
    /// double-subtraction bug. Removing a node inside an already-removed
    /// subtree reports the same error (the accounting already happened at
    /// the ancestor).
    pub fn apply_removal(&mut self, node: NodeId) -> Result<RemovalDelta, RemovalError> {
        if self.tombstones.contains(&node) {
            return Err(RemovalError::AlreadyRemoved);
        }
        let n = *self.node(node);
        if n.parent() == node {
            return Err(RemovalError::IsRoot);
        }
        if n.flags().contains(NodeFlags::EXCLUDED) {
            return Err(RemovalError::Excluded);
        }
        let parent_dir = self
            .dir_of(n.parent())
            .expect("non-root node's parent is a scanned directory");

        let delta = match self.dir_of(node) {
            Some(dir) => {
                let meta = self.dir(dir);
                let delta = RemovalDelta {
                    apparent: meta.ta,
                    disk: meta.td,
                    entries: meta.tn,
                    errors: meta.te,
                };
                // Tombstone the whole subtree so no descendant can be
                // removed (and double-subtracted) again. `children` skips
                // already-removed rows, whose contribution was subtracted
                // when they were removed — consistent with using the
                // dir's *current* aggregates as the delta.
                self.tombstones.insert(node);
                self.removed_dirs += 1;
                let mut stack = vec![dir];
                while let Some(d) = stack.pop() {
                    let children: Vec<NodeId> = self.children(d).collect();
                    for child in children {
                        self.tombstones.insert(child);
                        if let Some(child_dir) = self.dir_of(child) {
                            self.removed_dirs += 1;
                            stack.push(child_dir);
                        }
                    }
                }
                delta
            }
            None => {
                let size = n.size();
                let counted = !n.flags().contains(NodeFlags::HARDLINK_EXTRA);
                self.tombstones.insert(node);
                RemovalDelta {
                    apparent: if counted { size.apparent } else { 0 },
                    disk: if counted { size.real } else { 0 },
                    // `tn` excludes hardlink extras, mirror that here.
                    entries: u64::from(counted),
                    errors: u32::from(n.flags().contains(NodeFlags::ERROR)),
                }
            }
        };
        self.apply_negative_delta(parent_dir, delta);
        Ok(delta)
    }

    /// Mirror of [`Tree::apply_delta`] with negative deltas: subtract a
    /// removal from `dir` and every ancestor up to the root. `u64`
    /// subtraction: underflow is an accounting bug — loud in debug builds,
    /// clamped (never wrapped) in release.
    fn apply_negative_delta(&mut self, dir: DirId, delta: RemovalDelta) {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let meta = &mut self.dirs[d.index()];
            debug_assert!(
                meta.ta >= delta.apparent
                    && meta.td >= delta.disk
                    && meta.tn >= delta.entries
                    && meta.te >= delta.errors,
                "removal underflow: subtracting more than the aggregate holds"
            );
            meta.ta = meta.ta.saturating_sub(delta.apparent);
            meta.td = meta.td.saturating_sub(delta.disk);
            meta.tn = meta.tn.saturating_sub(delta.entries);
            meta.te = meta.te.saturating_sub(delta.errors);
            cur = meta.parent;
        }
    }

    // ---- owner-only mutation (crate-private, D1) ----

    /// Record a counted first-seen link of an `nlink > 1` inode (see
    /// [`Tree::is_hardlink`]).
    pub(crate) fn mark_hardlink_first(&mut self, id: NodeId) {
        self.hardlink_firsts.insert(id);
    }

    /// Move the "counted link" marker from `from` to `to`, keeping
    /// [`Tree::is_hardlink`] correct after post-scan canonical
    /// re-attribution moves an inode's counted link (the old link becomes a
    /// [`NodeFlags::HARDLINK_EXTRA`], the new one loses that flag but must
    /// still answer `is_hardlink`). Without this the deletion dialog's
    /// hardlink warning and the flat view's `⛓` badge would miss the
    /// canonical link.
    pub(crate) fn move_hardlink_first(&mut self, from: NodeId, to: NodeId) {
        self.hardlink_firsts.remove(&from);
        self.hardlink_firsts.insert(to);
    }

    pub(crate) fn push_node(
        &mut self,
        name: &[u8],
        kind: Kind,
        flags: NodeFlags,
        parent: NodeId,
        size: Size,
        mtime: i64,
    ) -> NodeId {
        let name = self.names.intern(name);
        let id = u32::try_from(self.nodes.len()).expect("node arena exceeds u32 entries");
        self.nodes
            .push(Node::new(name, kind, flags, parent, size, mtime));
        NodeId(id)
    }

    /// Push the root node (its own parent).
    pub(crate) fn push_root_node(&mut self, name: &[u8], size: Size, mtime: i64) -> NodeId {
        let id = u32::try_from(self.nodes.len()).expect("node arena exceeds u32 entries");
        let name = self.names.intern(name);
        self.nodes.push(Node::new(
            name,
            Kind::Dir,
            NodeFlags::default(),
            NodeId(id),
            size,
            mtime,
        ));
        NodeId(id)
    }

    /// Create directory metadata for a dir node. Initializes the subtree
    /// aggregates with the directory's *own* inode (sizes from the node,
    /// `tn = 1`), holds one pending token for the directory itself, and
    /// adds one pending token to the parent (a discovered child dir that
    /// is not yet complete).
    pub(crate) fn add_dir(&mut self, node: NodeId, parent: Option<DirId>, dev: u64) -> DirId {
        let id = u32::try_from(self.dirs.len()).expect("dir arena exceeds u32 entries");
        let own = self.node(node).size();
        self.dirs.push(DirMeta {
            node,
            parent,
            runs: Vec::new(),
            ta: own.apparent,
            td: own.real,
            tn: 1,
            te: 0,
            pending: 1,
            state: DirState::Scanning,
            dev,
        });
        if let Some(parent) = parent {
            self.dirs[parent.index()].pending += 1;
        }
        let id = DirId(id);
        self.dir_of.insert(node, id);
        id
    }

    /// Append a child run to a directory (one run per integrated section).
    pub(crate) fn push_run(&mut self, dir: DirId, run: ChildRun) {
        if run.len > 0 {
            self.dirs[dir.index()].runs.push(run);
        }
    }

    /// Batched aggregation (D1 graft): add a pre-summed section delta to
    /// `dir` and every ancestor up to the root. Plain non-atomic adds —
    /// the owner is the only writer.
    pub(crate) fn apply_delta(&mut self, dir: DirId, da: u64, dd: u64, dn: u64, de: u32) {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let meta = &mut self.dirs[d.index()];
            meta.ta += da;
            meta.td += dd;
            meta.tn += dn;
            meta.te += de;
            cur = meta.parent;
        }
    }

    /// Subtract a delta from `dir` and every ancestor up to the root — the
    /// exact inverse of [`Tree::apply_delta`]. Used by the post-scan
    /// hardlink canonical re-attribution (the amounts were added along
    /// this chain during the scan, so the subtraction cannot underflow).
    pub(crate) fn retract_delta(&mut self, dir: DirId, da: u64, dd: u64, dn: u64) {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let meta = &mut self.dirs[d.index()];
            meta.ta -= da;
            meta.td -= dd;
            meta.tn -= dn;
            cur = meta.parent;
        }
    }

    /// Set or clear [`NodeFlags::HARDLINK_EXTRA`] on a node (post-scan
    /// canonical re-attribution moves the "counted" link of an inode).
    pub(crate) fn set_hardlink_extra(&mut self, node: NodeId, extra: bool) {
        let n = &mut self.nodes[node.index()];
        let mut flags = NodeFlags::from_bits((n.name_kind >> FLAGS_SHIFT) as u8);
        if extra {
            flags.insert(NodeFlags::HARDLINK_EXTRA);
        } else {
            flags.remove(NodeFlags::HARDLINK_EXTRA);
        }
        n.name_kind =
            (n.name_kind & !(0b111 << FLAGS_SHIFT)) | (u32::from(flags.bits()) << FLAGS_SHIFT);
    }

    pub(crate) fn mark_error(&mut self, dir: DirId) {
        self.dirs[dir.index()].state = DirState::Error;
    }

    /// Drop one completion token from `dir` and cascade: when a
    /// directory's pending count reaches 0 it becomes complete and
    /// releases one token from its parent, recursively.
    ///
    /// The first decrement is the caller's token (the directory's own
    /// "self" token when its last section integrates); ancestor decrements
    /// are "child completed" tokens.
    pub(crate) fn release_token(&mut self, dir: DirId) {
        let mut cur = dir;
        loop {
            let meta = &mut self.dirs[cur.index()];
            debug_assert!(meta.pending > 0, "token released on completed dir");
            meta.pending -= 1;
            if meta.pending != 0 {
                break;
            }
            if meta.state == DirState::Scanning {
                meta.state = DirState::Complete;
            }
            match meta.parent {
                Some(parent) => cur = parent,
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_size(apparent: u64) -> Size {
        Size {
            apparent,
            real: apparent.next_multiple_of(4096),
        }
    }

    #[test]
    fn node_is_32_bytes() {
        assert_eq!(std::mem::size_of::<Node>(), 32);
    }

    #[test]
    fn node_packing_round_trips() {
        let mut tree = Tree::new();
        let root = tree.push_root_node(b"/", Size::default(), 42);
        let id = tree.push_node(
            b"caf\xe9.log",
            Kind::Symlink,
            NodeFlags::HARDLINK_EXTRA,
            root,
            Size {
                apparent: 7,
                real: 512,
            },
            -12345,
        );
        let node = tree.node(id);
        assert_eq!(tree.name(id), b"caf\xe9.log");
        assert_eq!(node.kind(), Kind::Symlink);
        assert!(node.flags().contains(NodeFlags::HARDLINK_EXTRA));
        assert!(!node.flags().contains(NodeFlags::ERROR));
        assert_eq!(node.parent(), root);
        assert_eq!(node.size().apparent, 7);
        assert_eq!(node.size().real, 512);
        assert_eq!(node.mtime(), -12345);
    }

    #[test]
    fn children_iterate_across_multiple_runs() {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/", Size::default(), 0);
        let root = tree.add_dir(root_node, None, 1);

        // Section 1: three files.
        let first = tree.push_node(
            b"a",
            Kind::File,
            NodeFlags::default(),
            root_node,
            file_size(1),
            0,
        );
        tree.push_node(
            b"b",
            Kind::File,
            NodeFlags::default(),
            root_node,
            file_size(2),
            0,
        );
        tree.push_node(
            b"c",
            Kind::File,
            NodeFlags::default(),
            root_node,
            file_size(3),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: first.0,
                len: 3,
            },
        );

        // Section 2: two more files (a "giant" streamed dir, D2).
        let fourth = tree.push_node(
            b"d",
            Kind::File,
            NodeFlags::default(),
            root_node,
            file_size(4),
            0,
        );
        tree.push_node(
            b"e",
            Kind::File,
            NodeFlags::default(),
            root_node,
            file_size(5),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: fourth.0,
                len: 2,
            },
        );

        let names: Vec<&[u8]> = tree.children(root).map(|id| tree.name(id)).collect();
        assert_eq!(names, [b"a" as &[u8], b"b", b"c", b"d", b"e"]);
        assert_eq!(tree.dir(root).runs().len(), 2);
    }

    #[test]
    fn empty_runs_are_not_stored() {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/", Size::default(), 0);
        let root = tree.add_dir(root_node, None, 1);
        tree.push_run(root, ChildRun { start: 1, len: 0 });
        assert!(tree.dir(root).runs().is_empty());
        assert_eq!(tree.children(root).count(), 0);
    }

    #[test]
    fn aggregation_walks_the_ancestor_chain() {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/", Size::default(), 0);
        let root = tree.add_dir(root_node, None, 1);
        let a_node = tree.push_node(
            b"a",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::default(),
            0,
        );
        let a = tree.add_dir(a_node, Some(root), 1);
        let b_node = tree.push_node(
            b"b",
            Kind::Dir,
            NodeFlags::default(),
            a_node,
            Size::default(),
            0,
        );
        let b = tree.add_dir(b_node, Some(a), 1);
        // Sibling of `a`: must NOT receive the delta applied at `b`.
        let c_node = tree.push_node(
            b"c",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::default(),
            0,
        );
        let c = tree.add_dir(c_node, Some(root), 1);

        tree.apply_delta(b, 100, 4096, 3, 1);

        for dir in [b, a, root] {
            let meta = tree.dir(dir);
            assert_eq!(meta.ta, 100);
            assert_eq!(meta.td, 4096);
            assert_eq!(meta.tn, 1 + 3, "own inode + 3 from the delta");
            assert_eq!(meta.te, 1);
        }
        let sibling = tree.dir(c);
        assert_eq!(
            (sibling.ta, sibling.td, sibling.tn, sibling.te),
            (0, 0, 1, 0)
        );
    }

    /// root/{f1 (100 B), sub/{leaf (10 B)}} with fully applied aggregates.
    fn removal_tree() -> (Tree, DirId, NodeId, DirId, NodeId, NodeId) {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/scan", Size::default(), 0);
        let root = tree.add_dir(root_node, None, 1);
        let f1 = tree.push_node(
            b"f1",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(100, 1),
            0,
        );
        let sub_node = tree.push_node(
            b"sub",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::new(4096, 8),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: f1.0,
                len: 2,
            },
        );
        let sub = tree.add_dir(sub_node, Some(root), 1);
        tree.apply_delta(root, 100 + 4096, 512 + 4096, 2, 0);
        let leaf = tree.push_node(
            b"leaf",
            Kind::File,
            NodeFlags::default(),
            sub_node,
            Size::new(10, 1),
            0,
        );
        tree.push_run(
            sub,
            ChildRun {
                start: leaf.0,
                len: 1,
            },
        );
        tree.apply_delta(sub, 10, 512, 1, 0);
        tree.release_token(sub);
        tree.release_token(root);
        (tree, root, f1, sub, sub_node, leaf)
    }

    #[test]
    fn removal_of_a_file_subtracts_up_the_chain() {
        let (mut tree, root, f1, sub, _, _) = removal_tree();
        let before = (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn);
        let delta = tree.apply_removal(f1).expect("file removal");
        assert_eq!(
            delta,
            RemovalDelta {
                apparent: 100,
                disk: 512,
                entries: 1,
                errors: 0
            }
        );
        assert!(tree.is_removed(f1));
        let meta = tree.dir(root);
        assert_eq!(meta.ta, before.0 - 100);
        assert_eq!(meta.td, before.1 - 512);
        assert_eq!(meta.tn, before.2 - 1);
        // Sibling dir untouched.
        assert_eq!(tree.dir(sub).ta, 4096 + 10);
    }

    #[test]
    fn removal_of_a_dir_tombstones_the_subtree() {
        let (mut tree, root, f1, _sub, sub_node, leaf) = removal_tree();
        let delta = tree.apply_removal(sub_node).expect("dir removal");
        assert_eq!(
            delta,
            RemovalDelta {
                apparent: 4096 + 10,
                disk: 4096 + 512,
                entries: 2,
                errors: 0
            }
        );
        assert!(tree.is_removed(sub_node));
        assert!(tree.is_removed(leaf), "descendants tombstoned too");
        assert!(!tree.is_removed(f1));
        // Root keeps only f1 (+ its own inode).
        let meta = tree.dir(root);
        assert_eq!((meta.ta, meta.td, meta.tn), (100, 512, 2));
        assert_eq!(tree.live_dir_count(), 1, "sub's metadata no longer live");
        // Removing a node inside the removed subtree is refused.
        assert_eq!(tree.apply_removal(leaf), Err(RemovalError::AlreadyRemoved));
    }

    #[test]
    fn removed_rows_disappear_from_children_iteration() {
        let (mut tree, root, f1, _, sub_node, _) = removal_tree();
        assert_eq!(tree.children(root).count(), 2);
        tree.apply_removal(f1).unwrap();
        let remaining: Vec<&[u8]> = tree.children(root).map(|id| tree.name(id)).collect();
        assert_eq!(remaining, [b"sub" as &[u8]]);
        tree.apply_removal(sub_node).unwrap();
        assert_eq!(tree.children(root).count(), 0);
    }

    #[test]
    fn double_removal_is_an_error() {
        let (mut tree, _, f1, _, _, _) = removal_tree();
        tree.apply_removal(f1).unwrap();
        assert_eq!(tree.apply_removal(f1), Err(RemovalError::AlreadyRemoved));
    }

    #[test]
    fn root_and_excluded_are_not_removable() {
        let (mut tree, root, _, _, _, _) = removal_tree();
        let root_node = tree.dir(root).node;
        assert_eq!(tree.apply_removal(root_node), Err(RemovalError::IsRoot));

        let mounted = tree.push_node(
            b"mnt",
            Kind::Dir,
            NodeFlags::EXCLUDED,
            root_node,
            Size::default(),
            0,
        );
        tree.set_excluded(mounted, ExcludedReason::OtherFs);
        assert_eq!(tree.apply_removal(mounted), Err(RemovalError::Excluded));
        assert!(!tree.is_removed(mounted));
    }

    #[test]
    fn hardlink_extra_removal_subtracts_zero() {
        let (mut tree, root, _, _, _, _) = removal_tree();
        let root_node = tree.dir(root).node;
        let extra = tree.push_node(
            b"extra-link",
            Kind::File,
            NodeFlags::HARDLINK_EXTRA,
            root_node,
            Size::new(500, 1),
            0,
        );
        // Extras never entered the aggregates, so nothing to add here.
        let before = (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn);
        let delta = tree.apply_removal(extra).expect("extra removal");
        assert_eq!(delta, RemovalDelta::default(), "contributed 0, frees 0");
        assert!(tree.is_removed(extra));
        let meta = tree.dir(root);
        assert_eq!((meta.ta, meta.td, meta.tn), before, "aggregates untouched");
    }

    #[test]
    fn hardlink_first_seen_is_queryable() {
        let (mut tree, root, f1, _, _, _) = removal_tree();
        let root_node = tree.dir(root).node;
        let extra = tree.push_node(
            b"link2",
            Kind::File,
            NodeFlags::HARDLINK_EXTRA,
            root_node,
            Size::new(100, 1),
            0,
        );
        tree.mark_hardlink_first(f1);
        assert!(tree.is_hardlink(f1), "counted first link");
        assert!(tree.is_hardlink(extra), "flagged extra link");
        assert!(!tree.is_hardlink(tree.dir(root).node));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "removal underflow")]
    fn removal_underflow_is_loud_in_debug() {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/r", Size::default(), 0);
        let root = tree.add_dir(root_node, None, 1);
        // Node pushed but its size never aggregated into the parent:
        // subtracting it underflows — an accounting bug that must not be
        // silent.
        let orphan_sized = tree.push_node(
            b"f",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(1000, 8),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: orphan_sized.0,
                len: 1,
            },
        );
        let _ = tree.apply_removal(orphan_sized);
    }

    #[test]
    fn completion_cascades_up_when_pending_drains() {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/", Size::default(), 0);
        let root = tree.add_dir(root_node, None, 1);
        let a_node = tree.push_node(
            b"a",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::default(),
            0,
        );
        let a = tree.add_dir(a_node, Some(root), 1);

        // Root's last section integrated: drops its self token, but `a` is
        // still scanning.
        tree.release_token(root);
        assert_eq!(tree.dir(root).state, DirState::Scanning);
        assert_eq!(tree.dir(a).state, DirState::Scanning);

        // `a`'s last section integrated: completes and cascades to root.
        tree.release_token(a);
        assert_eq!(tree.dir(a).state, DirState::Complete);
        assert_eq!(tree.dir(root).state, DirState::Complete);
    }
}
