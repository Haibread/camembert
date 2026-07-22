//! The owner: single writer of the arena tree (D1).
//!
//! Receives pre-summed sections from the workers over the one bounded
//! channel and integrates them: node creation, run appends, hardlink
//! dedup, batched aggregation up the ancestor chain, and the completion
//! cascade. Nothing else ever mutates the [`Tree`].
//!
//! # Cost budget (honest, amendment 7)
//!
//! Integration targets ~100–180 ns/entry on the owner thread (intern +
//! node push + registry probe for `nlink > 1`), i.e. a ceiling around
//! 8–10 M entries/s. Accepted: cold-cache scans are the priority regime
//! and the storage, not the owner, is the bottleneck there.

use std::collections::VecDeque;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tracing::{debug, warn};

use crate::size::Size;
use crate::tree::{ChildRun, DirId, DirState, ExcludedReason, Kind, NodeFlags, NodeId, Tree};

use super::ScanProgress;
use super::message::Batch;

/// Token of the scan root (workers allocate from 1 upward).
pub(crate) const ROOT_TOKEN: u64 = 0;

/// Cap (in buffered *entries*) of the out-of-order holding map.
///
/// Policy (binding amendment "bounded holding map", implemented simply):
/// batches whose directory token is not yet known — the child was scanned
/// before the parent section that discovered it was integrated, which work
/// stealing makes legal — wait in `holding`, a token-keyed map, accounted
/// by entry count against this cap. Overflow goes to `spill`, a plain
/// unordered `Vec` retried after every integration wave. The owner never
/// stops draining the channel: the discovering parent section is *behind*
/// the held batch in the channel by construction, so refusing to receive
/// would deadlock; the channel's own bound (backpressure on the workers)
/// is what limits total in-flight data.
const HOLDING_CAP_ENTRIES: usize = 512 * 1024;

/// First-seen hardlink registry entry.
///
/// Canonical re-attribution (dump rule: the owner is the link with the
/// smallest path under the raw-byte comparator) is a LATER increment, run
/// off the owner's critical path overlapped with finalize (D3). For now
/// live totals use first-seen attribution: the first link of a
/// `(dev, ino)` counts, later links contribute 0 and carry
/// [`NodeFlags::HARDLINK_EXTRA`].
#[allow(dead_code, reason = "consumed by the future re-attribution increment")]
struct HardlinkSeen {
    first: NodeId,
    /// Whether the first-seen link was counted in the aggregates (always
    /// true in this increment; kept for the re-attribution pass).
    counted: bool,
}

pub(crate) struct Owner {
    tree: Tree,
    root: DirId,
    /// Worker token → integrated directory.
    tokens: FxHashMap<u64, DirId>,
    /// Out-of-order batches waiting for their token (see
    /// [`HOLDING_CAP_ENTRIES`]).
    holding: FxHashMap<u64, Vec<Batch>>,
    holding_entries: usize,
    /// Overflow beyond the holding cap: plain unordered buffer.
    spill: Vec<Batch>,
    /// `(dev, ino)` → first-seen link, for `nlink > 1` non-directories.
    hardlinks: FxHashMap<(u64, u64), HardlinkSeen>,
    /// Later links to an already-seen inode (nodes flagged
    /// `HARDLINK_EXTRA`).
    hardlink_extra_links: u64,
    excluded_dirs: u64,
    /// Subset of `excluded_dirs` that are kernel pseudo-filesystems.
    excluded_kernfs: u64,
    progress: Arc<ScanProgress>,
}

impl Owner {
    /// Build the owner with the root directory already in the arena. The
    /// root's own stat is done by the scanner before workers start.
    pub(crate) fn new(
        root_name: &[u8],
        root_size: Size,
        root_mtime: i64,
        root_dev: u64,
        progress: Arc<ScanProgress>,
    ) -> Self {
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(root_name, root_size, root_mtime);
        let root = tree.add_dir(root_node, None, root_dev);
        let mut tokens = FxHashMap::default();
        tokens.insert(ROOT_TOKEN, root);
        Self {
            tree,
            root,
            tokens,
            holding: FxHashMap::default(),
            holding_entries: 0,
            spill: Vec::new(),
            hardlinks: FxHashMap::default(),
            hardlink_extra_links: 0,
            excluded_dirs: 0,
            excluded_kernfs: 0,
            progress,
        }
    }

    pub(crate) fn tree(&self) -> &Tree {
        &self.tree
    }

