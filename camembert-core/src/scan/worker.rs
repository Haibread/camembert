//! Scan workers: work-stealing, fd-relative directory traversal.
//!
//! Each job is one directory, identified by its token. Traversal is
//! **descriptor-relative** (`openat` + `getdents64` via
//! [`rustix::fs::RawDir`] + `statx`/`fstatat`): absolute paths are never
//! reconstructed, so there is no `PATH_MAX` limit and hostile symlink
//! swaps cannot redirect the walk (`O_NOFOLLOW` below the root).

use std::ffi::{CStr, OsStr};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::Sender;
use crossbeam_deque::{Injector, Stealer, Worker as WorkerQueue};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, StatxFlags};
use rustix::io::Errno;
use tracing::{debug, trace, warn};

use crate::size::Size;
use crate::tree::Kind;

use super::message::{Batch, BatchEntry, SECTION_CAP, SectionSums};

/// getdents64 buffer size per job. Comfortably above the largest single
/// dirent (~280 bytes); 32 KiB amortizes syscalls on big directories.
const DIRENT_BUF: usize = 32 * 1024;

/// Backoff while the queues are empty but jobs are still in flight.
const IDLE_BACKOFF: Duration = Duration::from_micros(200);

/// How a job reaches its directory fd.
pub(crate) enum JobFd {
    /// Already open (the scan root).
    Opened(OwnedFd),
    /// Open relative to the parent directory's fd. The `Arc` keeps the
    /// parent fd alive while children are queued; the fd count is bounded
    /// by the number of directories with queued-but-unopened children
    /// (pathological width can approach `RLIMIT_NOFILE` — known MVP
    /// limitation, documented in the report).
    At(Arc<OwnedFd>, Vec<u8>),
}

pub(crate) struct Job {
    pub fd: JobFd,
    pub token: u64,
}

/// State shared by all workers (and the owner, for `abort`).
pub(crate) struct WorkerShared {
    pub injector: Injector<Job>,
    pub stealers: Vec<Stealer<Job>>,
    /// Jobs pushed but not fully processed. Workers exit when it reaches 0
    /// (a job's sections are all sent before its decrement, so 0 implies
    /// every batch has been handed to the channel).
    pub pending_jobs: AtomicUsize,
    /// Flat unique directory tokens. See the note in `owner.rs`: the
    /// decisions describe child tokens as "parent token + ordinal", which
    /// does not nest into a flat key, so tokens are drawn from this shared
    /// counter instead — one fetch_add per *directory*, not per entry.
    pub next_token: AtomicU64,
    /// statx availability, flipped off on the first `ENOSYS` (seccomp,
    /// gVisor, old kernels) — all workers then use `fstatat`.
    pub statx_supported: AtomicBool,
    /// `st_dev` of the scan root; mount boundary reference.
    pub root_dev: u64,
    /// Descend into other filesystems instead of marking them excluded.
    pub cross_filesystems: bool,
    /// Owner-side failure: drop everything and exit.
    pub abort: AtomicBool,
}

/// Worker main loop: pop local work, steal from the injector or siblings,
/// exit when no job is in flight anywhere.
pub(crate) fn run(
    worker_id: usize,
    local: WorkerQueue<Job>,
    shared: &WorkerShared,
    tx: &Sender<Batch>,
) {
    debug!(worker_id, "scan worker started");
    loop {
        if shared.abort.load(Ordering::Acquire) {
            break;
        }
        let Some(job) = find_job(&local, shared) else {
            if shared.pending_jobs.load(Ordering::Acquire) == 0 {
                break;
            }
            std::thread::sleep(IDLE_BACKOFF);
            continue;
        };
        let ok = process_job(job, &local, shared, tx);
        shared.pending_jobs.fetch_sub(1, Ordering::AcqRel);
        if !ok {
            // Channel gone: owner bailed out. abort is set; unwind.
            break;
        }
    }
    debug!(worker_id, "scan worker exiting");
}

