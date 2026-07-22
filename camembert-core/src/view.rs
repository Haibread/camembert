//! View-scoped snapshots: the wait-free read path between the scan owner
//! and a UI (D5, Option A §5, `docs/design/`).
//!
//! The owner never shares the arena. Instead it publishes, via
//! [`arc_swap::ArcSwap`], a [`ViewSnapshot`] of exactly the directory being
//! viewed: an owned copy of that directory's rows (iterated over its run
//! list, D2) with each child directory's aggregates read at publish time.
//! The UI loads the current snapshot wait-free on every frame and requests
//! a different directory through a **capacity-1 latest-wins cell** (a
//! single `AtomicU64`): writing never blocks, the last request before the
//! owner looks wins, and the owner serves it on its next tick.
//!
//! Publication cadence (D5): 33 ms, degraded to 250 ms while the viewed
//! directory has more than [`DEGRADED_CHILD_THRESHOLD`] children (the
//! snapshot then carries `degraded: true` so the UI can show "updating…").
//! A nav request always publishes immediately, cadence notwithstanding.
//!
//! After the scan completes the owner thread exits and the caller receives
//! the [`crate::scan::ScanOutcome`]; post-scan navigation reads the frozen
//! arena directly through [`build_snapshot`] — see
//! [`crate::scan::Scanner::scan_live`] for why.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::tree::{DirId, DirState, NodeFlags, NodeId, Tree};

/// Normal snapshot publication cadence (D5: ≈30 fps).
pub const PUBLISH_CADENCE: Duration = Duration::from_millis(33);

/// Degraded cadence for very large viewed directories (D5).
pub const DEGRADED_CADENCE: Duration = Duration::from_millis(250);

/// Child count above which the viewed directory publishes on the degraded
/// cadence (D5: "more than ~20k children").
pub const DEGRADED_CHILD_THRESHOLD: usize = 20_000;

/// Sentinel for "no pending nav request" in the latest-wins cell.
const NAV_EMPTY: u64 = u64::MAX;

/// Scan state of one row, as captured at publish time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// Not a scanned directory (file, symlink, …): nothing pending.
    File,
    /// Directory with sections (its own or a descendant's) outstanding.
    Scanning,
    /// Directory fully integrated, all descendants complete.
    Complete,
    /// Directory that could not be read; its subtree is unknown.
    Error,
}

/// One row of a viewed directory: an owned copy of a child entry with (for
/// child directories) its live subtree aggregates.
#[derive(Debug, Clone)]
pub struct Row {
    /// Raw name bytes (owned copy — the snapshot outlives the publish).
    pub name: Box<[u8]>,
    /// The child's node in the arena.
    pub node: NodeId,
    /// Directory metadata id, when the child is a *scanned* directory.
    /// `None` for files and for excluded other-filesystem mount points
    /// (recorded but not descended into) — those are not navigable.
    pub dir: Option<DirId>,
    /// Whether the entry is a directory (including excluded mount points).
    pub is_dir: bool,
    /// Apparent bytes: subtree total for scanned dirs, own size otherwise.
    pub apparent: u64,
    /// Disk bytes (`st_blocks * 512`): subtree total for scanned dirs.
    pub disk: u64,
    /// Subtree inode count for scanned dirs, 1 otherwise.
    pub items: u64,
    /// Subtree error count for scanned dirs; 1 for a failed-stat entry.
    pub errors: u64,
    pub state: RowState,
    /// mtime in unix seconds (the entry's own).
    pub mtime: i64,
}

/// The viewed directory's own subtree aggregates.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirTotals {
    pub apparent: u64,
    pub disk: u64,
    pub items: u64,
    pub errors: u64,
}

