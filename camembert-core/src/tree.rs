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

use rustc_hash::FxHashMap;

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
    /// Directory on another filesystem: recorded, not descended into.
    pub const EXCLUDED_OTHERFS: Self = Self(1 << 2);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
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

/// The arena tree. Single-writer: all `&mut self` methods are crate-private
/// and called by the scan owner only (D1).
#[derive(Debug, Default)]
pub struct Tree {
    nodes: Vec<Node>,
    dirs: Vec<DirMeta>,
    names: NameInterner,
    /// Node → directory mapping for dir nodes. Kept out of the packed node
    /// to hold the 32-byte budget; a hash lookup on descend is fine for
    /// navigation-frequency access.
    dir_of: FxHashMap<NodeId, DirId>,
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

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn dir(&self, id: DirId) -> &DirMeta {
        &self.dirs[id.index()]
    }

    /// Raw name bytes of a node.
    pub fn name(&self, id: NodeId) -> &[u8] {
        self.names.get(self.node(id).name_ref())
    }

    /// The [`DirId`] of a directory node, if it has directory metadata
    /// (excluded other-filesystem dirs do not).
    pub fn dir_of(&self, node: NodeId) -> Option<DirId> {
        self.dir_of.get(&node).copied()
    }

    /// Iterate a directory's children across all its runs, in integration
    /// order (D2: slice-like iteration over the run list).
    pub fn children(&self, dir: DirId) -> impl Iterator<Item = NodeId> + '_ {
        self.dirs[dir.index()]
            .runs
            .iter()
            .flat_map(|run| (run.start..run.start + run.len).map(NodeId))
    }

    /// All directory ids (arena order).
    pub fn dir_ids(&self) -> impl Iterator<Item = DirId> {
        (0..u32::try_from(self.dirs.len()).expect("dir arena exceeds u32")).map(DirId)
    }

    /// Full path of a directory: the root's name (the path the scan was
    /// started with) joined with the names up the parent chain.
    pub fn path_of(&self, dir: DirId) -> std::path::PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let mut components: Vec<&[u8]> = Vec::new();
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let meta = self.dir(d);
            components.push(self.name(meta.node));
            cur = meta.parent;
        }
        let mut path = std::path::PathBuf::new();
        for component in components.into_iter().rev() {
            path.push(std::ffi::OsStr::from_bytes(component));
        }
        path
    }

    // ---- owner-only mutation (crate-private, D1) ----

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
