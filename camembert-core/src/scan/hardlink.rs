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
//! subtract along one chain, add along the other. Global (root) totals are
//! unchanged by construction — both chains end at the root.

use rustc_hash::FxHashMap;

use crate::tree::{DirId, NodeFlags, NodeId, Tree};

/// Side record for one `nlink > 1` non-directory link. The packed 32-byte
/// [`crate::tree::Node`] has no room for `ino`/`dev`/`nlink`, so the owner
/// keeps them here (also consumed by the dump writer for the `i`/`l`
/// entry fields).
#[derive(Debug, Clone, Copy)]
pub(crate) struct HardlinkLink {
    pub node: NodeId,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
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
            // Subtract what the first-seen link contributed, add the
            // canonical link's own recorded sizes (same inode, so they
            // normally agree; using each node's own values keeps every
            // directory total consistent with its entry lines).
            let sub = tree.node(counted).size();
            let add = tree.node(canonical).size();
            tree.retract_delta(old_chain, sub.apparent, sub.real, 1);
            tree.apply_delta(new_chain, add.apparent, add.real, 1, 0);
        }
        tree.set_hardlink_extra(counted, true);
        tree.set_hardlink_extra(canonical, false);
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
        let mut e = entry(name, Kind::File, 1000, 1024);
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
