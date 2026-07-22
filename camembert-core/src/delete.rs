//! Safe deletion executor (HANDOFF §5 "Garde-fous de suppression").
//!
//! Takes a frozen post-scan [`ScanOutcome`] plus a list of marked nodes,
//! deletes them from disk with defense-in-depth guards, and keeps the
//! tree's accounting honest through [`ScanOutcome::apply_removal`].
//!
//! # Guards (per entry, re-checked here even when the UI already refused)
//!
//! 1. **Never outside the scan root**: the path is rebuilt from the tree
//!    ([`crate::tree::Tree::path_of_node`]), never taken from user input,
//!    and must be strictly under the scanned root (the root itself is
//!    refused).
//! 2. **Never an excluded mount point** ([`NodeFlags::EXCLUDED`]): its
//!    subtree was never scanned; refused at mark time and again here.
//! 3. **The filesystem may have changed since the scan**: a fresh
//!    `symlink_metadata` (which does not follow the final component) must
//!    still see the entry, its file type must match the tree's record,
//!    and a directory's device must match the scanned `st_dev` (a mount
//!    that appeared underneath since the scan is refused). On any
//!    mismatch the entry is skipped with a per-entry note — never
//!    deleted.
//! 4. **Symlinks are deleted, never followed**: symlink entries go
//!    through `remove_file` on the link itself, and `remove_dir_all`
//!    (Rust ≥ 1.62) deletes directory contents via `openat`-style
//!    traversal that does not follow symlinks. Remaining known limitation
//!    (documented, accepted for this increment): an *intermediate* path
//!    component replaced by a symlink between the check and the delete is
//!    a TOCTOU window; closing it needs a fully descriptor-relative
//!    (`openat` + `unlinkat`) walk — a later increment.
//!
//! Failures (`EACCES`, `ENOENT`, …) never abort the batch: each entry
//! gets its own [`EntryOutcome`], failures are traced, and the report
//! carries the tally for the UI footer.
//!
//! # Later increments (left as design notes, per HANDOFF §5)
//!
//! - **Open-file warning**: before deleting, scan `/proc/*/fd` for open
//!   descriptors on the marked paths and warn with the guilty PID —
//!   deleting an open file frees no space until the process closes it
//!   (the classic `df` full / `du` empty). Shares the code the future
//!   "libérable" column needs.
//! - **XDG Trash**: optional trash-spec move instead of `unlink`, so a
//!   deletion can be undone. Off by default (servers rarely have a
//!   trash), flag-gated when it lands.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashSet;
use tracing::{debug, info, warn};

use crate::scan::ScanOutcome;
use crate::size::Size;
use crate::tree::{Kind, NodeFlags, NodeId, RemovalDelta, RemovalError};

/// Why an entry was skipped without touching the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Excluded mount point ([`NodeFlags::EXCLUDED`]) — never deletable.
    MountPoint,
    /// The rebuilt path is the scan root or escapes it.
    OutsideRoot,
    /// The on-disk file type no longer matches the tree's record (the
    /// filesystem changed since the scan).
    KindChanged,
    /// The directory's device differs from the scanned `st_dev` (a mount
    /// appeared here since the scan).
    DeviceChanged,
}

/// Per-entry result of a deletion batch.
#[derive(Debug)]
pub enum EntryOutcome {
    /// Removed from disk and subtracted from the tree.
    Deleted {
        /// What the removal subtracted from the aggregates (0 for a
        /// hardlink extra — it never contributed).
        freed: RemovalDelta,
    },
    /// Already tombstoned when its turn came: a marked ancestor directory
    /// earlier in the batch deleted it. Counted as deleted.
    Contained,
    /// A guard refused; nothing was touched on disk or in the tree.
    Skipped(SkipReason),
    /// The filesystem operation failed; nothing was removed from the
    /// tree (`remove_dir_all` may have partially emptied a directory —
    /// the aggregates then overestimate, which is the safe direction).
    Failed(io::Error),
}

/// One entry of a [`DeleteReport`].
#[derive(Debug)]
pub struct EntryResult {
    pub node: NodeId,
    /// The path as rebuilt from the tree (what was acted on).
    pub path: PathBuf,
    pub outcome: EntryOutcome,
}

/// Outcome of a whole deletion batch.
#[derive(Debug, Default)]
pub struct DeleteReport {
    /// Per-entry outcomes, in execution (shallowest-first) order.
    pub results: Vec<EntryResult>,
    /// Entries removed from disk (including [`EntryOutcome::Contained`]).
    pub deleted: u64,
    /// Entries whose filesystem operation failed.
    pub failed: u64,
    /// Entries refused by a guard.
    pub skipped: u64,
    /// Total subtracted from the tree's aggregates. For hardlinks with
    /// surviving links elsewhere this overestimates what the filesystem
    /// actually freed (see [`hardlink_files_in`]).
    pub freed: Size,
}

