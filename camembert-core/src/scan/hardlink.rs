//! Post-scan hardlink canonical re-attribution (dump-format decision D2,
//! scan-tree decision D3: off the critical path, run once on the frozen
//! tree).
//!
//! During the scan the owner attributes each `nlink > 1` inode to its
//! **first-seen** link (cheap, but scan-order dependent). The dump format
//! defines the **canonical owner** as the link whose full path is smallest
//! under the raw-byte, component-wise comparator (spec §4/§8) — that makes
//! aggregates reproducible across scans of an identical tree. This module
//! moves each inode's contribution from the first-seen link's ancestor
//! chain to the canonical link's: plain single-threaded arithmetic,
//! subtract along one chain, add along the other.
//!
//! # Which size the group carries, and what the root total does
//!
//! The subtraction uses the **first-seen** link's recorded size and the
//! addition uses the **canonical** link's recorded size. They are two
//! `statx` snapshots of the same inode taken at different points in the
//! scan, so they normally agree — and then the two chains, both ending at
//! the root, cancel above their lowest common ancestor and **root totals
//! are unchanged**. If a concurrent rewrite between those two `statx` calls
//! made them differ, the **canonical link's recorded size wins for the
//! whole group** and the root total shifts by (canonical − first-seen)
//! accordingly. That is the only reconciliation that keeps the tree's real
//! invariant intact: **every directory aggregate stays equal to the sum of
//! its own entry lines** (subtree-aggregate consistency, which the dump
//! writer and diff depend on). Each chain moves by exactly the size of the
//! node that lives on it, so at every ancestor the change equals the sum of
//! its children's changes — patching a residual delta at the root alone
//! would instead break parent = sum-of-children somewhere below it.

use rustc_hash::FxHashMap;
use tracing::debug;

use crate::tree::{DirId, NodeFlags, NodeId, Tree};

/// Side record for one `nlink > 1` non-directory link. The packed 32-byte
/// [`crate::tree::Node`] has no room for `ino`/`dev`/`nlink`, so the owner
/// keeps them here (also consumed by the dump writer for the `i`/`l`
/// entry fields).
///
/// The registry records **only `nlink > 1` inodes**: a file with a single
/// link never appears here. Callers applying the freeable-2 D4 hardlink
/// rule ("all links inside the selection, and the scan saw every link")
/// therefore treat absence from the registry as `nlink == 1`.
#[derive(Debug, Clone, Copy)]
pub struct HardlinkLink {
    /// The tree node of this link (one entry per *link*, so an inode with
    /// three scanned links yields three records).
    pub node: NodeId,
    /// `st_dev` of the inode (hardlinks never cross devices).
    pub dev: u64,
    /// `st_ino` of the inode.
    pub ino: u64,
    /// `st_nlink` as stat reported it: the number of links that *exist*
    /// on the filesystem, scanned or not.
    pub nlink: u32,
}

/// All scanned links of one `nlink > 1` inode, grouped by `(dev, ino)` —
/// the freeable-2 D4 rule inputs: an inode's bytes are freeable only when
/// the selection contains every scanned link (`nodes`) **and** the scan
/// saw every link that exists (`nodes.len() as u32 == nlink`).
#[derive(Debug, Clone)]
pub struct HardlinkGroup {
    /// Every scanned link of the inode (at least one).
    pub nodes: Vec<NodeId>,
    /// `st_nlink` — total links existing on the filesystem.
    pub nlink: u32,
}

/// Group the flat link registry by inode identity `(dev, ino)`. Each
/// group's `nlink` is the stat-reported link count (they agree across
/// links of one inode; the max is kept defensively).
pub(crate) fn group_links(links: &[HardlinkLink]) -> FxHashMap<(u64, u64), HardlinkGroup> {
    let mut groups: FxHashMap<(u64, u64), HardlinkGroup> = FxHashMap::default();
    for link in links {
        let group = groups
            .entry((link.dev, link.ino))
            .or_insert_with(|| HardlinkGroup {
                nodes: Vec::new(),
                nlink: link.nlink,
            });
        group.nodes.push(link.node);
        group.nlink = group.nlink.max(link.nlink);
    }
    groups
}

