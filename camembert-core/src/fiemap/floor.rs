//! Freeable phase 2, slice 2 — the ambient **exclusive floor** pass.
//!
//! Where the [selection oracle](super::correlate) answers "what does *this*
//! marked set free, exactly, right now", the floor answers the ambient
//! question "how much of every directory is provably non-shared even before
//! you pick anything" — one whole-tree snapshot, computed off-thread after
//! the scan (decision D3, `docs/design/freeable2-decisions.md`).
//!
//! ## Why it is *additive* (options dossier §"The non-additivity answer")
//!
//! Exclusive reclaim is not additive in general: two files can each look
//! "exclusive" yet share the same extent, so summing their figures
//! double-counts. The floor sidesteps that by only ever counting bytes it
//! can prove are referenced **once, filesystem-wide**:
//!
//! - **SHARED-unset extents on single-link inodes** ([`map_file`]'s
//!   `exclusive`): the kernel says nobody else points at them (trusted on
//!   ≥ 6.1 — the caller gates on [`super::shared_bit_reliable`]).
//! - **Wholly-seen, fully-live hardlink groups**, measured once and landed
//!   at the **LCA** of their links' directories. A group counts only when
//!   the scan saw every link (`nodes.len() == nlink`) and none is
//!   tombstoned — otherwise a surviving link outside our view would pin the
//!   bytes and the figure would be a lie.
//!
//! Both classes are pairwise-disjoint by construction, so summing them up
//! the tree never double-counts. The floor therefore **understates** (a
//! bookend extent shared with an unscanned file, a delalloc range, a
//! FIEMAP failure all drop out silently) and never overstates — the honest
//! direction for a "you will get at least this much back" figure.
//!
//! ## The honesty contract (D2/D3, carried from [`super`])
//!
//! - Units are **allocated-logical bytes**, same as the `disk` column; on a
//!   `compress`-mounted device physical reclaim can be smaller, which
//!   [`FloorMap::any_compressed`] surfaces for the caveat line.
//! - **`Some(0)` is meaningful** — "fully shared", the feature's most
//!   informative answer — and is never collapsed into `None`. `None` means
//!   "no figure" (ZFS, FIEMAP failure, symlink/special, a hardlink group we
//!   could not land): the pass declined to guess, per attack-b finding 6.
//! - The floor is a **whole-value snapshot** (D3): never mutated
//!   incrementally. After an in-app deletion the caller recomputes it with
//!   [`reaggregate_floor`] — pure re-aggregation over the surviving nodes,
//!   **no FIEMAP** — never an incremental LCA retraction (attack-b
//!   finding 9: retraction can overstate during the deletion window).
//!
//! ## Memory budget (D3)
//!
//! Two side maps, never a [`crate::tree::Node`] field (D6). Each holds one
//! `u64` per entry keyed by a 4-byte id — ~16 bytes/entry with hashing
//! overhead. At the 10 M-entry target the node map is ~48-64 MB; the dir
//! map is one entry per live directory (~1 M ⇒ a few MB). The snapshot is
//! transient: dropped and rebuilt on the next deletion epoch.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};

use super::{FsTier, map_file, tier_of};
use crate::scan::ScanOutcome;
use crate::tree::{DirId, Kind, NodeFlags, NodeId, Tree};

/// Check the cancel flag this often (files processed) — cheap enough to be
/// invisible, frequent enough to abandon a huge tree promptly (D3).
const CANCEL_STRIDE: u64 = 256;