    pub(crate) fn root(&self) -> DirId {
        self.root
    }

    pub(crate) fn root_complete(&self) -> bool {
        self.tree.dir(self.root).state != DirState::Scanning
    }

    pub(crate) fn excluded_dirs(&self) -> u64 {
        self.excluded_dirs
    }

    pub(crate) fn excluded_kernfs(&self) -> u64 {
        self.excluded_kernfs
    }

    pub(crate) fn hardlink_inodes(&self) -> u64 {
        self.hardlinks.len() as u64
    }

    pub(crate) fn hardlink_extra_links(&self) -> u64 {
        self.hardlink_extra_links
    }

    pub(crate) fn into_tree(self) -> (Tree, DirId) {
        (self.tree, self.root)
    }

    /// Handle one batch from the channel: integrate it if its directory is
    /// known, hold it otherwise, then drain everything that integration
    /// just unblocked.
    pub(crate) fn handle_batch(&mut self, batch: Batch) {
        let Some(&dir) = self.tokens.get(&batch.dir_token) else {
            self.hold(batch);
            return;
        };
        let mut queue = VecDeque::new();
        queue.push_back((dir, batch));
        while let Some((dir, batch)) = queue.pop_front() {
            let new_tokens = self.integrate(dir, batch);
            // Batches held for tokens this section just registered are now
            // integrable (and may themselves register more tokens).
            for token in new_tokens {
                if let Some(held) = self.holding.remove(&token) {
                    let dir = self.tokens[&token];
                    for batch in held {
                        self.holding_entries -= batch.entries.len();
                        queue.push_back((dir, batch));
                    }
                }
            }
            // Retry the spill: cheap linear pass, only entered when the
            // holding cap was blown.
            if !self.spill.is_empty() {
                let mut i = 0;
                while i < self.spill.len() {
                    match self.tokens.get(&self.spill[i].dir_token) {
                        Some(&dir) => {
                            let batch = self.spill.swap_remove(i);
                            queue.push_back((dir, batch));
                        }
                        None => i += 1,
                    }
                }
            }
        }
    }

    fn hold(&mut self, batch: Batch) {
        let entries = batch.entries.len();
        if self.holding_entries + entries > HOLDING_CAP_ENTRIES {
            // See HOLDING_CAP_ENTRIES for the policy.
            warn!(
                token = batch.dir_token,
                held = self.holding_entries,
                "holding map full, spilling out-of-order batch"
            );
            self.spill.push(batch);
            return;
        }
        self.holding_entries += entries;
        self.holding.entry(batch.dir_token).or_default().push(batch);
    }

