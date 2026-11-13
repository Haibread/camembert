//! Grafting a rescan of one directory into a finished scan
//! (`docs/design/subtree-rescan-decisions.md` D1 and D5).

use rustc_hash::{FxHashMap, FxHashSet};
use tracing::debug;

use super::ScanOutcome;
use super::hardlink::{self, HardlinkLink};
use crate::size::Size;
use crate::tree::{DirId, NodeFlags, NodeId, Tree};

/// Inodes a rescan must treat as already reached elsewhere.
///
/// Only meaningful when the session scan did not obtain link counts (the
/// Windows default, `LinkPolicy::FileIdRepeat`): there a link enters the
/// registry on its *second* sighting, so a subtree scan meeting an inode
/// whose other link lives outside the subtree would count it as a plain
/// file. Seeding the owner with those inodes makes the first sighting
/// count as the repeat it is. Empty, and ignored, everywhere `nlink > 1`
/// registers links on its own.
#[derive(Debug, Clone, Default)]
pub struct LinkSeed {
    pub(crate) inodes: FxHashSet<(u64, u64)>,
}

impl LinkSeed {
    pub fn is_empty(&self) -> bool {
        self.inodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inodes.len()
    }
}

/// Why [`ScanOutcome::graft_rescan`] refused. Nothing changed in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GraftError {
    #[error("the directory is no longer in the tree")]
    Removed,
    #[error("the rescan was cancelled")]
    Cancelled,
}

/// What a graft changed, for the UI's toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraftReport {
    /// The directory's subtree totals before and after.
    pub before: Size,
    pub after: Size,
    /// Hardlink groups whose counted link was re-chosen because they
    /// straddle the graft boundary or lost links to it.
    pub relinked_groups: usize,
    /// The rescan was of the scan root: the whole outcome was replaced and
    /// every `DirId`/`NodeId` from before is meaningless.
    pub replaced_whole: bool,
}

impl ScanOutcome {
    /// The [`LinkSeed`] a rescan of `dir` needs: every registered inode
    /// with a live link outside `dir`. Empty when the scan knows link
    /// counts, since those scans register multi-link inodes by themselves.
    pub fn rescan_link_seed(&self, dir: DirId) -> LinkSeed {
        if self.link_counts_known {
            return LinkSeed::default();
        }
        let tree = &self.tree;
        let dir_node = tree.dir(dir).node;
        let mut inodes = FxHashSet::default();
        for link in &self.hardlink_links {
            if !tree.is_removed(link.node) && !is_under(tree, link.node, dir_node) {
                inodes.insert((link.dev, link.ino));
            }
        }
        LinkSeed { inodes }
    }

    /// Replace `dir`'s subtree with `fresh`, a scan of the same directory
    /// (D1). Hardlink groups touched by the graft are re-canonicalised over
    /// their live links, so each inode is counted once, at its canonical
    /// link, exactly as a full scan of the same state would count it — for
    /// every link the original scan registered. The summary counters are
    /// refreshed from the tree.
    ///
    /// A rescan of the root replaces the whole outcome instead, so a full
    /// rescan never doubles the arena (`GraftReport::replaced_whole`).
    ///
    /// Post-scan only: the caller is the frozen arena's single writer.
    pub fn graft_rescan(
        &mut self,
        dir: DirId,
        mut fresh: ScanOutcome,
    ) -> Result<GraftReport, GraftError> {
        if fresh.cancelled {
            return Err(GraftError::Cancelled);
        }
        let dir_node = self.tree.dir(dir).node;
        if self.tree.is_removed(dir_node) {
            return Err(GraftError::Removed);
        }
        let before = subtree_size(&self.tree, dir);
        fresh.finalize_hardlinks();

        if dir == self.root {
            let after = fresh.totals;
            // The session's options and timing describe how it was scanned;
            // the fresh outcome was produced with the same options.
            *self = fresh;
            return Ok(GraftReport {
                before,
                after,
                relinked_groups: 0,
                replaced_whole: true,
            });
        }

        let fresh_root = fresh.root;
        let graft = self.tree.graft(dir, &fresh.tree, fresh_root);

        let tombstoned: FxHashSet<NodeId> = graft.tombstoned.iter().copied().collect();
        // Per touched inode: how many of its links the graft tombstoned.
        let mut removed: FxHashMap<(u64, u64), u32> = FxHashMap::default();
        self.hardlink_links.retain(|link| {
            if tombstoned.contains(&link.node) {
                *removed.entry((link.dev, link.ino)).or_default() += 1;
                false
            } else {
                true
            }
        });
        // The surviving outside links' `nlink` was stat'ed at scan time.
        let mut recorded: FxHashMap<(u64, u64), u32> = FxHashMap::default();
        for link in &self.hardlink_links {
            let key = (link.dev, link.ino);
            if removed.contains_key(&key) && !self.tree.is_removed(link.node) {
                let nlink = recorded.entry(key).or_default();
                *nlink = (*nlink).max(link.nlink);
            }
        }
        // A fresh link's `nlink` is the current truth for its inode.
        let mut current: FxHashMap<(u64, u64), u32> = FxHashMap::default();
        for link in &fresh.hardlink_links {
            let node = graft
                .map
                .get(link.node)
                .expect("every fresh link below the root was copied");
            let nlink = current.entry((link.dev, link.ino)).or_default();
            *nlink = (*nlink).max(link.nlink);
            self.hardlink_links.push(HardlinkLink { node, ..*link });
        }
        // An inode the rescan no longer reaches lost exactly the links the
        // graft tombstoned.
        for (&key, &gone) in &removed {
            current.entry(key).or_insert_with(|| {
                recorded
                    .get(&key)
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(gone)
            });
        }

        let mut groups: FxHashMap<(u64, u64), Vec<NodeId>> = FxHashMap::default();
        for link in &mut self.hardlink_links {
            let key = (link.dev, link.ino);
            if let Some(&nlink) = current.get(&key)
                && !self.tree.is_removed(link.node)
            {
                if self.link_counts_known {
                    link.nlink = nlink;
                }
                groups.entry(key).or_default().push(link.node);
            }
        }
        let mut relinked_groups = 0;
        let mut single: FxHashSet<(u64, u64)> = FxHashSet::default();
        for (key, nodes) in &groups {
            if recanonicalize(&mut self.tree, nodes) {
                relinked_groups += 1;
            }
            // Down to one link where counts are known: a plain file now,
            // which a full scan would not register at all.
            if self.link_counts_known && nodes.len() == 1 && current[key] <= 1 {
                self.tree.unmark_hardlink_first(nodes[0]);
                single.insert(*key);
            }
        }
        if !single.is_empty() {
            self.hardlink_links
                .retain(|link| !single.contains(&(link.dev, link.ino)));
        }
        if relinked_groups > 0 {
            debug!(relinked_groups, "rescan graft re-chose counted hardlinks");
        }

        self.link_counts_known &= fresh.link_counts_known;
        self.refresh_counters();
        Ok(GraftReport {
            before,
            after: subtree_size(&self.tree, dir),
            relinked_groups,
            replaced_whole: false,
        })
    }

