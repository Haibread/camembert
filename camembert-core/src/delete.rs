//! Safe deletion executor (HANDOFF §5 "Garde-fous de suppression").
//!
//! Takes a frozen post-scan [`ScanOutcome`] plus a list of marked nodes,
//! deletes them from disk with defense-in-depth guards, and keeps the
//! tree's accounting honest through [`ScanOutcome::apply_removal`].
//!
//! # A descriptor-relative walk, not a path
//!
//! Deletion never hands a rebuilt absolute path to `remove_dir_all` /
//! `remove_file`. That older shape re-statted the path, checked its type
//! and (for directories) device, then called a *path*-based remove — a
//! TOCTOU gap: between the stat and the remove an attacker who can write
//! the parent could swap the target for a symlink (an *intermediate*
//! component) or, worse, rename a **real** same-device directory into the
//! name, which `remove_dir_all` would then recurse into and destroy.
//! `remove_dir_all`'s own `O_NOFOLLOW` traversal catches a symlink swap of
//! the top-level target but not a real-directory swap of it.
//!
//! Instead, mirroring the scan engine ([`crate::scan`]), every step here is
//! descriptor-relative:
//!
//! 1. The scan **root** is opened once by path — following a symlink *as
//!    the root argument*, exactly as the scan does, because the root is the
//!    trust anchor the user named. Nothing below it is ever followed.
//! 2. From that root fd the target's **parent** is reached component by
//!    component with `openat(.., O_DIRECTORY | O_NOFOLLOW)`. A symlink
//!    swapped into any intermediate component makes the `openat` fail
//!    rather than redirect the walk — closing the intermediate-symlink
//!    window the previous increment left open.
//! 3. The target itself is `fstatat(parent_fd, name, AT_SYMLINK_NOFOLLOW)`
//!    and identity-checked *before* anything is removed, then removed with
//!    `unlinkat(parent_fd, name, …)` — for a directory, after recursively
//!    emptying it through `openat(.., O_NOFOLLOW)` + `unlinkat`, never a
//!    reconstructed path.
//!
//! # Guards (per entry, re-checked here even when the UI already refused)
//!
//! 1. **Never outside the scan root**: the path is rebuilt from the tree
//!    ([`crate::tree::Tree::path_of_node`]) only to order the batch, decide
//!    the root refusal, and label the report — the actual delete walks from
//!    the root fd. The path must be strictly under the scanned root (the
//!    root itself is refused).
//! 2. **Never an excluded mount point** ([`NodeFlags::EXCLUDED`]): its
//!    subtree was never scanned; refused at mark time and again here.
//! 3. **The filesystem may have changed since the scan**: the fresh
//!    `fstatat` (which does not follow the final component) must still see
//!    the entry, and its file type must match the tree's record. When the
//!    caller recorded the entry's identity, the live `(dev, ino)` must
//!    still equal it ([`SkipReason::IdentityMismatch`]) — a real directory
//!    (or file) swapped into the name since the mark is refused, not
//!    deleted. For a directory the live device must additionally match the
//!    scanned `st_dev` ([`SkipReason::DeviceChanged`]: a mount that appeared
//!    underneath since the scan is refused). On any mismatch the entry is
//!    skipped with a per-entry note — never deleted.
//! 4. **Symlinks are deleted, never followed**: a symlink target is
//!    `unlinkat`'d (the link itself), and directory recursion descends only
//!    into entries that are *real* directories (`d_type`/`fstatat`), opening
//!    each with `O_NOFOLLOW`, so a symlink is unlinked in place and never
//!    traversed.
//! 5. **Stat-to-open consistency**: before recursing into a directory the
//!    freshly `openat`'d fd is `fstat`'d and its `(dev, ino)` compared to
//!    the `fstatat` result — a swap slipped in between those two syscalls is
//!    caught and refused ([`SkipReason::IdentityMismatch`]) rather than
//!    followed.
//!
//! # Residual window (documented honestly)
//!
//! What remains open, precisely:
//!
//! - **The root's own path, above the anchor**, is trusted: opening it
//!   follows a symlink root argument and does not re-verify the ancestors
//!   *above* the scan root. This is the same trust the scan itself places
//!   in the path the user named — no more, no less.
//! - **Renames strictly within the marked subtree during the recursive
//!   walk.** Because every descent is a fresh `openat(.., O_NOFOLLOW)` from
//!   the parent fd and every removal is a per-entry `unlinkat`, the walk can
//!   only ever reach inodes under the *verified* top-level inode. A
//!   subtree moved out mid-walk simply stops being found; one moved in is
//!   genuinely inside the marked tree and is deleted. Nothing outside the
//!   verified top-level inode's subtree can be reached, and no symlink is
//!   ever followed — so the consequences of a mid-walk race are bounded to
//!   the subtree the user marked.
//! - **The caller's identity anchor is captured post-scan** (at confirm
//!   time), not at scan time: a swap of the *top-level target* that happens
//!   between the scan and the moment the caller records `(dev, ino)`, and
//!   which the user then confirms, is anchored to the swapped-in inode.
//!   Even then the descriptor walk still guarantees only that exact inode's
//!   subtree, under the root, with no symlink ever followed, is touched.
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

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashSet;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir};
use rustix::io::Errno;
use tracing::{debug, info, warn};