/// Re-attribute every multi-link inode to its canonical owner. Returns the
/// number of inodes whose owner moved. Idempotent: once the canonical link
/// is the counted one, every group is a no-op.
pub(crate) fn reattribute(tree: &mut Tree, links: &[HardlinkLink]) -> u64 {
    let mut groups: FxHashMap<(u64, u64), Vec<NodeId>> = FxHashMap::default();
    for link in links {
        groups
            .entry((link.dev, link.ino))
            .or_default()
            .push(link.node);
    }

    let mut moved = 0;
    for nodes in groups.values() {
        if nodes.len() < 2 {
            continue;
        }
        // Exactly one link per group is counted (no HARDLINK_EXTRA): the
        // first seen on the initial pass, the canonical after this one.
        let Some(counted) = nodes
            .iter()
            .copied()
            .find(|&n| !tree.node(n).flags().contains(NodeFlags::HARDLINK_EXTRA))
        else {
            debug_assert!(false, "hardlink group without a counted link");
            continue;
        };
        let canonical = *nodes
            .iter()
            .min_by(|&&a, &&b| cmp_paths(tree, a, b))
            .expect("group has >= 2 links");
        if canonical == counted {
            continue;
        }

        let old_chain = parent_dir(tree, counted);
        let new_chain = parent_dir(tree, canonical);
        if old_chain != new_chain {
            // Retract the first-seen link's recorded size from its chain and
            // apply the canonical link's recorded size to the canonical
            // chain — each node moves by exactly its own size, so every
            // directory aggregate stays equal to the sum of its own entry
            // lines (subtree-aggregate consistency; module docs). The two
            // are `statx` snapshots of one inode: normally identical, and
            // then the root nets out. A concurrent rewrite between the
            // snapshots can make them diverge — the canonical size then
            // wins for the group and the root shifts by (applied -
            // retracted), the only reconciliation that keeps parent =
            // sum-of-children everywhere.
            let retracted = tree.node(counted).size();
            let applied = tree.node(canonical).size();
            if retracted != applied {
                debug!(
                    retracted_apparent = retracted.apparent,
                    applied_apparent = applied.apparent,
                    "hardlink re-attribution size drift (inode rewritten between \
                     the two statx snapshots); canonical link's size wins for the \
                     group, root total shifts accordingly"
                );
            }
            tree.retract_delta(old_chain, retracted.apparent, retracted.real, 1);
            tree.apply_delta(new_chain, applied.apparent, applied.real, 1, 0);
        }
        tree.set_hardlink_extra(counted, true);
        tree.set_hardlink_extra(canonical, false);
        // Keep the counted-link side set (Tree::is_hardlink) consistent
        // with the flags we just flipped.
        tree.move_hardlink_first(counted, canonical);
        moved += 1;
    }
    moved
}

/// The directory a node's entry line lives in (its parent).
fn parent_dir(tree: &Tree, node: NodeId) -> DirId {
    let parent = tree.node(node).parent();
    tree.dir_of(parent)
        .expect("a scanned entry's parent is a scanned directory")
}