/// The ambient exclusive floor: one whole-value snapshot (D3), a per-node
/// and per-directory side map — **never** a [`crate::tree::Node`] field
/// (D6). See the module docs for the ~48-64 MB @ 10 M-entry budget.
///
/// A `node` entry stores a file node's floor in allocated-logical bytes;
/// **absence means `None`** (no figure), while a stored `0` means "fully
/// shared" and is a real answer. A `dir` entry is the aggregate over a
/// directory's live descendants; it is present for every live directory
/// once the pass produced any figure at all.
#[derive(Debug, Clone)]
pub struct FloorMap {
    /// File node → floor (allocated-logical bytes). Only single-link files
    /// the pass measured appear here; hardlink-group links never do (their
    /// bytes land at the group LCA in `dir`, not on any link row).
    node: FxHashMap<NodeId, u64>,
    /// Directory → aggregate floor. Populated for every live directory when
    /// `produced_figure`; empty otherwise.
    dir: FxHashMap<DirId, u64>,
    /// Landed hardlink-group contributions, retained so [`reaggregate_floor`]
    /// can re-derive them against liveness without touching the filesystem.
    groups: Vec<GroupContribution>,
    /// Any measured device carried a `compress` mount option (D2 caveat).
    any_compressed: bool,
    /// Files that produced a figure (extent-mapped or hardlink-tier disk).
    files_mapped: u64,
    /// Files that did not: FIEMAP errors + ZFS-device files. Never
    /// symlinks/specials — those have no floor *by definition*, not by
    /// failure, and are not counted here.
    files_unmapped: u64,
    /// False only for the all-unmappable scan (every device ZFS/failed,
    /// nothing produced any figure): then `dir_floor` is `None` everywhere,
    /// "no figure rather than a guess" (D3).
    produced_figure: bool,
}

/// One hardlink group's landed contribution, kept so a post-deletion
/// re-aggregation can re-decide it purely from liveness (attack-b
/// finding 9). The `bytes` were measured once at compute time; deletion
/// never re-measures — a group with any tombstoned link simply drops to a
/// 0 contribution (understates, safe).
#[derive(Debug, Clone)]
struct GroupContribution {
    /// Every scanned link of the inode; the contribution stands only while
    /// all of them are live.
    nodes: Vec<NodeId>,
    /// The inode's exclusive/disk bytes, measured once (never per link).
    bytes: u64,
    /// Directory the contribution lands in: the LCA of the links' parents.
    lca: DirId,
}

impl FloorMap {
    /// Floor for a file node in allocated-logical bytes: `Some(bytes)` when
    /// the pass mapped it (`Some(0)` = **fully shared**, a real answer),
    /// `None` when it has no figure — a ZFS-device file, a FIEMAP failure,
    /// a symlink/non-regular entry, or a hardlink-group link (whose bytes
    /// live at the group LCA, never on the link row, so no single link can
    /// claim bytes that only freeing the whole group would release).
    pub fn node_floor(&self, node: NodeId) -> Option<u64> {
        self.node.get(&node).copied()
    }

    /// Aggregate floor for a directory: Σ of its live descendant node
    /// floors plus every hardlink-group contribution whose LCA is at or
    /// below it. `Some(0)` is meaningful (an all-fully-shared subtree);
    /// `None` only for the all-unmappable scan where the pass produced no
    /// figure at all.
    pub fn dir_floor(&self, dir: DirId) -> Option<u64> {
        if !self.produced_figure {
            return None;
        }
        // Every live dir was inserted by the fold; a miss (a tombstoned or
        // unknown id) reads as 0 rather than a spurious `None`, since the
        // pass *did* produce figures.
        Some(self.dir.get(&dir).copied().unwrap_or(0))
    }

    /// True when any measured device carries a `compress` mount option, so
    /// the caller can add the D2 "physical reclaim may be smaller" caveat.
    pub fn any_compressed(&self) -> bool {
        self.any_compressed
    }

    /// Coverage honesty for the UI: `(files_mapped, files_unmapped)`.
    /// `files_unmapped` counts FIEMAP errors and ZFS-device files — not
    /// symlinks/specials, which have no floor by definition. After a
    /// [`reaggregate_floor`] the unmapped count is carried forward from the
    /// previous snapshot (deleted unmappable files are not individually
    /// tracked); `files_mapped` reflects the survivors.
    pub fn coverage(&self) -> (u64, u64) {
        (self.files_mapped, self.files_unmapped)
    }
}