use crate::scan::ScanOutcome;
use crate::size::Size;
use crate::tree::{Kind, NodeFlags, NodeId, RemovalDelta, RemovalError};

/// `openat` flags for every directory *below* the root: read-only,
/// directory-only (a non-dir is an error), never following a final-component
/// symlink, close-on-exec. The root itself is opened without `NOFOLLOW`
/// (see the module docs — the root argument may legitimately be a symlink).
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// getdents buffer for the recursive directory drain. One directory's worth
/// of entries is read per pass; heap-allocated (not a stack array) so deep
/// recursion does not pile large buffers onto the call stack.
const DIRENT_BUF: usize = 1 << 14;

/// Inode identity a caller recorded for a marked node — checked against the
/// live filesystem ([`SkipReason::IdentityMismatch`]) before the node is
/// touched. The tree's packed 32-byte [`crate::tree::Node`] has no room for
/// `(dev, ino)` (only directories keep their `dev`, in
/// [`crate::tree::DirMeta`]), so the executor cannot reconstruct it — the
/// caller supplies it (the UI captures it the moment it builds the delete
/// batch; see [`DeleteTarget`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeId {
    /// `st_dev` of the marked inode.
    pub dev: u64,
    /// `st_ino` of the marked inode.
    pub ino: u64,
}

/// One node to delete, optionally with the [`InodeId`] the caller recorded
/// for it. `expected: None` deletes without the identity check (still fully
/// descriptor-relative, still every other guard) — the terse
/// [`delete_nodes`] entry point builds these from bare [`NodeId`]s.
#[derive(Debug, Clone, Copy)]
pub struct DeleteTarget {
    /// The node to remove, in the frozen arena.
    pub node: NodeId,
    /// The `(dev, ino)` the caller saw for this node when it marked it, or
    /// `None` to skip the identity check.
    pub expected: Option<InodeId>,
}

impl From<NodeId> for DeleteTarget {
    fn from(node: NodeId) -> Self {
        Self {
            node,
            expected: None,
        }
    }
}

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
    /// The live `(dev, ino)` no longer matches the identity the caller
    /// recorded for this node: a different inode (e.g. a real directory
    /// renamed into the name) sits there now. Refused, never deleted.
    IdentityMismatch,
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
    /// tree (the recursive drain may have partially emptied a directory —
    /// the aggregates then overestimate, which is the safe direction).
    Failed(io::Error),
}