/// Raw-byte, component-wise full-path comparison (spec §4): names up the
/// parent chain, compared root-first.
fn cmp_paths(tree: &Tree, a: NodeId, b: NodeId) -> std::cmp::Ordering {
    let components = |node: NodeId| {
        let mut comps: Vec<&[u8]> = Vec::new();
        let mut cur = node;
        loop {
            comps.push(tree.name(cur));
            let parent = tree.node(cur).parent();
            if parent == cur {
                break;
            }
            cur = parent;
        }
        comps.reverse();
        comps
    };
    components(a).cmp(&components(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::message::{Batch, BatchEntry, SectionSums};
    use crate::scan::owner::{Owner, ROOT_TOKEN};
    use crate::size::Size;
    use crate::tree::Kind;
    use std::sync::Arc;

    fn entry(name: &[u8], kind: Kind, apparent: u64, disk: u64) -> BatchEntry {
        BatchEntry {
            name: name.to_vec(),
            kind,
            apparent,
            disk,
            mtime: 0,
            nlink: 1,
            ino: 0,
            dev: 1,
            error: false,
            child_token: None,
            excluded: None,
        }
    }

    fn link_entry(name: &[u8], ino: u64) -> BatchEntry {
        link_entry_sized(name, ino, 1000, 1024)
    }

    fn link_entry_sized(name: &[u8], ino: u64, apparent: u64, disk: u64) -> BatchEntry {
        let mut e = entry(name, Kind::File, apparent, disk);
        e.nlink = 2;
        e.ino = ino;
        e
    }

    fn dir_entry(name: &[u8], token: u64) -> BatchEntry {
        let mut e = entry(name, Kind::Dir, 4096, 4096);
        e.child_token = Some(token);
        e
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

    /// root/{aaa/link0, zzz/link1}, hardlinked pair, with zzz's section
    /// integrated FIRST so first-seen attribution lands on zzz — the
    /// canonical owner (aaa/link0) differs.
    fn scan_with_wrong_first_seen() -> (Tree, DirId, DirId, DirId, Vec<HardlinkLink>) {
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, Arc::default());
        owner.handle_batch(batch(
            ROOT_TOKEN,
            vec![dir_entry(b"aaa", 1), dir_entry(b"zzz", 2)],
        ));
        // zzz first: its link is counted, aaa's becomes the extra.
        owner.handle_batch(batch(2, vec![link_entry(b"link1", 42)]));
        owner.handle_batch(batch(1, vec![link_entry(b"link0", 42)]));
        assert!(owner.root_complete());

        let root = owner.root();
        let (tree, _, links) = owner.into_parts();
        let mut dirs = tree.dir_ids();
        let _ = dirs.next(); // root
        let aaa = dirs.next().unwrap();
        let zzz = dirs.next().unwrap();
        assert_eq!(tree.name(tree.dir(aaa).node), b"aaa");
        assert_eq!(tree.name(tree.dir(zzz).node), b"zzz");
        (tree, root, aaa, zzz, links)
    }

    #[test]
    fn reattribution_moves_totals_to_the_canonical_owner() {
        let (mut tree, root, aaa, zzz, links) = scan_with_wrong_first_seen();

        // Before: first-seen attribution counted the inode under zzz.
        assert_eq!(tree.dir(aaa).ta, 4096, "aaa: own inode only");
        assert_eq!(tree.dir(zzz).ta, 4096 + 1000, "zzz: own inode + link");
        let root_before = (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn);

        let moved = reattribute(&mut tree, &links);
        assert_eq!(moved, 1);

        // After: canonical owner (aaa/link0, smallest path) counts it.
        assert_eq!(tree.dir(aaa).ta, 4096 + 1000);
        assert_eq!(tree.dir(aaa).td, 4096 + 1024);
        assert_eq!(tree.dir(aaa).tn, 2);
        assert_eq!(tree.dir(zzz).ta, 4096);
        assert_eq!(tree.dir(zzz).td, 4096);
        assert_eq!(tree.dir(zzz).tn, 1);

        // Global totals unchanged (both chains end at the root).
        assert_eq!(
            (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn),
            root_before
        );

        // Flags moved with the attribution.
        let extras: Vec<(Vec<u8>, bool)> = links
            .iter()
            .map(|l| {
                (
                    tree.name(l.node).to_vec(),
                    tree.node(l.node)
                        .flags()
                        .contains(NodeFlags::HARDLINK_EXTRA),
                )
            })
            .collect();
        assert!(extras.contains(&(b"link1".to_vec(), true)));
        assert!(extras.contains(&(b"link0".to_vec(), false)));

        // Both links still answer `is_hardlink` — the counted-link side set
        // followed the re-attribution (the canonical link is no longer
        // flagged EXTRA but must still count as a hardlink).
        for link in &links {
            assert!(
                tree.is_hardlink(link.node),
                "every link of an nlink>1 inode stays a hardlink after finalize"
            );
        }
    }

    #[test]
    fn reattribution_with_size_drift_lets_the_canonical_size_win() {
        // A concurrent rewrite changed the inode between the scan's two
        // statx snapshots: the first-seen link (zzz/link1) recorded
        // 1000/1024, the canonical link (aaa/link0, smallest path) recorded
        // 2000/2048. Per the module's documented semantics the canonical
        // size wins for the whole group; the root total shifts by the
        // difference, and every directory aggregate stays equal to the sum
        // of its own entry lines.
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, Arc::default());
        owner.handle_batch(batch(
            ROOT_TOKEN,
            vec![dir_entry(b"aaa", 1), dir_entry(b"zzz", 2)],
        ));
        // zzz integrated first, so its link is the first-seen/counted one.
        owner.handle_batch(batch(2, vec![link_entry_sized(b"link1", 42, 1000, 1024)]));
        owner.handle_batch(batch(1, vec![link_entry_sized(b"link0", 42, 2000, 2048)]));
        assert!(owner.root_complete());

        let root = owner.root();
        let (mut tree, _, links) = owner.into_parts();
        let mut dirs = tree.dir_ids();
        let _ = dirs.next(); // root
        let aaa = dirs.next().unwrap();
        let zzz = dirs.next().unwrap();
        assert_eq!(tree.name(tree.dir(aaa).node), b"aaa");
        assert_eq!(tree.name(tree.dir(zzz).node), b"zzz");

        // Before: first-seen attribution counted the 1000/1024 snapshot
        // under zzz. Subtree-aggregate consistency holds (root = Σ children).
        assert_eq!(
            (tree.dir(aaa).ta, tree.dir(aaa).td, tree.dir(aaa).tn),
            (4096, 4096, 1)
        );
        assert_eq!(
            (tree.dir(zzz).ta, tree.dir(zzz).td, tree.dir(zzz).tn),
            (4096 + 1000, 4096 + 1024, 2)
        );
        let root_before = (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn);
        assert_eq!(tree.dir(root).ta, tree.dir(aaa).ta + tree.dir(zzz).ta);
        assert_eq!(tree.dir(root).td, tree.dir(aaa).td + tree.dir(zzz).td);

        assert_eq!(reattribute(&mut tree, &links), 1);

        // After: the canonical link0's 2000/2048 snapshot is the group's
        // ground truth; zzz keeps only its own directory inode.
        assert_eq!(
            (tree.dir(aaa).ta, tree.dir(aaa).td, tree.dir(aaa).tn),
            (4096 + 2000, 4096 + 2048, 2)
        );
        assert_eq!(
            (tree.dir(zzz).ta, tree.dir(zzz).td, tree.dir(zzz).tn),
            (4096, 4096, 1)
        );

        // The root total *shifted* by (canonical - first-seen) — the
        // invariant is not "root unchanged" once the snapshots disagree.
        // The link count nets to zero (one link out of zzz, one into aaa).
        assert_eq!(
            (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn),
            (
                root_before.0 + (2000 - 1000),
                root_before.1 + (2048 - 1024),
                root_before.2,
            )
        );

        // Crucially, parent = sum-of-children still holds at the root for
        // the size aggregates: the delta rode up the two chains, it was
        // never patched in at the top. (`tn` counts every real entry,
        // hardlink extras included, so it is not a naive child-sum — that
        // is orthogonal to the size invariant F2 is about.)
        assert_eq!(tree.dir(root).ta, tree.dir(aaa).ta + tree.dir(zzz).ta);
        assert_eq!(tree.dir(root).td, tree.dir(aaa).td + tree.dir(zzz).td);
    }

    #[test]
    fn group_links_groups_by_inode_identity() {
        let (tree, _, _, _, links) = scan_with_wrong_first_seen();
        let groups = group_links(&links);
        assert_eq!(groups.len(), 1, "one nlink>1 inode");
        let group = &groups[&(1, 42)];
        assert_eq!(group.nlink, 2);
        assert_eq!(group.nodes.len(), 2);
        let mut names: Vec<_> = group.nodes.iter().map(|&n| tree.name(n)).collect();
        names.sort_unstable();
        assert_eq!(names, [b"link0", b"link1"]);
        // The D4 subset check's other half: the scan saw every link.
        assert_eq!(group.nodes.len() as u32, group.nlink);
    }

    #[test]
    fn group_links_on_an_empty_registry_is_empty() {
        assert!(group_links(&[]).is_empty());
    }

    #[test]
    fn reattribution_is_idempotent() {
        let (mut tree, root, aaa, zzz, links) = scan_with_wrong_first_seen();
        assert_eq!(reattribute(&mut tree, &links), 1);
        let snapshot = |tree: &Tree| {
            [root, aaa, zzz].map(|d| {
                let m = tree.dir(d);
                (m.ta, m.td, m.tn, m.te)
            })
        };
        let after_first = snapshot(&tree);
        assert_eq!(reattribute(&mut tree, &links), 0, "second run is a no-op");
        assert_eq!(snapshot(&tree), after_first);
    }

    #[test]
    fn same_directory_links_flip_flags_without_moving_totals() {
        // Both links in the root; readdir gave them in reverse name order.
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, Arc::default());
        owner.handle_batch(batch(
            ROOT_TOKEN,
            vec![link_entry(b"zz", 7), link_entry(b"aa", 7)],
        ));
        let root = owner.root();
        let (mut tree, _, links) = owner.into_parts();
        let before = (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn);
        assert_eq!(reattribute(&mut tree, &links), 1);
        assert_eq!(
            (tree.dir(root).ta, tree.dir(root).td, tree.dir(root).tn),
            before
        );
        let aa = links.iter().find(|l| tree.name(l.node) == b"aa").unwrap();
        assert!(
            !tree
                .node(aa.node)
                .flags()
                .contains(NodeFlags::HARDLINK_EXTRA),
            "canonical link is the counted one"
        );
    }

    #[test]
    fn component_wise_comparison_beats_whole_string() {
        // Whole-string bytes would order "foo.bar" (0x2E) before "foo/x"
        // (0x2F); component-wise, "foo" < "foo.bar" so foo/x wins.
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, Arc::default());
        owner.handle_batch(batch(
            ROOT_TOKEN,
            vec![link_entry(b"foo.bar", 9), dir_entry(b"foo", 1)],
        ));
        owner.handle_batch(batch(1, vec![link_entry(b"x", 9)]));
        assert!(owner.root_complete());
        let (mut tree, _, links) = owner.into_parts();
        reattribute(&mut tree, &links);
        let x = links.iter().find(|l| tree.name(l.node) == b"x").unwrap();
        assert!(
            !tree
                .node(x.node)
                .flags()
                .contains(NodeFlags::HARDLINK_EXTRA),
            "foo/x is canonical over foo.bar"
        );
    }
}
