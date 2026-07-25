//! Linux backend for the scan engine (the platform seam).
//!
//! Descriptor-relative traversal (`openat`/`getdents64`/`statx`,
//! [`worker`]), the io_uring-batched statx engine ([`uring`]), and
//! storage-media detection ([`media`]) all live here, gated behind
//! `target_os = "linux"` (not `cfg(unix)`: `rustix::fs::RawDir` and
//! `rustix::fs::statx` are themselves `#[cfg(linux_kernel)]`, and
//! mountinfo/sysfs are Linux-only — a macOS backend would need a
//! different worker, not a shared one).
//!
//! `scan.rs` is platform-free and reaches this module through exactly six
//! hooks — [`open_root`], [`effective_threads`], [`probe_uring`],
//! [`start`], [`run`], [`path_on_compressed_mount`] — plus the two opaque
//! types [`Job`] and [`Shared`] threaded through them. A future Windows
//! backend (`scan/windows.rs`) implements the same contract; `scan.rs`
//! selects between them with `#[cfg]`, never a trait object or a runtime
//! enum.

mod media;
mod uring;
mod worker;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

use crossbeam_channel::Sender;
use crossbeam_deque::{Injector, Worker as WorkerQueue};
use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags};

use crate::size::Size;

use super::ScanError;
use super::message::Batch;
use super::owner::ROOT_TOKEN;

pub use media::path_on_compressed_mount;
pub(crate) use worker::Job;
pub(crate) use worker::WorkerShared as Shared;

/// An opened, `fstat`-stable root directory handle (see [`open_root`]).
pub(crate) type Root = OwnedFd;

/// The scan root's `fstat` result, trimmed to what the driver needs:
/// device (mount-boundary reference and media-detection key), the root
/// node's own [`Size`], and mtime.
pub(crate) struct RootStat {
    pub(crate) dev: u64,
    pub(crate) size: Size,
    pub(crate) mtime: i64,
}

/// Open + `fstat` the scan root. `O_DIRECTORY` (a non-dir root is an
/// error), no `O_NOFOLLOW`: a symlink *as the root argument* is followed;
/// symlinks below the root never are.
pub(crate) fn open_root(path: &Path) -> Result<(Root, RootStat), ScanError> {
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
    let stat = rustix::fs::fstat(&root_fd).map_err(|errno| ScanError::StatRoot {
        path: path.to_path_buf(),
        source: errno.into(),
    })?;
    Ok((
        root_fd,
        RootStat {
            dev: stat.st_dev,
            size: Size::new(stat.st_size as u64, stat.st_blocks as u64),
            mtime: stat.st_mtime,
        },
    ))
}

/// Resolve `threads` into a worker count for a scan of `root_path` (whose
/// `st_dev` is `root_dev`), plus a human-readable description of the
/// decision for logging (`"explicit"`, `"ssd"`, `"hdd (sda rotational)"`,
/// `"unknown (anon bdev (major 0), …)"`, `"ssd (btrfs via
/// /dev/nvme0n1p2)"`, …).
///
/// An explicit (non-zero) `threads` always wins, unchanged, and skips
/// media detection entirely. `0` runs the auto policy: [`media::resolve_media`]
/// probes the root's backing device (real sysfs in production, with a
/// `/proc/self/mountinfo` fallback for anonymous `major == 0` devices
/// such as btrfs — see the [`media`] module docs) and
/// [`media::thread_count`] — a pure function, unit-tested on its own —
/// turns that plus the core count into a worker count.
pub(crate) fn effective_threads(
    threads: usize,
    root_dev: u64,
    root_path: &Path,
) -> (usize, String) {
    if threads > 0 {
        return (threads, "explicit".to_string());
    }
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let media = media::resolve_media(root_dev, root_path, Path::new(media::DEFAULT_SYSFS_ROOT));
    let threads = media::thread_count(cores, &media);
    (threads, media.describe())
}

/// Probe io_uring availability for this process (see [`uring::probe`]).
pub(crate) fn probe_uring() -> Result<(), String> {
    uring::probe()
}

/// Build the shared worker state and per-worker work-stealing queues, and
/// seed the injector with the root job — everything the driver's
/// `WorkerShared` struct literal and root job push used to do inline.
pub(crate) fn start(
    threads: usize,
    root: Root,
    root_dev: u64,
    use_uring: bool,
    cross_filesystems: bool,
) -> (Shared, Vec<WorkerQueue<Job>>) {
    let queues: Vec<WorkerQueue<Job>> = (0..threads).map(|_| WorkerQueue::new_fifo()).collect();
    let shared = Shared {
        injector: Injector::new(),
        stealers: queues.iter().map(WorkerQueue::stealer).collect(),
        pending_jobs: AtomicUsize::new(1),
        next_token: AtomicU64::new(ROOT_TOKEN + 1),
        statx_supported: AtomicBool::new(true),
        use_uring,
        cross_filesystems,
        abort: AtomicBool::new(false),
    };
    shared.injector.push(worker::Job {
        fd: worker::JobFd::Opened(root),
        token: ROOT_TOKEN,
        dev: root_dev,
    });
    (shared, queues)
}

/// Worker main loop (see [`worker::run`]).
pub(crate) fn run(worker_id: usize, local: WorkerQueue<Job>, shared: &Shared, tx: &Sender<Batch>) {
    worker::run(worker_id, local, shared, tx)
}
