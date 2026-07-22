//! Parallel filesystem scan engine (MVP skeleton).
//!
//! Architecture (D1, `docs/design/scan-tree-decisions.md`): work-stealing
//! scan workers traverse descriptor-relative (`openat`/`getdents64`/
//! `statx`, no absolute paths, no `PATH_MAX`), pre-sum per-directory
//! sections and send them over **one bounded channel** to a single owner
//! that is the sole writer of a plain, non-concurrent arena
//! ([`crate::tree::Tree`]). No async runtime, no io_uring in this
//! increment (the statx-completion invariant it will need is documented in
//! [`message`] and `owner.rs`).
//!
//! The owner runs on the **calling thread**: [`Scanner::scan`] blocks
//! until the scan completes, which keeps the API synchronous and saves a
//! thread; progress is observable from other threads through the shared
//! [`ScanProgress`] handle. For a UI, [`Scanner::scan_live`] moves the
//! whole engine (owner included) to a background thread and returns a
//! [`crate::view::ViewBus`] immediately: the owner publishes view-scoped
//! snapshots and serves nav requests from its tick (called between batch
//! integrations and on receive timeouts), per D5.

mod message;
mod owner;
mod worker;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_deque::{Injector, Worker as WorkerQueue};
use rustix::fs::{Mode, OFlags};
use tracing::info;

use crate::size::Size;
use crate::tree::{DirId, DirMeta, Node, NodeId, Tree};
use crate::view::{ViewBus, ViewPublisher};

use owner::{Owner, ROOT_TOKEN};
use worker::{Job, JobFd, WorkerShared};

/// Bound of the worker → owner channel (sections). Backpressure: workers
/// stall rather than letting integration lag grow unbounded.
const CHANNEL_CAP: usize = 32;

/// Owner-side receive timeout: bounds how stale the owner's tick (view
/// publication, nav requests, cancellation checks, liveness) can get when
/// no batches arrive. Matches the D5 snapshot cadence.
const RECV_TIMEOUT: Duration = Duration::from_millis(33);

/// Scan configuration.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Worker threads. `0` = heuristic: 2× available parallelism, capped
    /// at 8 (this increment; the HDD/NVMe adaptive policy comes later).
    pub threads: usize,
    /// Descend into other filesystems instead of recording the mount
    /// point as an excluded directory.
    pub cross_filesystems: bool,
}

impl ScanOptions {
    fn effective_threads(&self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        (cores * 2).clamp(1, 8)
    }
}

/// Cheap shared progress counters, updated by the owner per integrated
/// batch and readable from any thread (e.g. a UI or a progress-line
/// poller).
#[derive(Debug, Default)]
pub struct ScanProgress {
    entries: AtomicU64,
    dirs: AtomicU64,
    errors: AtomicU64,
    disk_bytes: AtomicU64,
}

impl ScanProgress {
    /// Entries integrated so far (inodes, before hardlink dedup).
    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// Directories discovered so far.
    pub fn dirs(&self) -> u64 {
        self.dirs.load(Ordering::Relaxed)
    }

    /// Errors so far (unreadable directories + failed stats).
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Disk bytes (`st_blocks * 512`) aggregated so far.
    pub fn disk_bytes(&self) -> u64 {
        self.disk_bytes.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        self.entries.store(0, Ordering::Relaxed);
        self.dirs.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.disk_bytes.store(0, Ordering::Relaxed);
    }