/// One entry of a [`DeleteReport`].
#[derive(Debug)]
pub struct EntryResult {
    pub node: NodeId,
    /// The path as rebuilt from the tree (what the entry stands for; the
    /// delete itself walked from the root fd, never this path).
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

/// Delete the given nodes from disk with no caller-recorded identity check
/// (every other guard still applies, and the walk is still fully
/// descriptor-relative). Convenience over [`delete_nodes_checked`] for
/// callers — and tests — that do not carry an [`InodeId`] per node.
pub fn delete_nodes(outcome: &mut ScanOutcome, nodes: &[NodeId]) -> DeleteReport {
    let targets: Vec<DeleteTarget> = nodes.iter().copied().map(DeleteTarget::from).collect();
    delete_nodes_checked(outcome, &targets)
}

/// Delete the given targets from disk, shallowest path first (so a marked
/// directory removes its marked descendants as [`EntryOutcome::Contained`]
/// instead of racing them), applying every guard in the module docs and,
/// where [`DeleteTarget::expected`] is set, the `(dev, ino)` identity check.
pub fn delete_nodes_checked(outcome: &mut ScanOutcome, targets: &[DeleteTarget]) -> DeleteReport {
    let root = outcome.root_path().to_path_buf();
    // The trust anchor: the scan root, opened once by path. Like the scan
    // (see `Scanner::scan_with_tick`), a symlink *as the root argument* is
    // followed; nothing below is. Held for the whole batch; every entry's
    // walk starts from this fd.
    let root_fd = rustix::fs::open(
        &root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    );
    let root_fd_ref = match &root_fd {
        Ok(fd) => Ok(fd.as_fd()),
        Err(errno) => Err(*errno),
    };

    let mut ordered: Vec<(NodeId, Option<InodeId>, PathBuf)> = targets
        .iter()
        .map(|t| (t.node, t.expected, outcome.tree().path_of_node(t.node)))
        .collect();
    ordered.sort_by(|(_, _, a), (_, _, b)| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });

    let mut report = DeleteReport::default();
    for (node, expected, path) in ordered {
        let entry_outcome = delete_one(outcome, node, expected, &path, &root, root_fd_ref);
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

fn delete_one(
    outcome: &mut ScanOutcome,
    node: NodeId,
    expected: Option<InodeId>,
    path: &Path,
    root: &Path,
    root_fd: Result<BorrowedFd<'_>, Errno>,
) -> EntryOutcome {
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

    // Resolve the target's parent descriptor-relative from the root fd.
    let root_fd = match root_fd {
        Ok(fd) => fd,
        Err(errno) => return EntryOutcome::Failed(errno_to_io(errno)),
    };
    // `path` is strictly under `root` (checked above), so stripping the
    // root leaves at least one component; the last is the target's name.
    let rel = path
        .strip_prefix(root)
        .expect("path is strictly under root by the check above");
    let mut components: Vec<&OsStr> = rel.iter().collect();
    let Some(name) = components.pop() else {
        // Empty relative path means `path == root`, already refused above.
        return EntryOutcome::Skipped(SkipReason::OutsideRoot);
    };
    let mut walked: Option<OwnedFd> = None;
    for comp in components {
        let base = walked.as_ref().map_or(root_fd, AsFd::as_fd);
        match rustix::fs::openat(base, comp, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => walked = Some(fd),
            Err(errno) => return EntryOutcome::Failed(errno_to_io(errno)),
        }
    }
    let parent_fd = walked.as_ref().map_or(root_fd, AsFd::as_fd);

    // Fresh look at the entry itself, without following a symlink.
    let st = match rustix::fs::statat(parent_fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(errno) => return EntryOutcome::Failed(errno_to_io(errno)),
    };
    if !kind_matches(kind, FileType::from_raw_mode(st.st_mode)) {
        return EntryOutcome::Skipped(SkipReason::KindChanged);
    }
    if let Some(expected) = expected
        && (st.st_dev != expected.dev || st.st_ino != expected.ino)
    {
        return EntryOutcome::Skipped(SkipReason::IdentityMismatch);
    }
    if kind.is_dir()
        && let Some(dir) = tree.dir_of(node)
        && tree.dir(dir).dev != st.st_dev
    {
        return EntryOutcome::Skipped(SkipReason::DeviceChanged);
    }

    let removal = if kind.is_dir() {
        remove_directory(parent_fd, name, st.st_dev, st.st_ino)
    } else {
        // Files, symlinks (the link itself), and device/fifo/socket nodes.
        rustix::fs::unlinkat(parent_fd, name, AtFlags::empty()).map_err(RemoveError::Io)
    };
    match removal {
        Ok(()) => {}
        Err(RemoveError::IdentityRace) => {
            return EntryOutcome::Skipped(SkipReason::IdentityMismatch);
        }
        Err(RemoveError::Io(errno)) => return EntryOutcome::Failed(errno_to_io(errno)),
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

/// Failure mode of the descriptor-relative removal.
enum RemoveError {
    /// The fd we opened for the target is not the inode we just `fstatat`'d
    /// — a swap slipped in between the two syscalls. Refuse, do not delete.
    IdentityRace,
    /// A filesystem syscall failed.
    Io(Errno),
}

/// Remove the directory `name` under `parent_fd`: open it `O_NOFOLLOW`,
/// confirm it is still the `(dev, ino)` the caller just `fstatat`'d (closing
/// the stat-to-open window), recursively empty it, then `unlinkat`
/// `AT_REMOVEDIR` the now-empty directory. Every step is descriptor-relative
/// — no path is ever rebuilt.
fn remove_directory(
    parent_fd: BorrowedFd<'_>,
    name: &OsStr,
    want_dev: u64,
    want_ino: u64,
) -> Result<(), RemoveError> {
    let dir_fd =
        rustix::fs::openat(parent_fd, name, DIR_FLAGS, Mode::empty()).map_err(RemoveError::Io)?;
    let opened = rustix::fs::fstat(&dir_fd).map_err(RemoveError::Io)?;
    if opened.st_dev != want_dev || opened.st_ino != want_ino {
        return Err(RemoveError::IdentityRace);
    }
    remove_dir_children(dir_fd.as_fd()).map_err(RemoveError::Io)?;
    rustix::fs::unlinkat(parent_fd, name, AtFlags::REMOVEDIR).map_err(RemoveError::Io)?;
    Ok(())
}

/// Recursively delete the contents of the already-open directory `dir_fd`,
/// leaving it empty for the caller to `unlinkat AT_REMOVEDIR`. Names are
/// snapshotted first: unlinking entries *while* iterating the getdents
/// stream can make the kernel skip or repeat entries. Each descent is a
/// fresh `openat(.., O_NOFOLLOW)` from `dir_fd`, so the walk cannot escape
/// this subtree and never follows a symlink (a symlink entry is `d_type`
/// `DT_LNK`, handled as a plain `unlinkat`).
fn remove_dir_children(dir_fd: BorrowedFd<'_>) -> Result<(), Errno> {
    let mut buf = vec![MaybeUninit::<u8>::uninit(); DIRENT_BUF];
    let mut iter = RawDir::new(dir_fd, &mut buf);
    let mut children: Vec<(CString, bool)> = Vec::new();
    while let Some(entry) = iter.next() {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let is_dir = match entry.file_type() {
            FileType::Directory => true,
            // Filesystems that do not fill `d_type` report Unknown; resolve
            // it with a no-follow stat so a symlink is never mistaken for a
            // directory (and thus never descended into).
            FileType::Unknown => is_dir_at(dir_fd, name),
            _ => false,
        };
        children.push((name.to_owned(), is_dir));
    }
    for (name, is_dir) in &children {
        if *is_dir {
            let sub = rustix::fs::openat(dir_fd, name.as_c_str(), DIR_FLAGS, Mode::empty())?;
            remove_dir_children(sub.as_fd())?;
            rustix::fs::unlinkat(dir_fd, name.as_c_str(), AtFlags::REMOVEDIR)?;
        } else {
            rustix::fs::unlinkat(dir_fd, name.as_c_str(), AtFlags::empty())?;
        }
    }
    Ok(())
}

/// Whether `name` under `dir_fd` is a real directory, resolved with a
/// no-follow stat (used only when `d_type` is unavailable). A failed stat
/// (the entry vanished mid-walk, etc.) answers "not a directory" — the
/// subsequent `unlinkat` will surface the real error.
fn is_dir_at(dir_fd: BorrowedFd<'_>, name: &CStr) -> bool {
    rustix::fs::statat(dir_fd, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory)
        .unwrap_or(false)
}

fn errno_to_io(errno: Errno) -> io::Error {
    io::Error::from_raw_os_error(errno.raw_os_error())
}

/// Whether the scanned kind still matches the on-disk file type. For
/// [`Kind::Other`] (kind unknown at scan time) anything non-directory is
/// accepted — the deletion path for non-dirs (`unlinkat`) is safe for any
/// of them, while treating an unknown as a dir never is.
fn kind_matches(kind: Kind, ft: FileType) -> bool {
    match kind {
        Kind::Dir => ft == FileType::Directory,
        Kind::File => ft == FileType::RegularFile,
        Kind::Symlink => ft == FileType::Symlink,
        Kind::Block => ft == FileType::BlockDevice,
        Kind::Char => ft == FileType::CharacterDevice,
        Kind::Fifo => ft == FileType::Fifo,
        Kind::Socket => ft == FileType::Socket,
        Kind::Other => ft != FileType::Directory,
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