/// Delete the given nodes from disk, shallowest path first (so a marked
/// directory removes its marked descendants as [`EntryOutcome::Contained`]
/// instead of racing them), applying every guard in the module docs.
pub fn delete_nodes(outcome: &mut ScanOutcome, nodes: &[NodeId]) -> DeleteReport {
    let root = outcome.root_path().to_path_buf();
    let mut ordered: Vec<(NodeId, PathBuf)> = nodes
        .iter()
        .map(|&node| (node, outcome.tree().path_of_node(node)))
        .collect();
    ordered.sort_by(|(_, a), (_, b)| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });

    let mut report = DeleteReport::default();
    for (node, path) in ordered {
        let entry_outcome = delete_one(outcome, node, &path, &root);
        match &entry_outcome {
            EntryOutcome::Deleted { freed } => {
                report.deleted += 1;
                report.freed.apparent += freed.apparent;
                report.freed.real += freed.disk;
                debug!(path = %path.display(), freed = freed.disk, "deleted");
            }
            EntryOutcome::Contained => {
                report.deleted += 1;
                debug!(path = %path.display(), "already removed by a marked ancestor");
            }
            EntryOutcome::Skipped(reason) => {
                report.skipped += 1;
                warn!(path = %path.display(), ?reason, "deletion skipped");
            }
            EntryOutcome::Failed(err) => {
                report.failed += 1;
                warn!(path = %path.display(), %err, "deletion failed");
            }
        }
        report.results.push(EntryResult {
            node,
            path,
            outcome: entry_outcome,
        });
    }
    info!(
        deleted = report.deleted,
        failed = report.failed,
        skipped = report.skipped,
        freed_disk = report.freed.real,
        "deletion batch done"
    );
    report
}

fn delete_one(outcome: &mut ScanOutcome, node: NodeId, path: &Path, root: &Path) -> EntryOutcome {
    let tree = outcome.tree();
    if tree.is_removed(node) {
        return EntryOutcome::Contained;
    }
    let record = tree.node(node);
    let kind = record.kind();
    if record.flags().contains(NodeFlags::EXCLUDED) {
        return EntryOutcome::Skipped(SkipReason::MountPoint);
    }
    if path == root || !path.starts_with(root) {
        return EntryOutcome::Skipped(SkipReason::OutsideRoot);
    }
    // Fresh look at the entry itself, without following a symlink.
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) => return EntryOutcome::Failed(err),
    };
    if !kind_matches(kind, meta.file_type()) {
        return EntryOutcome::Skipped(SkipReason::KindChanged);
    }
    if kind.is_dir()
        && let Some(dir) = tree.dir_of(node)
    {
        use std::os::unix::fs::MetadataExt;
        if tree.dir(dir).dev != meta.dev() {
            return EntryOutcome::Skipped(SkipReason::DeviceChanged);
        }
    }

    let result = if kind.is_dir() {
        fs::remove_dir_all(path)
    } else {
        // Files, symlinks (the link itself), and device/fifo/socket nodes.
        fs::remove_file(path)
    };
    if let Err(err) = result {
        return EntryOutcome::Failed(err);
    }
    match outcome.apply_removal(node) {
        Ok(delta) => EntryOutcome::Deleted { freed: delta },
        // Unreachable after the is_removed check above, but never panic in
        // a deletion path: the disk entry is gone, report it as contained.
        Err(RemovalError::AlreadyRemoved) => EntryOutcome::Contained,
        Err(err) => {
            debug!(path = %path.display(), %err, "post-delete accounting refused");
            EntryOutcome::Contained
        }
    }
}

/// Whether the scanned kind still matches the on-disk file type. For
/// [`Kind::Other`] (kind unknown at scan time) anything non-directory is
/// accepted — the deletion path for non-dirs (`remove_file`) is safe for
/// any of them, while treating an unknown as a dir never is.
fn kind_matches(kind: Kind, ft: fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    match kind {
        Kind::Dir => ft.is_dir(),
        Kind::File => ft.is_file(),
        Kind::Symlink => ft.is_symlink(),
        Kind::Block => ft.is_block_device(),
        Kind::Char => ft.is_char_device(),
        Kind::Fifo => ft.is_fifo(),
        Kind::Socket => ft.is_socket(),
        Kind::Other => !ft.is_dir(),
    }
}

/// Count the distinct hardlinked files (links of `nlink > 1` inodes —
/// counted firsts and extras alike) among `nodes`, descending into marked
/// directories. Feeds the confirmation dialog's warning: deleting such
/// entries frees space only when *all* links to the inode are deleted;
/// exact freeable math is the future "libérable" column.
pub fn hardlink_files_in(outcome: &ScanOutcome, nodes: &[NodeId]) -> u64 {
    let tree = outcome.tree();
    let mut found: FxHashSet<NodeId> = FxHashSet::default();
    for &node in nodes {
        match tree.dir_of(node) {
            None => {
                if tree.is_hardlink(node) {
                    found.insert(node);
                }
            }
            Some(dir) => {
                let mut stack = vec![dir];
                while let Some(d) = stack.pop() {
                    for child in tree.children(d) {
                        match tree.dir_of(child) {
                            Some(child_dir) => stack.push(child_dir),
                            None => {
                                if tree.is_hardlink(child) {
                                    found.insert(child);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    found.len() as u64
}