/// Whole-scan counters as of publish time.
#[derive(Debug, Clone, Copy)]
pub struct ScanStats {
    /// Inodes integrated so far (hardlink extras excluded).
    pub entries: u64,
    /// Directories discovered so far.
    pub dirs: u64,
    /// Errors so far (unreadable dirs + failed stats).
    pub errors: u64,
    /// Disk bytes aggregated so far.
    pub disk_bytes: u64,
    /// Wall time since the scan started.
    pub elapsed: Duration,
    /// The root finished (Complete or Error): the scan is done.
    pub root_complete: bool,
}

/// Whole-scan counters read off the tree's root aggregates.
pub fn scan_stats(tree: &Tree, root: DirId, elapsed: Duration) -> ScanStats {
    let meta = tree.dir(root);
    ScanStats {
        entries: meta.tn,
        dirs: tree.live_dir_count(),
        errors: u64::from(meta.te),
        disk_bytes: meta.td,
        elapsed,
        root_complete: meta.state != DirState::Scanning,
    }
}

/// One published view of one directory. Immutable once published.
#[derive(Debug)]
pub struct ViewSnapshot {
    /// Monotonic publish counter; a UI re-sorts only when it changes.
    pub generation: u64,
    /// The directory these rows belong to.
    pub dir: DirId,
    /// Parent directory (for going up); `None` at the scan root.
    pub parent: Option<DirId>,
    /// Full path of the viewed directory.
    pub path: PathBuf,
    pub rows: Vec<Row>,
    /// The viewed directory's own subtree totals.
    pub totals: DirTotals,
    pub stats: ScanStats,
    /// Any `nlink > 1` inode was seen: totals are provisional
    /// (first-seen attribution, D3) until the scan ends.
    pub hardlinks_seen: bool,
    /// Published on the degraded 250 ms cadence (D5): the UI shows
    /// "updating…".
    pub degraded: bool,
}

impl ViewSnapshot {
    /// Placeholder published before the first tick (empty view of the
    /// not-yet-built root).
    fn initial(path: PathBuf) -> Self {
        Self {
            generation: 0,
            dir: DirId(0),
            parent: None,
            path,
            rows: Vec::new(),
            totals: DirTotals::default(),
            stats: ScanStats {
                entries: 0,
                dirs: 0,
                errors: 0,
                disk_bytes: 0,
                elapsed: Duration::ZERO,
                root_complete: false,
            },
            hardlinks_seen: false,
            degraded: false,
        }
    }
}

/// Build a snapshot of `dir` by copying its rows out of the arena — never
/// the whole tree. Child directories read their live aggregates at call
/// time. Public because the post-scan phase uses it too: once the scan
/// finishes the UI owns the frozen arena and serves its own navigation
/// through this function (single-threaded, no owner thread kept alive).
pub fn build_snapshot(
    tree: &Tree,
    dir: DirId,
    generation: u64,
    stats: ScanStats,
    hardlinks_seen: bool,
    degraded: bool,
) -> ViewSnapshot {
    let meta = tree.dir(dir);
    let mut rows = Vec::with_capacity(children_count(tree, dir));
    for node_id in tree.children(dir) {
        let node = tree.node(node_id);
        let is_dir = node.kind().is_dir();
        let sub = if is_dir { tree.dir_of(node_id) } else { None };
        let row = match sub {
            Some(d) => {
                let m = tree.dir(d);
                Row {
                    name: tree.name(node_id).into(),
                    node: node_id,
                    dir: Some(d),
                    is_dir,
                    apparent: m.ta,
                    disk: m.td,
                    items: m.tn,
                    errors: u64::from(m.te),
                    state: match m.state {
                        DirState::Scanning => RowState::Scanning,
                        DirState::Complete => RowState::Complete,
                        DirState::Error => RowState::Error,
                    },
                    mtime: node.mtime(),
                }
            }
            None => Row {
                name: tree.name(node_id).into(),
                node: node_id,
                dir: None,
                is_dir,
                apparent: node.size().apparent,
                disk: node.size().real,
                items: 1,
                errors: u64::from(node.flags().contains(NodeFlags::ERROR)),
                state: RowState::File,
                mtime: node.mtime(),
            },
        };
        rows.push(row);
    }
    ViewSnapshot {
        generation,
        dir,
        parent: meta.parent,
        path: tree.path_of(dir),
        rows,
        totals: DirTotals {
            apparent: meta.ta,
            disk: meta.td,
            items: meta.tn,
            errors: u64::from(meta.te),
        },
        stats,
        hardlinks_seen,
        degraded,
    }
}