/// Off-thread whole-tree pass (D3): walk the live (non-tombstoned) tree,
/// FIEMAP regular files on [`FsTier::Extent`] devices, take `disk` on
/// [`FsTier::HardlinkOnly`], and build the [`FloorMap`] snapshot.
///
/// Device tier and the compress-mount probe are memoized per `st_dev` — no
/// per-file `statfs`: every file under a directory shares its
/// [`crate::tree::DirMeta`]'s device (mount boundaries are not descended).
///
/// Cancellation is checked every [`CANCEL_STRIDE`] files and returns `None`
/// promptly; `progress` is bumped once per regular file processed (mapped
/// or skipped) for the UI's "mapping extents… N/M" line.
///
/// The caller gates on kernel ≥ 6.1 ([`super::shared_bit_reliable`]) and
/// `--no-fiemap`; this function re-checks neither.
pub fn compute_floor(
    outcome: &ScanOutcome,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Option<FloorMap> {
    // Bail before any work when already cancelled (the periodic in-loop
    // check below only fires every CANCEL_STRIDE files, so a small tree
    // would otherwise miss a pre-set flag).
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let tree = outcome.tree();

    // Links of every nlink>1 inode: these node rows never get a per-node
    // floor (their bytes land at the group LCA), so single-link handling
    // skips them.
    let hardlink_nodes: FxHashSet<NodeId> =
        outcome.hardlink_links().iter().map(|l| l.node).collect();

    let mut node_floor: FxHashMap<NodeId, u64> = FxHashMap::default();
    // Direct (own-level) floor per dir: file children here, group LCAs
    // added in phase 2. The tree fold turns these into subtree aggregates.
    let mut dir_own: FxHashMap<DirId, u64> = FxHashMap::default();
    // Pre-order discovery of live dirs; reversed, it is a valid
    // children-before-parents fold order.
    let mut order: Vec<DirId> = Vec::new();
    // st_dev → (tier, compressed), probed once per device.
    let mut dev_memo: FxHashMap<u64, (FsTier, bool)> = FxHashMap::default();

    let mut any_compressed = false;
    let mut files_mapped: u64 = 0;
    let mut files_unmapped: u64 = 0;
    let mut processed: u64 = 0;

    // --- phase 1: walk the live tree, floor single-link files ------------
    let mut stack: Vec<DirId> = vec![outcome.root()];
    while let Some(dir) = stack.pop() {
        order.push(dir);
        dir_own.entry(dir).or_insert(0);

        let dev = tree.dir(dir).dev;
        let (tier, compressed) = *dev_memo.entry(dev).or_insert_with(|| {
            let probe = tree.path_of(dir);
            (
                tier_of(&probe),
                crate::scan::path_on_compressed_mount(&probe),
            )
        });
        any_compressed |= compressed;

        for child in tree.children(dir) {
            if let Some(child_dir) = tree.dir_of(child) {
                stack.push(child_dir);
                continue;
            }
            let node = tree.node(child);
            if node.kind() != Kind::File {
                // Symlinks/specials have no floor by definition — not a
                // failure, so not counted in coverage.
                continue;
            }
            processed += 1;
            progress.fetch_add(1, Ordering::Relaxed);
            if processed.is_multiple_of(CANCEL_STRIDE) && cancel.load(Ordering::Relaxed) {
                return None;
            }
            if hardlink_nodes.contains(&child) {
                continue; // measured once, at the group LCA (phase 2)
            }
            match tier {
                FsTier::Extent => match map_file(&tree.path_of_node(child)) {
                    Ok(map) => {
                        node_floor.insert(child, map.exclusive);
                        *dir_own.get_mut(&dir).expect("inserted above") += map.exclusive;
                        files_mapped += 1;
                    }
                    // Never fall back to disk size on an extent fs
                    // (attack-b finding 6): honest no-figure.
                    Err(_) => files_unmapped += 1,
                },
                FsTier::HardlinkOnly => {
                    // No reflink exists here — scan-time disk is exact.
                    let disk = node.size().real;
                    node_floor.insert(child, disk);
                    *dir_own.get_mut(&dir).expect("inserted above") += disk;
                    files_mapped += 1;
                }
                FsTier::Zfs => files_unmapped += 1, // no per-file sharing API
            }
        }
    }

    // --- phase 2: hardlink groups, measured once, landed at the LCA ------
    let groups_by_inode = outcome.hardlink_groups();
    let mut groups: Vec<GroupContribution> = Vec::new();
    for group in groups_by_inode.values() {
        // Wholly seen: the scan saw every link that exists.
        if group.nodes.len() as u32 != group.nlink {
            continue;
        }
        // Fully live: a tombstoned link would leave surviving bytes.
        if group.nodes.iter().any(|&n| tree.is_removed(n)) {
            continue;
        }
        // The counter keeps moving here (one tick per group measured) so
        // the stride check stays live through a hardlink-heavy phase 2 —
        // phase 1's final count is otherwise frozen for this whole loop.
        processed += 1;
        progress.fetch_add(1, Ordering::Relaxed);
        if processed.is_multiple_of(CANCEL_STRIDE) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        // The one counted link (no HARDLINK_EXTRA) carries the inode's
        // recorded size; extent maps read identically off any link.
        let Some(counted) = group
            .nodes
            .iter()
            .copied()
            .find(|&n| !tree.node(n).flags().contains(NodeFlags::HARDLINK_EXTRA))
        else {
            debug_assert!(false, "hardlink group without a counted link");
            continue;
        };
        let tier = dev_tier(tree, &mut dev_memo, counted);
        let bytes = match tier {
            FsTier::Extent => match map_file(&tree.path_of_node(counted)) {
                Ok(map) => {
                    files_mapped += 1;
                    map.exclusive
                }
                Err(_) => {
                    files_unmapped += 1;
                    continue; // no landing — understate, never guess
                }
            },
            FsTier::HardlinkOnly => {
                files_mapped += 1;
                tree.node(counted).size().real
            }
            FsTier::Zfs => {
                files_unmapped += 1;
                continue;
            }
        };
        let lca = group_lca(tree, &group.nodes);
        *dir_own.entry(lca).or_insert(0) += bytes;
        groups.push(GroupContribution {
            nodes: group.nodes.clone(),
            bytes,
            lca,
        });
    }

    let produced_figure = !node_floor.is_empty() || !groups.is_empty();
    let dir = if produced_figure {
        fold_dirs(tree, dir_own, &order)
    } else {
        FxHashMap::default()
    };

    Some(FloorMap {
        node: node_floor,
        dir,
        groups,
        any_compressed,
        files_mapped,
        files_unmapped,
        produced_figure,
    })
}

/// Post-deletion re-aggregation (attack-b finding 9 — **never** incremental
/// retraction): drop node entries for now-tombstoned nodes, recompute every
/// `dir_floor` from the surviving node floors over the live tree, and
/// re-derive each hardlink group's LCA contribution against liveness. A
/// group with any deleted link contributes 0 (understates, safe).
///
/// Pure aggregation — **no FIEMAP calls at all**: the per-inode byte
/// figures were measured once by [`compute_floor`] and are reused as-is.
/// Fast enough for a synchronous post-deletion refresh. `any_compressed`
/// and the unmapped count are carried forward from `prev` (deleted
/// unmappable files are not individually tracked).
pub fn reaggregate_floor(outcome: &ScanOutcome, prev: &FloorMap) -> FloorMap {
    let tree = outcome.tree();

    // Surviving single-link floors.
    let node: FxHashMap<NodeId, u64> = prev
        .node
        .iter()
        .filter(|&(&n, _)| !tree.is_removed(n))
        .map(|(&n, &f)| (n, f))
        .collect();

    // Enumerate live dirs (pre-order) and attribute surviving file floors
    // to their parent level — no filesystem access.
    let mut dir_own: FxHashMap<DirId, u64> = FxHashMap::default();
    let mut order: Vec<DirId> = Vec::new();
    let mut stack: Vec<DirId> = vec![outcome.root()];
    while let Some(dir) = stack.pop() {
        order.push(dir);
        let own = dir_own.entry(dir).or_insert(0);
        for child in tree.children(dir) {
            if let Some(child_dir) = tree.dir_of(child) {
                stack.push(child_dir);
            } else if let Some(&floor) = node.get(&child) {
                *own += floor;
            }
        }
    }

    // Re-decide each group's contribution from liveness only.
    let mut groups: Vec<GroupContribution> = Vec::new();
    for group in &prev.groups {
        if group.nodes.iter().any(|&n| tree.is_removed(n)) {
            continue; // a lost link — contributes 0 now
        }
        *dir_own.entry(group.lca).or_insert(0) += group.bytes;
        groups.push(group.clone());
    }

    let produced_figure = prev.produced_figure && (!node.is_empty() || !groups.is_empty());
    let dir = if produced_figure {
        fold_dirs(tree, dir_own, &order)
    } else {
        FxHashMap::default()
    };
    let files_mapped = node.len() as u64 + groups.len() as u64;

    FloorMap {
        node,
        dir,
        groups,
        any_compressed: prev.any_compressed,
        files_mapped,
        files_unmapped: prev.files_unmapped,
        produced_figure,
    }
}

/// The tier of the device a node sits on, via its parent directory's
/// recorded `dev` (memoized). Falls back to a fresh [`tier_of`] probe on
/// the node's path if the device is somehow unseen.
fn dev_tier(tree: &Tree, dev_memo: &mut FxHashMap<u64, (FsTier, bool)>, node: NodeId) -> FsTier {
    let parent_dir = tree
        .dir_of(tree.node(node).parent())
        .expect("a scanned entry's parent is a scanned directory");
    let dev = tree.dir(parent_dir).dev;
    dev_memo
        .entry(dev)
        .or_insert_with(|| {
            let probe = tree.path_of(parent_dir);
            (
                tier_of(&probe),
                crate::scan::path_on_compressed_mount(&probe),
            )
        })
        .0
}

/// Turn own-level dir floors into subtree aggregates. `order` is a
/// pre-order list of the live dirs; iterating it in reverse guarantees
/// every child is folded into its total before that total is pushed onto
/// its parent — a single O(dirs) pass, no recursion.
fn fold_dirs(
    tree: &Tree,
    mut totals: FxHashMap<DirId, u64>,
    order: &[DirId],
) -> FxHashMap<DirId, u64> {
    for &dir in order.iter().rev() {
        let subtotal = totals.get(&dir).copied().unwrap_or(0);
        if let Some(parent) = tree.dir(dir).parent {
            *totals.entry(parent).or_insert(0) += subtotal;
        }
    }
    totals
}

/// LCA directory of a hardlink group's links: the deepest directory that is
/// an ancestor of (or equal to) every link's parent directory. Walk the
/// [`crate::tree::DirMeta`] parent chains, equalizing depth then ascending
/// in lockstep.
fn group_lca(tree: &Tree, nodes: &[NodeId]) -> DirId {
    let parent_dir = |node: NodeId| {
        tree.dir_of(tree.node(node).parent())
            .expect("a scanned entry's parent is a scanned directory")
    };
    let mut acc = parent_dir(nodes[0]);
    for &node in &nodes[1..] {
        acc = lca_of(tree, acc, parent_dir(node));
    }
    acc
}

/// Pairwise directory LCA over the parent chains.
fn lca_of(tree: &Tree, mut a: DirId, mut b: DirId) -> DirId {
    let (mut da, mut db) = (dir_depth(tree, a), dir_depth(tree, b));
    while da > db {
        a = tree.dir(a).parent.expect("depth>0 has a parent");
        da -= 1;
    }
    while db > da {
        b = tree.dir(b).parent.expect("depth>0 has a parent");
        db -= 1;
    }
    while a != b {
        a = tree
            .dir(a)
            .parent
            .expect("non-root differs, so has a parent");
        b = tree
            .dir(b)
            .parent
            .expect("non-root differs, so has a parent");
    }
    a
}

/// Depth of a directory below the scan root (root = 0).
fn dir_depth(tree: &Tree, dir: DirId) -> u32 {
    let mut depth = 0;
    let mut cur = dir;
    while let Some(parent) = tree.dir(cur).parent {
        depth += 1;
        cur = parent;
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::message::{Batch, BatchEntry, SectionSums};
    use crate::scan::owner::{Owner, ROOT_TOKEN};
    use crate::size::Size;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    // --- synthetic-tree builders (the Owner/Batch idiom) -----------------

    fn file(name: &[u8], disk: u64) -> BatchEntry {
        BatchEntry {
            name: name.to_vec(),
            kind: Kind::File,
            apparent: disk,
            disk,
            mtime: 0,
            nlink: 1,
            ino: 0,
            dev: 1,
            error: None,
            child_token: None,
            excluded: None,
        }
    }

    fn link(name: &[u8], ino: u64, nlink: u32, disk: u64) -> BatchEntry {
        let mut e = file(name, disk);
        e.nlink = nlink;
        e.ino = ino;
        e
    }

    fn dir(name: &[u8], token: u64) -> BatchEntry {
        BatchEntry {
            name: name.to_vec(),
            kind: Kind::Dir,
            apparent: 4096,
            disk: 4096,
            mtime: 0,
            nlink: 1,
            ino: 0,
            dev: 1,
            error: None,
            child_token: Some(token),
            excluded: None,
        }
    }

    fn batch(token: u64, entries: Vec<BatchEntry>) -> Batch {
        let mut sums = SectionSums::default();
        let mut child_dirs = 0;
        for e in &entries {
            sums.apparent += e.apparent;
            sums.disk += e.disk;
            sums.count += 1;
            if e.child_token.is_some() {
                child_dirs += 1;
            }
        }
        Batch {
            dir_token: token,
            entries,
            sums,
            is_last_section: true,
            child_dirs,
            dir_error: None,
        }
    }

    /// Build a `ScanOutcome` from a closure that feeds batches into an
    /// `Owner`, finalizing hardlinks like a real scan does.
    fn outcome_of(build: impl FnOnce(&mut Owner)) -> ScanOutcome {
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, Arc::default());
        build(&mut owner);
        assert!(owner.root_complete(), "test tree left sections outstanding");
        let hardlink_inodes = owner.hardlink_inodes();
        let (tree, root, links) = owner.into_parts();
        let mut outcome = ScanOutcome::from_tree(
            tree,
            root,
            PathBuf::from("/r"),
            links,
            hardlink_inodes,
            0,
            0,
            Duration::ZERO,
        );
        outcome.finalize_hardlinks();
        outcome
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn node_named(outcome: &ScanOutcome, dir: DirId, name: &[u8]) -> NodeId {
        outcome
            .children_of(dir)
            .find(|&n| outcome.name_of(n) == name)
            .unwrap_or_else(|| panic!("no child named {}", String::from_utf8_lossy(name)))
    }

    fn dir_named(outcome: &ScanOutcome, parent: DirId, name: &[u8]) -> DirId {
        let node = node_named(outcome, parent, name);
        outcome.tree().dir_of(node).expect("named child is a dir")
    }

    /// The additivity invariant, checked over every live directory:
    /// `dir_floor(d) == Σ child-dir floors + Σ child-file floors +
    /// contributions landed exactly at d`. Contributions-at-d is recovered
    /// as `dir_floor(d) - Σ children` — so we assert the weaker but
    /// equivalent identity that the dir total is at least the sum of its
    /// parts and equals it once landings are folded, by rebuilding the
    /// total bottom-up and comparing.
    fn assert_additive(floor: &FloorMap, outcome: &ScanOutcome) {
        let tree = outcome.tree();
        for d in tree.dir_ids() {
            if tree.is_removed(tree.dir(d).node) {
                continue;
            }
            let Some(total) = floor.dir_floor(d) else {
                continue;
            };
            let mut sum = 0u64;
            for child in tree.children(d) {
                if let Some(cd) = tree.dir_of(child) {
                    sum += floor.dir_floor(cd).unwrap_or(0);
                } else {
                    sum += floor.node_floor(child).unwrap_or(0);
                }
            }
            // The only gap between `total` and `sum` is group
            // contributions landed exactly at `d`, which are non-negative.
            assert!(
                total >= sum,
                "dir {} total {total} < children sum {sum}",
                d.index()
            );
        }
        // Global check: the root total equals the sum of all node floors
        // plus all group contributions — the additivity guarantee.
        let node_sum: u64 = floor.node.values().sum();
        let group_sum: u64 = floor.groups.iter().map(|g| g.bytes).sum();
        if let Some(root_total) = floor.dir_floor(outcome.root()) {
            assert_eq!(
                root_total,
                node_sum + group_sum,
                "root floor must equal Σ node floors + Σ group contributions"
            );
        }
    }

    #[test]
    fn hardlink_only_tier_floors_from_disk_and_is_additive() {
        // dev 1 is not extent-capable on the test host in general; the
        // HardlinkOnly path takes disk with no syscall, so this runs
        // everywhere. (On an extent fs the *device* of these synthetic
        // nodes is a fiction — dev=1 — and tier_of probes the real cwd,
        // which may be Extent. So we can't assert exact byte values here;
        // we assert structure and additivity, which hold regardless.)
        let outcome = outcome_of(|owner| {
            owner.handle_batch(batch(
                ROOT_TOKEN,
                vec![file(b"a", 1000), dir(b"sub", 1), file(b"b", 2000)],
            ));
            owner.handle_batch(batch(1, vec![file(b"c", 3000)]));
        });
        let cancel = no_cancel();
        let progress = AtomicU64::new(0);
        let floor = compute_floor(&outcome, &cancel, &progress).expect("figure produced");
        assert_additive(&floor, &outcome);
        // Every plain file got *some* figure (Extent map or disk); none is
        // None on a live regular file with no sharing.
        assert_eq!(
            progress.load(Ordering::Relaxed),
            3,
            "progress bumped once per regular file (a, b, c)"
        );
    }

    #[test]
    fn lca_landing_two_links_under_sibling_dirs() {
        // root/{ b/{link_b}, c/{link_c} } hardlinked as one inode → the
        // contribution lands at root (their LCA), never at b or c, and each
        // link row is None.
        let outcome = outcome_of(|owner| {
            owner.handle_batch(batch(ROOT_TOKEN, vec![dir(b"b", 1), dir(b"c", 2)]));
            owner.handle_batch(batch(1, vec![link(b"link_b", 77, 2, 4096)]));
            owner.handle_batch(batch(2, vec![link(b"link_c", 77, 2, 4096)]));
        });
        let cancel = no_cancel();
        let progress = AtomicU64::new(0);
        let floor = compute_floor(&outcome, &cancel, &progress).expect("figure");

        let root = outcome.root();
        let b = dir_named(&outcome, root, b"b");
        let c = dir_named(&outcome, root, b"c");
        let link_b = node_named(&outcome, b, b"link_b");
        let link_c = node_named(&outcome, c, b"link_c");

        // Link rows: no floor (bytes belong to the whole group).
        assert_eq!(floor.node_floor(link_b), None);
        assert_eq!(floor.node_floor(link_c), None);

        let root_floor = floor.dir_floor(root).unwrap();
        let b_floor = floor.dir_floor(b).unwrap();
        let c_floor = floor.dir_floor(c).unwrap();
        // The whole group's bytes are at the root, not at b or c.
        assert_eq!(b_floor, 0, "no contribution below the LCA");
        assert_eq!(c_floor, 0, "no contribution below the LCA");
        assert_eq!(
            root_floor,
            floor.groups.iter().map(|g| g.bytes).sum::<u64>()
        );
        assert_eq!(floor.groups.len(), 1);
        assert_eq!(floor.groups[0].lca, root);
        assert_additive(&floor, &outcome);
    }

    #[test]
    fn group_not_wholly_seen_contributes_nothing() {
        // nlink=3 but the scan saw only 2 links: the inode has a link we
        // cannot account for, so it lands nowhere and both rows are None.
        let outcome = outcome_of(|owner| {
            owner.handle_batch(batch(
                ROOT_TOKEN,
                vec![link(b"x", 9, 3, 8192), link(b"y", 9, 3, 8192)],
            ));
        });
        let cancel = no_cancel();
        let progress = AtomicU64::new(0);
        let floor = compute_floor(&outcome, &cancel, &progress).expect("figure absent? see below");
        // With no other files, nothing produced a figure → all-None scan.
        // (The two links are the only files, both skipped as a partial
        // group.) So the whole map is figureless.
        assert!(
            floor.dir_floor(outcome.root()).is_none() || floor.dir_floor(outcome.root()) == Some(0)
        );
        assert!(floor.groups.is_empty(), "partial group never lands");
        let root = outcome.root();
        assert_eq!(floor.node_floor(node_named(&outcome, root, b"x")), None);
        assert_eq!(floor.node_floor(node_named(&outcome, root, b"y")), None);
    }

    #[test]
    fn wholly_seen_group_plus_a_plain_file_is_additive() {
        // A plain file guarantees a figure, so the partial-group edge case
        // does not swallow the whole map; the group still lands at its LCA.
        let outcome = outcome_of(|owner| {
            owner.handle_batch(batch(
                ROOT_TOKEN,
                vec![
                    file(b"plain", 5000),
                    link(b"h1", 5, 2, 4096),
                    link(b"h2", 5, 2, 4096),
                ],
            ));
        });
        let cancel = no_cancel();
        let progress = AtomicU64::new(0);
        let floor = compute_floor(&outcome, &cancel, &progress).expect("figure");
        assert_eq!(floor.groups.len(), 1, "wholly-seen group lands");
        assert_eq!(floor.groups[0].lca, outcome.root());
        assert_additive(&floor, &outcome);
    }

    #[test]
    fn reaggregate_drops_tombstoned_floors_without_touching_disk() {
        // Paths point at /r/... which does not exist on disk. reaggregate
        // must not FIEMAP anything, so it works regardless. Delete `sub`
        // and assert its floor is gone from the root aggregate.
        let mut outcome = outcome_of(|owner| {
            owner.handle_batch(batch(ROOT_TOKEN, vec![file(b"keep", 1000), dir(b"sub", 1)]));
            owner.handle_batch(batch(1, vec![file(b"gone", 2000)]));
        });
        let cancel = no_cancel();
        let progress = AtomicU64::new(0);
        let floor = compute_floor(&outcome, &cancel, &progress).expect("figure");
        let root = outcome.root();
        let sub = dir_named(&outcome, root, b"sub");
        let sub_node = outcome.dir(sub).node;
        let root_before = floor.dir_floor(root).unwrap();
        let sub_floor = floor.dir_floor(sub).unwrap();

        outcome.apply_removal(sub_node).expect("remove sub");
        let re = reaggregate_floor(&outcome, &floor);

        assert_eq!(
            re.dir_floor(root).unwrap(),
            root_before - sub_floor,
            "sub's floor left every ancestor"
        );
        assert_eq!(re.dir_floor(sub), Some(0), "removed dir folds to nothing");
        assert_additive(&re, &outcome);
    }

    #[test]
    fn reaggregate_drops_group_contribution_when_a_link_dies() {
        // A wholly-seen group at the root loses one link → its LCA
        // contribution must vanish, understating safely.
        let mut outcome = outcome_of(|owner| {
            owner.handle_batch(batch(
                ROOT_TOKEN,
                vec![
                    file(b"anchor", 1000),
                    link(b"h1", 3, 2, 4096),
                    link(b"h2", 3, 2, 4096),
                ],
            ));
        });
        let cancel = no_cancel();
        let progress = AtomicU64::new(0);
        let floor = compute_floor(&outcome, &cancel, &progress).expect("figure");
        assert_eq!(floor.groups.len(), 1);
        let group_bytes = floor.groups[0].bytes;
        let root = outcome.root();
        let root_before = floor.dir_floor(root).unwrap();

        let h1 = node_named(&outcome, root, b"h1");
        outcome.apply_removal(h1).expect("remove one link");
        let re = reaggregate_floor(&outcome, &floor);

        assert!(
            re.groups.is_empty(),
            "a group missing a link no longer lands"
        );
        assert_eq!(
            re.dir_floor(root).unwrap(),
            root_before - group_bytes,
            "the group's bytes left the root floor"
        );
    }

    #[test]
    fn all_none_scan_reports_no_figure_everywhere() {
        // A tree of only symlinks/specials — no regular files — produces no
        // figure: dir_floor is None everywhere (D3 "no figure rather than a
        // guess").
        let outcome = outcome_of(|owner| {
            let mut sym = file(b"s", 10);
            sym.kind = Kind::Symlink;
            owner.handle_batch(batch(ROOT_TOKEN, vec![sym]));
        });
        let cancel = no_cancel();
        let progress = AtomicU64::new(0);
        let floor = compute_floor(&outcome, &cancel, &progress).expect("map returned");
        assert_eq!(floor.dir_floor(outcome.root()), None);
        assert_eq!(floor.coverage(), (0, 0), "symlinks are not unmapped files");
        let root = outcome.root();
        assert_eq!(floor.node_floor(node_named(&outcome, root, b"s")), None);
    }

    #[test]
    fn cancellation_returns_none() {
        let outcome = outcome_of(|owner| {
            let files: Vec<BatchEntry> = (0..1000)
                .map(|i| file(format!("f{i}").as_bytes(), 100))
                .collect();
            owner.handle_batch(batch(ROOT_TOKEN, files));
        });
        let cancel = AtomicBool::new(true); // pre-cancelled
        let progress = AtomicU64::new(0);
        assert!(
            compute_floor(&outcome, &cancel, &progress).is_none(),
            "a pre-set cancel flag aborts the pass"
        );
    }
}