    /// Integrate one section into the tree. Returns the tokens of child
    /// directories registered by this section.
    fn integrate(&mut self, dir: DirId, batch: Batch) -> Vec<u64> {
        let mut new_tokens = Vec::new();

        if let Some(errno) = batch.dir_error {
            // Unreadable directory: state Error, counted in te up the
            // chain ("comptabiliser l'illisible"), then completed like any
            // other dir so the cascade proceeds.
            debug!(?dir, %errno, "directory unreadable");
            self.tree.mark_error(dir);
            self.tree.apply_delta(dir, 0, 0, 0, 1);
            self.progress.add_errors(1);
            self.tree.release_token(dir);
            return new_tokens;
        }

        let dir_node = self.tree.dir(dir).node;
        let dir_dev = self.tree.dir(dir).dev;
        let run_start = u32::try_from(self.tree.node_count()).expect("node arena exceeds u32");
        let run_len = u32::try_from(batch.entries.len()).expect("section exceeds u32");

        // Hardlink dedup: the worker pre-sums blindly; subtract every
        // later link of an already-seen inode from the section delta.
        let mut dup = Size::default();
        let mut dup_count: u64 = 0;
        let mut child_dirs_seen: u32 = 0;

        for entry in batch.entries {
            let mut flags = NodeFlags::default();
            if entry.error {
                flags.insert(NodeFlags::ERROR);
            }
            if entry.excluded.is_some() {
                flags.insert(NodeFlags::EXCLUDED);
                self.excluded_dirs += 1;
                if entry.excluded == Some(ExcludedReason::KernFs) {
                    self.excluded_kernfs += 1;
                }
            }

            let size = Size {
                apparent: entry.apparent,
                real: entry.disk,
            };
            let is_extra_link = entry.kind != Kind::Dir
                && entry.nlink > 1
                && self.hardlinks.contains_key(&(entry.dev, entry.ino));
            if is_extra_link {
                flags.insert(NodeFlags::HARDLINK_EXTRA);
                dup.add(size);
                dup_count += 1;
                self.hardlink_extra_links += 1;
            }

            let node =
                self.tree
                    .push_node(&entry.name, entry.kind, flags, dir_node, size, entry.mtime);

            if let Some(reason) = entry.excluded {
                self.tree.set_excluded(node, reason);
            }

            if entry.kind != Kind::Dir && entry.nlink > 1 && !is_extra_link {
                self.hardlinks.insert(
                    (entry.dev, entry.ino),
                    HardlinkSeen {
                        first: node,
                        counted: true,
                    },
                );
            }

            if let Some(token) = entry.child_token {
                let dev = if entry.dev != 0 { entry.dev } else { dir_dev };
                let child = self.tree.add_dir(node, Some(dir), dev);
                self.tokens.insert(token, child);
                new_tokens.push(token);
                child_dirs_seen += 1;
            }
        }
        debug_assert_eq!(child_dirs_seen, batch.child_dirs, "child_dirs miscount");

        self.tree.push_run(
            dir,
            ChildRun {
                start: run_start,
                len: run_len,
            },
        );

        // Batched aggregation: one plain-add walk up the chain per section
        // (D1 graft), with hardlink extras removed.
        let da = batch.sums.apparent - dup.apparent;
        let dd = batch.sums.disk - dup.real;
        let dn = batch.sums.count - dup_count;
        self.tree.apply_delta(dir, da, dd, dn, batch.sums.errors);

        self.progress.add_entries(batch.sums.count);
        self.progress.add_disk_bytes(dd);
        self.progress.add_errors(u64::from(batch.sums.errors));
        self.progress.add_dirs(u64::from(batch.child_dirs));

        if batch.is_last_section {
            // Self-token release. Invariant (binding amendment): this must
            // also gate on outstanding-statx == 0 once stats become
            // asynchronous (io_uring path); in this thread-pool
            // implementation every stat result is already inside a section
            // when the last section arrives, so is_last_section suffices.
            self.tree.release_token(dir);
        }
        new_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::message::{BatchEntry, SectionSums};

    fn progress() -> Arc<ScanProgress> {
        Arc::new(ScanProgress::default())
    }

    fn file_entry(name: &[u8], apparent: u64, disk: u64) -> BatchEntry {
        BatchEntry {
            name: name.to_vec(),
            kind: Kind::File,
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

    fn dir_entry(name: &[u8], token: u64) -> BatchEntry {
        BatchEntry {
            name: name.to_vec(),
            kind: Kind::Dir,
            apparent: 4096,
            disk: 4096,
            mtime: 0,
            nlink: 2,
            ino: 0,
            dev: 1,
            error: false,
            child_token: Some(token),
            excluded: None,
        }
    }

    fn batch(token: u64, entries: Vec<BatchEntry>, is_last: bool) -> Batch {
        let mut sums = SectionSums::default();
        let mut child_dirs = 0;
        for e in &entries {
            sums.apparent += e.apparent;
            sums.disk += e.disk;
            sums.count += 1;
            if e.error {
                sums.errors += 1;
            }
            if e.child_token.is_some() {
                child_dirs += 1;
            }
        }
        Batch {
            dir_token: token,
            entries,
            sums,
            is_last_section: is_last,
            child_dirs,
            dir_error: None,
        }
    }

    #[test]
    fn in_order_integration_completes_root() {
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, progress());
        owner.handle_batch(batch(
            ROOT_TOKEN,
            vec![file_entry(b"a", 100, 512), file_entry(b"b", 50, 512)],
            true,
        ));
        assert!(owner.root_complete());
        let root = owner.root();
        let meta = owner.tree().dir(root);
        assert_eq!(meta.state, DirState::Complete);
        assert_eq!(meta.ta, 150);
        assert_eq!(meta.td, 1024);
        assert_eq!(meta.tn, 3); // root + 2 files
    }

    #[test]
    fn out_of_order_child_batch_is_held_then_integrated() {
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, progress());

        // Child's batch arrives BEFORE the root section that discovers it
        // (work stealing makes this legal).
        owner.handle_batch(batch(1, vec![file_entry(b"f", 100, 512)], true));
        assert!(!owner.root_complete());
        assert_eq!(owner.tree().node_count(), 1); // only the root node
        assert_eq!(owner.holding_entries, 1);

        // Root's (last) section discovers child dir "sub" with token 1:
        // the held batch drains, the child completes, root cascades.
        owner.handle_batch(batch(ROOT_TOKEN, vec![dir_entry(b"sub", 1)], true));
        assert!(owner.root_complete());
        assert_eq!(owner.holding_entries, 0);
        assert!(owner.holding.is_empty());

        let root = owner.root();
        let root_meta = owner.tree().dir(root);
        assert_eq!(root_meta.state, DirState::Complete);
        assert_eq!(root_meta.ta, 4096 + 100);
        assert_eq!(root_meta.td, 4096 + 512);
        assert_eq!(root_meta.tn, 3); // root + sub + f

        let sub_node = owner.tree().children(root).next().unwrap();
        let sub = owner.tree().dir_of(sub_node).unwrap();
        let sub_meta = owner.tree().dir(sub);
        assert_eq!(sub_meta.state, DirState::Complete);
        assert_eq!(sub_meta.ta, 4096 + 100);
        assert_eq!(sub_meta.tn, 2);
    }

    #[test]
    fn deep_out_of_order_chain_drains_recursively() {
        // grandchild and child batches both arrive before root's.
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, progress());
        owner.handle_batch(batch(2, vec![file_entry(b"deep", 7, 512)], true));
        owner.handle_batch(batch(1, vec![dir_entry(b"mid", 2)], true));
        assert!(!owner.root_complete());
        owner.handle_batch(batch(ROOT_TOKEN, vec![dir_entry(b"top", 1)], true));
        assert!(owner.root_complete());
        let root_meta = owner.tree().dir(owner.root());
        assert_eq!(root_meta.ta, 4096 + 4096 + 7);
        assert_eq!(root_meta.tn, 4);
        assert_eq!(root_meta.te, 0);
    }