/// Number of children of `dir` (sum of its run lengths, D2). Counts
/// tombstoned rows too — used only as a capacity hint and for the
/// degraded-cadence threshold, both of which tolerate a post-removal
/// overcount (removals happen after the scan; the cadence is then moot).
pub fn children_count(tree: &Tree, dir: DirId) -> usize {
    tree.dir(dir)
        .runs()
        .iter()
        .map(|run| run.len as usize)
        .sum()
}

/// Shared handle between the scan owner and a UI.
///
/// - UI side: [`ViewBus::load`] (wait-free snapshot read, every frame),
///   [`ViewBus::request`] (latest-wins nav), [`ViewBus::cancel`].
/// - Owner side (crate-internal): `take_request` + `publish`.
#[derive(Debug)]
pub struct ViewBus {
    snapshot: ArcSwap<ViewSnapshot>,
    /// Capacity-1 latest-wins nav cell: `NAV_EMPTY` or a `DirId` index.
    nav: AtomicU64,
    cancel: AtomicBool,
}

impl ViewBus {
    pub(crate) fn new(root_path: PathBuf) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(ViewSnapshot::initial(root_path)),
            nav: AtomicU64::new(NAV_EMPTY),
            cancel: AtomicBool::new(false),
        }
    }

    /// Current snapshot, wait-free. Safe to call every frame.
    pub fn load(&self) -> Arc<ViewSnapshot> {
        self.snapshot.load_full()
    }

    /// Ask the owner to view `dir`. Never blocks; if a previous request is
    /// still unserved it is overwritten (latest wins).
    pub fn request(&self, dir: DirId) {
        self.nav.store(u64::from(dir.0), Ordering::Release);
    }

    /// Ask the scan to stop early. Workers notice per directory; the owner
    /// drains and returns a partial, `cancelled` outcome.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Owner side: consume the pending nav request, if any.
    pub(crate) fn take_request(&self) -> Option<DirId> {
        match self.nav.swap(NAV_EMPTY, Ordering::AcqRel) {
            NAV_EMPTY => None,
            raw => Some(DirId(raw as u32)),
        }
    }

    pub(crate) fn publish(&self, snapshot: ViewSnapshot) {
        self.snapshot.store(Arc::new(snapshot));
    }
}

/// Owner-side publication state: which directory is viewed, when it was
/// last published, and the running generation counter. Driven from the
/// scan owner's tick (between batch integrations).
pub(crate) struct ViewPublisher {
    bus: Arc<ViewBus>,
    /// `None` until the first tick (the root is not known before the scan
    /// builds it).
    viewed: Option<DirId>,
    generation: u64,
    last_publish: Option<Instant>,
    started: Instant,
    /// The final `root_complete` snapshot went out (published exactly
    /// once, cadence notwithstanding).
    published_complete: bool,
}

impl ViewPublisher {
    pub(crate) fn new(bus: Arc<ViewBus>) -> Self {
        Self {
            bus,
            viewed: None,
            generation: 0,
            last_publish: None,
            started: Instant::now(),
            published_complete: false,
        }
    }