fn find_job(local: &WorkerQueue<Job>, shared: &WorkerShared) -> Option<Job> {
    local.pop().or_else(|| {
        std::iter::repeat_with(|| {
            shared
                .injector
                .steal_batch_and_pop(local)
                .or_else(|| shared.stealers.iter().map(|s| s.steal()).collect())
        })
        .find(|s| !s.is_retry())
        .and_then(|s| s.success())
    })
}

/// Enumerate one directory, streaming sections to the owner. Returns false
/// only when the owner is gone (send failed).
fn process_job(
    job: Job,
    local: &WorkerQueue<Job>,
    shared: &WorkerShared,
    tx: &Sender<Batch>,
) -> bool {
    let token = job.token;
    let fd = match job.fd {
        JobFd::Opened(fd) => Arc::new(fd),
        JobFd::At(parent, name) => {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            match rustix::fs::openat(&*parent, OsStr::from_bytes(&name), flags, Mode::empty()) {
                Ok(fd) => Arc::new(fd),
                Err(errno) => {
                    debug!(
                        name = %String::from_utf8_lossy(&name),
                        %errno,
                        "directory open failed"
                    );
                    return send_batch(tx, shared, error_batch(token, errno));
                }
            }
        }
    };

    let mut buf = [MaybeUninit::<u8>::uninit(); DIRENT_BUF];
    let mut iter = RawDir::new(fd.as_fd(), &mut buf);
    let mut entries: Vec<BatchEntry> = Vec::new();
    let mut sums = SectionSums::default();
    let mut child_dirs: u32 = 0;

    while let Some(dirent) = iter.next() {
        let dirent = match dirent {
            Ok(dirent) => dirent,
            Err(errno) => {
                // getdents failed mid-listing; keep what we have, count
                // the directory as partially errored.
                warn!(token, %errno, "getdents failed mid-directory");
                sums.errors += 1;
                break;
            }
        };
        let name_c = dirent.file_name();
        let name = name_c.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }

        let entry = match stat_at(fd.as_fd(), name_c, &shared.statx_supported) {
            Ok(stat) => {
                trace!(
                    name = %String::from_utf8_lossy(name),
                    apparent = stat.size.apparent,
                    disk = stat.size.real,
                    "stat"
                );
                let kind = kind_of(stat.file_type);
                let mut entry = BatchEntry {
                    name: name.to_vec(),
                    kind,
                    apparent: stat.size.apparent,
                    disk: stat.size.real,
                    mtime: stat.mtime,
                    nlink: stat.nlink,
                    ino: stat.ino,
                    dev: stat.dev,
                    error: false,
                    child_token: None,
                    excluded_otherfs: false,
                };
                if kind == Kind::Dir {
                    if stat.dev != shared.root_dev && !shared.cross_filesystems {
                        debug!(
                            name = %String::from_utf8_lossy(name),
                            dev = stat.dev,
                            root_dev = shared.root_dev,
                            "mount boundary: not descending"
                        );
                        entry.excluded_otherfs = true;
                    } else {
                        let child_token = shared.next_token.fetch_add(1, Ordering::Relaxed);
                        entry.child_token = Some(child_token);
                        child_dirs += 1;
                        shared.pending_jobs.fetch_add(1, Ordering::AcqRel);
                        local.push(Job {
                            fd: JobFd::At(fd.clone(), name.to_vec()),
                            token: child_token,
                        });
                    }
                }
                entry
            }
            Err(errno) => {
                debug!(
                    name = %String::from_utf8_lossy(name),
                    %errno,
                    "stat failed"
                );
                sums.errors += 1;
                BatchEntry {
                    name: name.to_vec(),
                    kind: kind_of(dirent.file_type()),
                    apparent: 0,
                    disk: 0,
                    mtime: 0,
                    nlink: 0,
                    ino: 0,
                    dev: 0,
                    error: true,
                    child_token: None,
                    excluded_otherfs: false,
                }
            }
        };
        sums.apparent += entry.apparent;
        sums.disk += entry.disk;
        sums.count += 1;
        entries.push(entry);

        if entries.len() >= SECTION_CAP {
            // Giant directory: flush a full section (D2 — one run each).
            let batch = Batch {
                dir_token: token,
                entries: std::mem::take(&mut entries),
                sums: std::mem::take(&mut sums),
                is_last_section: false,
                child_dirs: std::mem::take(&mut child_dirs),
                dir_error: None,
            };
            if !send_batch(tx, shared, batch) {
                return false;
            }
        }
    }

    // Final (possibly empty) section: carries the self-token release.
    send_batch(
        tx,
        shared,
        Batch {
            dir_token: token,
            entries,
            sums,
            is_last_section: true,
            child_dirs,
            dir_error: None,
        },
    )
}