    #[test]
    fn multi_section_giant_completes_only_on_last_section() {
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, progress());
        owner.handle_batch(batch(ROOT_TOKEN, vec![file_entry(b"s1", 10, 512)], false));
        assert!(!owner.root_complete());
        owner.handle_batch(batch(ROOT_TOKEN, vec![file_entry(b"s2", 20, 512)], true));
        assert!(owner.root_complete());
        let meta = owner.tree().dir(owner.root());
        assert_eq!(meta.ta, 30);
        assert_eq!(meta.runs().len(), 2);
    }

    #[test]
    fn unreadable_dir_counts_te_and_still_completes() {
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, progress());
        owner.handle_batch(batch(ROOT_TOKEN, vec![dir_entry(b"locked", 1)], true));
        assert!(!owner.root_complete());
        owner.handle_batch(Batch {
            dir_token: 1,
            entries: Vec::new(),
            sums: SectionSums::default(),
            is_last_section: true,
            child_dirs: 0,
            dir_error: Some(rustix::io::Errno::ACCESS),
        });
        assert!(owner.root_complete());
        let root_meta = owner.tree().dir(owner.root());
        assert_eq!(root_meta.te, 1);
        let locked_node = owner.tree().children(owner.root()).next().unwrap();
        let locked = owner.tree().dir_of(locked_node).unwrap();
        assert_eq!(owner.tree().dir(locked).state, DirState::Error);
    }

    #[test]
    fn hardlink_second_link_contributes_zero() {
        let mut owner = Owner::new(b"/r", Size::default(), 0, 1, progress());
        let mut first = file_entry(b"one", 500, 512);
        first.nlink = 2;
        first.ino = 99;
        let mut second = file_entry(b"two", 500, 512);
        second.nlink = 2;
        second.ino = 99;
        owner.handle_batch(batch(ROOT_TOKEN, vec![first, second], true));
        let meta = owner.tree().dir(owner.root());
        assert_eq!(meta.ta, 500, "inode counted once");
        assert_eq!(meta.td, 512);
        assert_eq!(meta.tn, 2, "root + one inode");
        assert_eq!(owner.hardlink_inodes(), 1);
        assert_eq!(owner.hardlink_extra_links(), 1);
        let flagged: Vec<bool> = owner
            .tree()
            .children(owner.root())
            .map(|id| {
                owner
                    .tree()
                    .node(id)
                    .flags()
                    .contains(NodeFlags::HARDLINK_EXTRA)
            })
            .collect();
        assert_eq!(flagged, [false, true]);
    }
}