    /// One owner tick: serve a nav request immediately, otherwise
    /// republish when the cadence for the viewed directory elapsed (D5).
    pub(crate) fn tick(&mut self, tree: &Tree, root: DirId, hardlinks_seen: bool) {
        let mut viewed = *self.viewed.get_or_insert(root);
        let mut force = false;
        if let Some(requested) = self.bus.take_request() {
            // Stale ids cannot normally occur (the UI only sees ids from
            // snapshots), but never index the arena on an unchecked value.
            if requested.index() < tree.dir_count() {
                self.viewed = Some(requested);
                viewed = requested;
                force = true;
            }
        }

        let degraded = children_count(tree, viewed) > DEGRADED_CHILD_THRESHOLD;
        let cadence = if degraded {
            DEGRADED_CADENCE
        } else {
            PUBLISH_CADENCE
        };
        let root_complete = tree.dir(root).state != DirState::Scanning;
        if root_complete && !self.published_complete {
            force = true;
        }
        let due = self
            .last_publish
            .is_none_or(|last| last.elapsed() >= cadence);
        if !(force || due) {
            return;
        }

        self.generation += 1;
        let stats = scan_stats(tree, root, self.started.elapsed());
        let snapshot = build_snapshot(
            tree,
            viewed,
            self.generation,
            stats,
            hardlinks_seen,
            degraded,
        );
        self.published_complete = root_complete;
        self.last_publish = Some(Instant::now());
        self.bus.publish(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Size;
    use crate::tree::{ChildRun, Kind};

    /// root/{f1 (100 B), f2 (50 B), sub/{leaf (10 B)}} built directly with
    /// the owner-side arena mutators.
    fn sample_tree() -> (Tree, DirId, DirId) {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/scan", Size::new(4096, 8), 1000);
        let root = tree.add_dir(root_node, None, 1);

        let first = tree.push_node(
            b"f1",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(100, 1),
            111,
        );
        tree.push_node(
            b"f2",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(50, 1),
            222,
        );
        let sub_node = tree.push_node(
            b"sub",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::new(4096, 8),
            333,
        );
        tree.push_run(
            root,
            ChildRun {
                start: first.index() as u32,
                len: 3,
            },
        );
        let sub = tree.add_dir(sub_node, Some(root), 1);
        tree.apply_delta(root, 100 + 50 + 4096, 512 + 512 + 4096, 3, 0);

        let leaf = tree.push_node(
            b"leaf",
            Kind::File,
            NodeFlags::default(),
            sub_node,
            Size::new(10, 1),
            444,
        );
        tree.push_run(
            sub,
            ChildRun {
                start: leaf.index() as u32,
                len: 1,
            },
        );
        tree.apply_delta(sub, 10, 512, 1, 0);
        tree.release_token(sub);
        tree.release_token(root);
        (tree, root, sub)
    }

    fn stats_of(tree: &Tree, root: DirId) -> ScanStats {
        scan_stats(tree, root, Duration::from_millis(5))
    }

    #[test]
    fn snapshot_rows_match_the_tree() {
        let (tree, root, sub) = sample_tree();
        let snap = build_snapshot(&tree, root, 1, stats_of(&tree, root), false, false);

        assert_eq!(snap.dir, root);
        assert_eq!(snap.parent, None);
        assert_eq!(snap.path, PathBuf::from("/scan"));
        assert_eq!(snap.rows.len(), 3);

        let f1 = &snap.rows[0];
        assert_eq!(&*f1.name, b"f1");
        assert!(!f1.is_dir);
        assert_eq!(f1.dir, None);
        assert_eq!((f1.apparent, f1.disk, f1.items), (100, 512, 1));
        assert_eq!(f1.state, RowState::File);
        assert_eq!(f1.mtime, 111);

        let sub_row = &snap.rows[2];
        assert_eq!(&*sub_row.name, b"sub");
        assert!(sub_row.is_dir);
        assert_eq!(sub_row.dir, Some(sub));
        // Subtree aggregates, not own size: sub's own 4096 + leaf's 10.
        assert_eq!(sub_row.apparent, 4096 + 10);
        assert_eq!(sub_row.disk, 4096 + 512);
        assert_eq!(sub_row.items, 2);
        assert_eq!(sub_row.state, RowState::Complete);

        // Viewed dir totals = the dir's own aggregates.
        assert_eq!(snap.totals.apparent, tree.dir(root).ta);
        assert_eq!(snap.totals.disk, tree.dir(root).td);
        assert_eq!(snap.totals.items, tree.dir(root).tn);

        // Sub-view: parent set, one row.
        let sub_snap = build_snapshot(&tree, sub, 2, stats_of(&tree, root), false, false);
        assert_eq!(sub_snap.parent, Some(root));
        assert_eq!(sub_snap.rows.len(), 1);
        assert_eq!(&*sub_snap.rows[0].name, b"leaf");
        assert_eq!(sub_snap.path, PathBuf::from("/scan/sub"));
    }

    #[test]
    fn nav_request_is_latest_wins() {
        let bus = ViewBus::new(PathBuf::from("/x"));
        assert_eq!(bus.take_request(), None);
        bus.request(DirId(1));
        bus.request(DirId(2));
        assert_eq!(bus.take_request(), Some(DirId(2)), "last write wins");
        assert_eq!(bus.take_request(), None, "consumed");
    }

    #[test]
    fn publication_generation_increments() {
        let (tree, root, sub) = sample_tree();
        let bus = Arc::new(ViewBus::new(PathBuf::from("/scan")));
        let mut publisher = ViewPublisher::new(Arc::clone(&bus));

        assert_eq!(bus.load().generation, 0, "initial placeholder");
        publisher.tick(&tree, root, false);
        assert_eq!(bus.load().generation, 1, "first tick publishes");

        // Within the cadence and without a nav request: no republish
        // (the sample tree's root is complete, and the one forced
        // completion publish already went out with the first tick)...
        publisher.tick(&tree, root, false);
        assert_eq!(bus.load().generation, 1);

        // ...but a nav request publishes immediately, cadence or not.
        bus.request(sub);
        publisher.tick(&tree, root, false);
        let snap = bus.load();
        assert_eq!(snap.generation, 2);
        assert_eq!(snap.dir, sub);
        assert_eq!(snap.parent, Some(root));
    }

    #[test]
    fn completion_forces_a_final_publish() {
        let (tree, root, _) = sample_tree();
        let bus = Arc::new(ViewBus::new(PathBuf::from("/scan")));
        let mut publisher = ViewPublisher::new(Arc::clone(&bus));

        publisher.tick(&tree, root, false);
        let first = bus.load();
        assert!(first.stats.root_complete);

        // Root already complete: exactly one forced completion publish;
        // further ticks inside the cadence stay quiet.
        publisher.tick(&tree, root, false);
        assert_eq!(bus.load().generation, first.generation);
    }

    #[test]
    fn degraded_flag_above_child_threshold() {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/big", Size::default(), 0);
        let root = tree.add_dir(root_node, None, 1);
        let n = DEGRADED_CHILD_THRESHOLD + 1;
        let first = tree.push_node(
            b"c0",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(1, 1),
            0,
        );
        for i in 1..n {
            tree.push_node(
                format!("c{i}").as_bytes(),
                Kind::File,
                NodeFlags::default(),
                root_node,
                Size::new(1, 1),
                0,
            );
        }
        tree.push_run(
            root,
            ChildRun {
                start: first.index() as u32,
                len: n as u32,
            },
        );

        let bus = Arc::new(ViewBus::new(PathBuf::from("/big")));
        let mut publisher = ViewPublisher::new(Arc::clone(&bus));
        publisher.tick(&tree, root, false);
        let snap = bus.load();
        assert!(snap.degraded, ">20k children publish degraded (D5)");
        assert_eq!(snap.rows.len(), n);
    }

    #[test]
    fn hardlink_flag_reaches_the_snapshot() {
        let (tree, root, _) = sample_tree();
        let snap = build_snapshot(&tree, root, 1, stats_of(&tree, root), true, false);
        assert!(snap.hardlinks_seen);
    }
}