    /// Re-derive every summary counter from the tree and the registry.
    fn refresh_counters(&mut self) {
        let root_meta = self.tree.dir(self.root);
        self.totals = Size {
            apparent: root_meta.ta,
            real: root_meta.td,
        };
        self.entries = root_meta.tn;
        self.errors = u64::from(root_meta.te);
        self.dirs = self.tree.live_dir_count();
        (self.excluded_dirs, self.excluded_kernfs) = self.tree.live_excluded_counts();

        let mut per_inode: FxHashMap<(u64, u64), u64> = FxHashMap::default();
        let mut extras = 0;
        for link in &self.hardlink_links {
            if self.tree.is_removed(link.node) {
                continue;
            }
            *per_inode.entry((link.dev, link.ino)).or_default() += 1;
            if self
                .tree
                .node(link.node)
                .flags()
                .contains(NodeFlags::HARDLINK_EXTRA)
            {
                extras += 1;
            }
        }
        // Same two meanings as the field documents: any registered inode
        // when link counts are known, an inode reached by more than one
        // path in this tree otherwise.
        self.hardlink_inodes = if self.link_counts_known {
            per_inode.len() as u64
        } else {
            per_inode.values().filter(|&&links| links > 1).count() as u64
        };
        self.hardlink_extra_links = extras;
    }
}

/// Make the canonical link of one inode's live links the only counted one.
/// Returns whether anything moved.
fn recanonicalize(tree: &mut Tree, nodes: &[NodeId]) -> bool {
    let canonical = *nodes
        .iter()
        .min_by(|&&a, &&b| hardlink::cmp_paths(tree, a, b))
        .expect("a group has at least one live link");
    let mut moved = false;
    for &node in nodes {
        if node == canonical || is_extra(tree, node) {
            continue;
        }
        let size = tree.node(node).size();
        tree.retract_delta(
            hardlink::parent_dir(tree, node),
            size.apparent,
            size.real,
            1,
        );
        tree.set_hardlink_extra(node, true);
        tree.unmark_hardlink_first(node);
        moved = true;
    }
    if is_extra(tree, canonical) {
        let size = tree.node(canonical).size();
        tree.apply_delta(
            hardlink::parent_dir(tree, canonical),
            size.apparent,
            size.real,
            1,
            0,
        );
        tree.set_hardlink_extra(canonical, false);
        tree.mark_hardlink_first(canonical);
        moved = true;
    }
    moved
}

fn is_extra(tree: &Tree, node: NodeId) -> bool {
    tree.node(node).flags().contains(NodeFlags::HARDLINK_EXTRA)
}

fn is_under(tree: &Tree, node: NodeId, ancestor: NodeId) -> bool {
    let mut cur = node;
    loop {
        if cur == ancestor {
            return true;
        }
        let parent = tree.node(cur).parent();
        if parent == cur {
            return false;
        }
        cur = parent;
    }
}

fn subtree_size(tree: &Tree, dir: DirId) -> Size {
    let meta = tree.dir(dir);
    Size {
        apparent: meta.ta,
        real: meta.td,
    }
}