fn error_batch(token: u64, errno: Errno) -> Batch {
    Batch {
        dir_token: token,
        entries: Vec::new(),
        sums: SectionSums::default(),
        is_last_section: true,
        child_dirs: 0,
        dir_error: Some(errno),
    }
}

/// Send with abort-awareness: the bounded channel (backpressure, cap set in
/// `scan.rs`) can block; if the owner has bailed out we must not deadlock.
fn send_batch(tx: &Sender<Batch>, shared: &WorkerShared, batch: Batch) -> bool {
    let mut batch = batch;
    loop {
        match tx.send_timeout(batch, Duration::from_millis(100)) {
            Ok(()) => return true,
            Err(crossbeam_channel::SendTimeoutError::Timeout(b)) => {
                if shared.abort.load(Ordering::Acquire) {
                    return false;
                }
                batch = b;
            }
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

struct EntryStat {
    file_type: FileType,
    size: Size,
    mtime: i64,
    nlink: u32,
    ino: u64,
    dev: u64,
}

/// `statx` with a runtime fallback to `fstatat` when the kernel (or a
/// seccomp/gVisor sandbox) rejects it with `ENOSYS`. Always
/// `AT_SYMLINK_NOFOLLOW`: symlinks are never followed, they are stored
/// with their own sizes (lstat semantics).
fn stat_at(
    dirfd: BorrowedFd<'_>,
    name: &CStr,
    statx_supported: &AtomicBool,
) -> Result<EntryStat, Errno> {
    if statx_supported.load(Ordering::Relaxed) {
        let mask = StatxFlags::TYPE
            | StatxFlags::SIZE
            | StatxFlags::BLOCKS
            | StatxFlags::MTIME
            | StatxFlags::NLINK
            | StatxFlags::INO;
        match rustix::fs::statx(dirfd, name, AtFlags::SYMLINK_NOFOLLOW, mask) {
            Ok(x) => {
                return Ok(EntryStat {
                    file_type: FileType::from_raw_mode(u32::from(x.stx_mode)),
                    size: Size::new(x.stx_size, x.stx_blocks),
                    mtime: x.stx_mtime.tv_sec,
                    nlink: x.stx_nlink,
                    ino: x.stx_ino,
                    dev: rustix::fs::makedev(x.stx_dev_major, x.stx_dev_minor),
                });
            }
            Err(Errno::NOSYS) => {
                debug!("statx unsupported (ENOSYS), falling back to fstatat for this scan");
                statx_supported.store(false, Ordering::Relaxed);
            }
            Err(errno) => return Err(errno),
        }
    }
    let st = rustix::fs::statat(dirfd, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(EntryStat {
        file_type: FileType::from_raw_mode(st.st_mode),
        size: Size::new(st.st_size as u64, st.st_blocks as u64),
        mtime: st.st_mtime,
        nlink: u32::try_from(st.st_nlink).unwrap_or(u32::MAX),
        ino: st.st_ino,
        dev: st.st_dev,
    })
}

fn kind_of(file_type: FileType) -> Kind {
    match file_type {
        FileType::Directory => Kind::Dir,
        FileType::RegularFile => Kind::File,
        FileType::Symlink => Kind::Symlink,
        FileType::BlockDevice => Kind::Block,
        FileType::CharacterDevice => Kind::Char,
        FileType::Fifo => Kind::Fifo,
        FileType::Socket => Kind::Socket,
        FileType::Unknown => Kind::Other,
    }
}