    pub(crate) fn add_entries(&self, n: u64) {
        self.entries.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn add_dirs(&self, n: u64) {
        self.dirs.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn add_errors(&self, n: u64) {
        self.errors.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn add_disk_bytes(&self, n: u64) {
        self.disk_bytes.fetch_add(n, Ordering::Relaxed);
    }
}

/// Errors that abort a scan. Per-entry failures never abort: they are
/// counted (`te`, [`ScanOutcome::errors`]) and the scan carries on.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("cannot open scan root {path:?}: {source}")]
    OpenRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot stat scan root {path:?}: {source}")]
    StatRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("scan root {path:?} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("scan ended without completing the tree (engine bug): {reason}")]
    Incomplete { reason: String },
}

/// The parallel scanner. Construct once, then [`Scanner::scan`].
pub struct Scanner {
    options: ScanOptions,
    progress: Arc<ScanProgress>,
}

impl Scanner {
    pub fn new(options: ScanOptions) -> Self {
        Self {
            options,
            progress: Arc::new(ScanProgress::default()),
        }
    }

    /// Shared progress handle; poll it from another thread while
    /// [`Scanner::scan`] blocks.
    pub fn progress(&self) -> Arc<ScanProgress> {
        Arc::clone(&self.progress)
    }

    /// Scan `path` to completion (blocking; the owner runs on this
    /// thread).
    pub fn scan(&self, path: impl AsRef<Path>) -> Result<ScanOutcome, ScanError> {
        self.scan_with_tick(path.as_ref(), |_| true)
    }

    /// Scan `path` on a background thread and return immediately with the
    /// [`ViewBus`] a UI reads snapshots from (D5) plus the handle the
    /// final [`ScanOutcome`] is retrieved through.
    ///
    /// During the scan the owner serves nav requests and publishes
    /// snapshots from its tick. **After completion the owner thread
    /// exits** and the outcome (the frozen arena) is handed to the caller
    /// via [`LiveScan::join`]; post-scan navigation is served by the
    /// caller reading the arena directly with
    /// [`crate::view::build_snapshot`]. Chosen over keeping the owner
    /// thread alive: the arena is immutable once the scan ends, so
    /// single-threaded direct reads are trivially correct and there is no
    /// idle thread or shutdown handshake to maintain.
    ///
    /// Cancellation: [`ViewBus::cancel`] (or [`LiveScan::cancel`]) makes
    /// workers stop taking work, the owner drain, and [`LiveScan::join`]
    /// return a partial outcome with [`ScanOutcome::cancelled`] set.
    pub fn scan_live(self, path: impl Into<PathBuf>) -> LiveScan {
        let path: PathBuf = path.into();
        let bus = Arc::new(ViewBus::new(path.clone()));
        let owner_bus = Arc::clone(&bus);
        let handle = std::thread::Builder::new()
            .name("camembert-owner".into())
            .spawn(move || {
                let mut publisher = ViewPublisher::new(Arc::clone(&owner_bus));
                self.scan_with_tick(&path, |ctx| {
                    publisher.tick(ctx.tree, ctx.root, ctx.hardlinks_seen);
                    !owner_bus.cancel_requested()
                })
            })
            .expect("spawn scan owner thread");
        LiveScan { bus, handle }
    }

    /// Core scan loop. `tick` runs on the owner thread between batch
    /// integrations and on receive timeouts; returning `false` cancels
    /// the scan (workers stop, the owner drains, the outcome is partial
    /// and flagged [`ScanOutcome::cancelled`]).
    pub(crate) fn scan_with_tick(
        &self,
        path: &Path,
        mut tick: impl FnMut(TickContext<'_>) -> bool,
    ) -> Result<ScanOutcome, ScanError> {
        let start = Instant::now();
        self.progress.reset();

        // Open + stat the root. O_DIRECTORY (a non-dir root is an error),
        // no O_NOFOLLOW: a symlink *as the root argument* is followed;
        // symlinks below the root never are.
        let root_fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| {
            if errno == rustix::io::Errno::NOTDIR {
                ScanError::NotADirectory {
                    path: path.to_path_buf(),
                }
            } else {
                ScanError::OpenRoot {
                    path: path.to_path_buf(),
                    source: errno.into(),
                }
            }
        })?;
        let root_stat = rustix::fs::fstat(&root_fd).map_err(|errno| ScanError::StatRoot {
            path: path.to_path_buf(),
            source: errno.into(),
        })?;
        let root_dev = root_stat.st_dev;
        let root_size = Size::new(root_stat.st_size as u64, root_stat.st_blocks as u64);

        let threads = self.options.effective_threads();
        info!(
            path = %path.display(),
            threads,
            cross_filesystems = self.options.cross_filesystems,
            "scan starting"
        );

        let (tx, rx) = crossbeam_channel::bounded::<message::Batch>(CHANNEL_CAP);
        let queues: Vec<WorkerQueue<Job>> = (0..threads).map(|_| WorkerQueue::new_fifo()).collect();
        let shared = WorkerShared {
            injector: Injector::new(),
            stealers: queues.iter().map(WorkerQueue::stealer).collect(),
            pending_jobs: AtomicUsize::new(1),
            next_token: AtomicU64::new(ROOT_TOKEN + 1),
            statx_supported: AtomicBool::new(true),
            cross_filesystems: self.options.cross_filesystems,
            abort: AtomicBool::new(false),
        };
        shared.injector.push(Job {
            fd: JobFd::Opened(root_fd),
            token: ROOT_TOKEN,
            dev: root_dev,
        });

        let mut owner = Owner::new(
            path.as_os_str().as_encoded_bytes(),
            root_size,
            root_stat.st_mtime,
            root_dev,
            Arc::clone(&self.progress),
        );

        let result = std::thread::scope(|scope| {
            // Whatever happens below (including a panic in the owner
            // loop), workers must be told to exit before the scope joins
            // them.
            struct AbortGuard<'a>(&'a AtomicBool);
            impl Drop for AbortGuard<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _guard = AbortGuard(&shared.abort);

            for (id, queue) in queues.into_iter().enumerate() {
                let shared = &shared;
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name(format!("camembert-scan-{id}"))
                    .spawn_scoped(scope, move || worker::run(id, queue, shared, &tx))
                    .expect("spawn scan worker");
            }
            drop(tx);

            // Owner loop: integrate until the root completes (or the tick
            // cancels the scan). `tick` (D5 publication hook + nav +
            // cancellation) runs after every integration AND on receive
            // timeouts, so the view stays live even when the storage
            // stalls.
            let mut run_tick = |owner: &Owner| {
                tick(TickContext {
                    tree: owner.tree(),
                    root: owner.root(),
                    hardlinks_seen: owner.hardlink_inodes() > 0,
                })
            };
            loop {
                if owner.root_complete() {
                    break;
                }
                match rx.recv_timeout(RECV_TIMEOUT) {
                    Ok(batch) => {
                        owner.handle_batch(batch);
                        if !run_tick(&owner) {
                            return Ok(true);
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if !run_tick(&owner) {
                            return Ok(true);
                        }
                        if shared.pending_jobs.load(Ordering::Acquire) == 0 {
                            // All jobs done: drain what is left, then the
                            // root must be complete.
                            while let Ok(batch) = rx.try_recv() {
                                owner.handle_batch(batch);
                                if !run_tick(&owner) {
                                    return Ok(true);
                                }
                            }
                            if !owner.root_complete() {
                                return Err(ScanError::Incomplete {
                                    reason: "workers idle but tree incomplete".into(),
                                });
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        if !owner.root_complete() {
                            return Err(ScanError::Incomplete {
                                reason: "workers exited but tree incomplete".into(),
                            });
                        }
                    }
                }
            }
            // Final tick after root completion so the publisher can emit
            // its root_complete snapshot before the owner returns.
            run_tick(&owner);
            Ok(false)
        });
        let cancelled = result?;

        let elapsed = start.elapsed();
        let excluded_dirs = owner.excluded_dirs();
        let excluded_kernfs = owner.excluded_kernfs();
        let hardlink_inodes = owner.hardlink_inodes();
        let hardlink_extra_links = owner.hardlink_extra_links();
        let (tree, root) = owner.into_tree();
        let root_meta = tree.dir(root);
        let outcome = ScanOutcome {
            totals: Size {
                apparent: root_meta.ta,
                real: root_meta.td,
            },
            entries: root_meta.tn,
            dirs: tree.dir_count() as u64,
            errors: u64::from(root_meta.te),
            excluded_dirs,
            excluded_kernfs,
            hardlink_inodes,
            hardlink_extra_links,
            elapsed,
            cancelled,
            root_path: path.to_path_buf(),
            root,
            tree,
        };
        info!(
            entries = outcome.entries,
            dirs = outcome.dirs,
            errors = outcome.errors,
            cancelled,
            elapsed_ms = elapsed.as_millis() as u64,
            "scan finished"
        );
        Ok(outcome)
    }
}

/// What the owner tick sees between batch integrations.
pub(crate) struct TickContext<'a> {
    pub tree: &'a Tree,
    pub root: DirId,
    /// Any `nlink > 1` inode seen so far (drives the D3 provisional-totals
    /// note).
    pub hardlinks_seen: bool,
}

/// A scan running on a background thread, navigable while it runs.
///
/// Returned by [`Scanner::scan_live`]: the [`ViewBus`] is available
/// immediately; the final [`ScanOutcome`] via [`LiveScan::join`] once
/// [`LiveScan::is_finished`] (or blockingly at any time).
pub struct LiveScan {
    bus: Arc<ViewBus>,
    handle: std::thread::JoinHandle<Result<ScanOutcome, ScanError>>,
}

impl LiveScan {
    /// The shared snapshot/nav handle (wait-free on the UI side).
    pub fn bus(&self) -> &Arc<ViewBus> {
        &self.bus
    }

    /// Whether the scan thread has finished (completed, cancelled, or
    /// failed). [`LiveScan::join`] will not block once this is true.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Request cancellation (see [`ViewBus::cancel`]); the scan winds down
    /// within roughly one worker send-retry interval (~100 ms).
    pub fn cancel(&self) {
        self.bus.cancel();
    }

    /// Wait for the scan thread and return its outcome. A panic on the
    /// owner thread is resumed here.
    pub fn join(self) -> Result<ScanOutcome, ScanError> {
        match self.handle.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

/// Result of a completed scan: the arena tree plus summary counters.
#[derive(Debug)]
pub struct ScanOutcome {
    tree: Tree,
    root: DirId,
    root_path: PathBuf,
    /// Subtree totals of the root (hardlink first-seen attribution).
    pub totals: Size,
    /// Inodes counted (root's `tn`: hardlink extras excluded).
    pub entries: u64,
    /// Directories with metadata (scanned or unreadable; excluded
    /// other-fs mount points not included).
    pub dirs: u64,
    /// Unreadable directories + failed stats (root's `te`).
    pub errors: u64,
    /// Mount points recorded but not descended into.
    pub excluded_dirs: u64,
    /// Subset of `excluded_dirs` that are kernel pseudo-filesystems
    /// (`/proc`, `/sys`, …) — excluded even with `--cross-filesystems`.
    pub excluded_kernfs: u64,
    /// Distinct `(dev, ino)` with `nlink > 1` seen.
    pub hardlink_inodes: u64,
    /// Later links that contributed 0 to aggregates.
    pub hardlink_extra_links: u64,
    pub elapsed: Duration,
    /// The scan was cancelled ([`ViewBus::cancel`]): the tree and every
    /// counter above are partial (whatever integrated before the stop).
    pub cancelled: bool,
}

impl ScanOutcome {
    /// The underlying arena (read-only).
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// The scan root directory.
    pub fn root(&self) -> DirId {
        self.root
    }

    /// The root path as given to [`Scanner::scan`].
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Iterate a directory's children across its run list (D2).
    pub fn children_of(&self, dir: DirId) -> impl Iterator<Item = NodeId> + '_ {
        self.tree.children(dir)
    }

    /// Raw name bytes of a node.
    pub fn name_of(&self, node: NodeId) -> &[u8] {
        self.tree.name(node)
    }

    pub fn node(&self, id: NodeId) -> &Node {
        self.tree.node(id)
    }

    pub fn dir(&self, id: DirId) -> &DirMeta {
        self.tree.dir(id)
    }

    /// Full path of a directory: the root path joined with the names up
    /// the parent chain.
    pub fn path_of(&self, dir: DirId) -> PathBuf {
        self.tree.path_of(dir)
    }

    /// The `n` largest directories by real (disk) subtree size,
    /// descending. Ties broken by arena order for determinism.
    pub fn top_dirs_by_disk(&self, n: usize) -> Vec<DirId> {
        let mut dirs: Vec<DirId> = self.tree.dir_ids().collect();
        dirs.sort_by_key(|&d| (std::cmp::Reverse(self.tree.dir(d).td), d.index()));
        dirs.truncate(n);
        dirs
    }

    /// The `n` directories with the most *direct* errors (their own
    /// unreadable children / failed stats, not their subtrees'),
    /// descending, with the count. Subtree `te` would rank every ancestor
    /// above the actual error site; direct counts point at *where* the
    /// total is incomplete ("comptabiliser l'illisible").
    pub fn top_dirs_by_errors(&self, n: usize) -> Vec<(DirId, u32)> {
        let mut direct: Vec<u32> = self.tree.dir_ids().map(|d| self.tree.dir(d).te).collect();
        for d in self.tree.dir_ids() {
            if let Some(parent) = self.tree.dir(d).parent {
                direct[parent.index()] = direct[parent.index()].saturating_sub(self.tree.dir(d).te);
            }
        }
        let mut dirs: Vec<(DirId, u32)> = self
            .tree
            .dir_ids()
            .map(|d| (d, direct[d.index()]))
            .filter(|&(_, te)| te > 0)
            .collect();
        dirs.sort_by_key(|&(d, te)| (std::cmp::Reverse(te), d.index()));
        dirs.truncate(n);
        dirs
    }
}
