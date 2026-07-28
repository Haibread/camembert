//! Windows backend for the scan engine (the platform seam).
//!
//! Implements the same six-hook contract as [`super::linux`] —
//! [`open_root`], [`effective_threads`], [`probe_uring`], [`start`],
//! [`run`], [`path_on_compressed_mount`] — plus the two opaque types
//! [`Job`] and [`Shared`]. `scan.rs` picks between the two with `#[cfg]`
//! and knows nothing else about either.
//!
//! Where the two backends genuinely differ (design:
//! `docs/design/windows-backend-design.md`):
//!
//! - Traversal is **path-relative**, not descriptor-relative. There is no
//!   `openat`, so a child directory is opened by name joined onto its
//!   parent's full path. Canonicalising the root to a `\\?\` path once at
//!   [`open_root`] is what keeps the whole walk clear of `MAX_PATH`; every
//!   path below it inherits the prefix by construction. Two consequences
//!   that `\\?\` brings with it: path *normalisation is disabled*, which is
//!   safe only because names come from directory listings and never from
//!   user fragments, and the 255-character component limit still applies.
//! - Sizes come from the directory listing itself
//!   (`FILE_ID_EXTD_DIR_INFO`), so there is no per-entry stat call for
//!   them — `std::fs::read_dir` is unusable here not because of handles but
//!   because `WIN32_FIND_DATAW` carries no allocation size at all.
//! - There is no io_uring, no `st_dev` per entry, and no kernel
//!   pseudo-filesystem to exclude.

mod worker;

use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

use crossbeam_channel::Sender;
use crossbeam_deque::{Injector, Worker as WorkerQueue};
use tracing::debug;
use windows_sys::Win32::Foundation::{ERROR_DIRECTORY, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileIdInfo, FileStandardInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};

use crate::size::Size;

use super::ScanError;
use super::message::Batch;
use super::owner::ROOT_TOKEN;

pub(crate) use worker::Job;
pub(crate) use worker::WorkerShared as Shared;

/// A NUL-terminated, `\\?\`-prefixed UTF-16 directory path.
///
/// Held behind an `Arc` and shared by every child job of a directory, so
/// the per-job cost of "which directory is this" is a pointer plus the
/// child's own name — the Windows analogue of the Linux worker's
/// `Arc<OwnedFd>` parent, with the opposite resource profile: no handle is
/// held open while children are queued, so the `RLIMIT_NOFILE` pathology
/// the Linux backend documents does not exist here. Memory grows with
/// queued-but-unopened directories instead.
pub(crate) struct WidePath(Vec<u16>);

impl WidePath {
    fn from_path(path: &Path) -> Self {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        Self(wide)
    }

    /// Pointer for `PCWSTR` parameters. Always NUL-terminated.
    fn as_pcwstr(&self) -> *const u16 {
        self.0.as_ptr()
    }

    /// The path without its terminating NUL.
    fn body(&self) -> &[u16] {
        &self.0[..self.0.len() - 1]
    }

    /// `self \ name`. `\\?\` paths are handed to the kernel unnormalised,
    /// so a doubled separator would be a literal empty component rather
    /// than being collapsed — hence the explicit check for a root that
    /// already ends in one (`\\?\C:\`).
    pub(crate) fn join(&self, name: &[u16]) -> Self {
        let body = self.body();
        let mut wide = Vec::with_capacity(body.len() + name.len() + 2);
        wide.extend_from_slice(body);
        if wide.last() != Some(&SEPARATOR) {
            wide.push(SEPARATOR);
        }
        wide.extend_from_slice(name);
        wide.push(0);
        Self(wide)
    }
}

impl std::fmt::Display for WidePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf16_lossy(self.body()))
    }
}

const SEPARATOR: u16 = b'\\' as u16;

/// An opened scan root: the directory handle plus its own `\\?\` path,
/// which every descendant job is built from.
///
/// The design sketched this as a bare `OwnedHandle`; it cannot be, because
/// path-relative traversal needs the root's path and the seam gives
/// [`start`] no other way to receive it.
pub(crate) struct Root {
    handle: OwnedHandle,
    path: Arc<WidePath>,
}

/// The scan root's metadata, trimmed to what the driver needs. `dev` is the
/// volume serial number — the `st_dev` analogue, and the only one this
/// backend has: entries in a listing carry no volume of their own, and T1
/// never crosses a volume boundary anyway (see [`worker`]).
pub(crate) struct RootStat {
    pub(crate) dev: u64,
    pub(crate) size: Size,
    pub(crate) mtime: i64,
}

/// `CreateFileW` flags shared by the root and every directory below it.
///
/// `FILE_LIST_DIRECTORY` is not optional: the design named only
/// `FILE_READ_ATTRIBUTES` (which is what lets a scanner read metadata "even
/// if `GENERIC_READ` access would have been denied"), but
/// `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` enumerates, and
/// enumeration needs list access — with attributes alone every directory
/// fails with `ERROR_ACCESS_DENIED`.
///
/// `FILE_FLAG_BACKUP_SEMANTICS` is mandatory to open a directory at all.
/// All three `FILE_SHARE_*` bits, or the scanner collides with anything
/// that merely has a file open.
pub(crate) const DIR_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES;
pub(crate) const DIR_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Open a directory by full `\\?\` path. `extra_flags` carries
/// `FILE_FLAG_OPEN_REPARSE_POINT` for directories below the root (lstat
/// parity) and nothing for the root itself, which follows links exactly as
/// the Linux backend's root does.
///
/// Returns the raw Win32 code on failure — the caller decides whether that
/// is a scan-aborting root failure or one recorded directory error.
pub(crate) fn open_dir(path: &WidePath, extra_flags: u32) -> Result<OwnedHandle, u32> {
    // SAFETY: `path` is NUL-terminated by construction (`WidePath` appends
    // it and never removes it), which is the only precondition CreateFileW
    // places on `lpFileName`; the null `lpSecurityAttributes` and null
    // `hTemplateFile` are both documented as valid.
    let handle = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            path.as_pcwstr(),
            DIR_ACCESS,
            DIR_SHARE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | extra_flags,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        // SAFETY: read of this thread's last-error slot, immediately after
        // the failing call and with nothing in between that could clobber
        // it. No pointers involved.
        return Err(unsafe { GetLastError() });
    }
    // SAFETY: CreateFileW returned success, so `handle` is a fresh, owned,
    // non-null handle that nothing else holds; transferring it to
    // `OwnedHandle` gives it exactly one owner and one `CloseHandle`.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

/// Read one `FILE_INFO_BY_HANDLE_CLASS` record from an open handle.
///
/// Generic over the info struct rather than repeated per class: every
/// caller here wants the same "fill this fixed-size struct or tell me the
/// Win32 code" shape, and the variable-length classes (the directory
/// listing) go through [`worker`]'s own buffer walk instead.
fn info_by_handle<T: Default>(handle: HANDLE, class: i32) -> Result<T, u32> {
    let mut info = T::default();
    // SAFETY: `class` is paired with `T` at every call site below, and the
    // length passed is exactly `T`'s, so the kernel writes within the
    // allocation. `handle` is borrowed from a live `OwnedHandle`.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            class,
            std::ptr::from_mut(&mut info).cast(),
            u32::try_from(size_of::<T>()).expect("info struct fits in u32"),
        )
    };
    if ok == 0 {
        // SAFETY: as in `open_dir` — last-error read straight after the
        // failing call.
        return Err(unsafe { GetLastError() });
    }
    Ok(info)
}

/// Windows `FILETIME` (100 ns ticks since 1601-01-01 UTC) to unix seconds.
/// Saturating rather than wrapping: a corrupt or zero timestamp should read
/// as an implausible date, never as a value that shifts totals elsewhere.
pub(crate) fn filetime_to_unix(ticks: i64) -> i64 {
    /// Seconds between the Windows epoch and the unix epoch.
    const EPOCH_DELTA: i64 = 11_644_473_600;
    (ticks / 10_000_000).saturating_sub(EPOCH_DELTA)
}

/// Open + stat the scan root.
///
/// Canonicalising first does three jobs at once: it resolves a symlink or
/// junction given *as the root argument* (matching the Linux backend, which
/// omits `O_NOFOLLOW` only there), it yields the `\\?\` prefix the whole
/// walk needs, and it fails early on a path that does not exist.
pub(crate) fn open_root(path: &Path) -> Result<(Root, RootStat), ScanError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| {
        // `canonicalize` reports a non-directory component as
        // ERROR_DIRECTORY; surfacing it as a generic open failure would
        // hide the one root error users actually cause themselves.
        if source.raw_os_error() == Some(ERROR_DIRECTORY as i32) {
            ScanError::NotADirectory {
                path: path.to_path_buf(),
            }
        } else {
            ScanError::OpenRoot {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let wide = WidePath::from_path(&canonical);
    debug!(root = %wide, "canonicalized scan root");

    let handle = open_dir(&wide, 0).map_err(|code| ScanError::OpenRoot {
        path: path.to_path_buf(),
        source: std::io::Error::from_raw_os_error(code as i32),
    })?;
    let raw: HANDLE = handle.as_raw_handle().cast();

    let stat_err = |code: u32| ScanError::StatRoot {
        path: path.to_path_buf(),
        source: std::io::Error::from_raw_os_error(code as i32),
    };
    let basic: FILE_BASIC_INFO = info_by_handle(raw, FileBasicInfo).map_err(stat_err)?;
    // `FILE_FLAG_BACKUP_SEMANTICS` happily opens a *file* too, so the
    // directory check has to be explicit — otherwise a file root would fail
    // much later, as an unexplained empty enumeration.
    if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(ScanError::NotADirectory {
            path: path.to_path_buf(),
        });
    }
    let standard: FILE_STANDARD_INFO = info_by_handle(raw, FileStandardInfo).map_err(stat_err)?;
    // `FILE_ID_INFO`'s serial is 64-bit, unlike `BY_HANDLE_FILE_INFORMATION`'s
    // `dwVolumeSerialNumber`; ReFS needs the width.
    let id: FILE_ID_INFO = info_by_handle(raw, FileIdInfo).map_err(stat_err)?;

    let stat = RootStat {
        dev: id.VolumeSerialNumber,
        size: entry_size(standard.EndOfFile, standard.AllocationSize),
        mtime: filetime_to_unix(basic.LastWriteTime),
    };
    Ok((
        Root {
            handle,
            path: Arc::new(wide),
        },
        stat,
    ))
}

/// Build a [`Size`] from a listing's two size fields.
///
/// `AllocationSize` is bytes already — cluster-granular by MS-FSCC §2.4.38,
/// compression-aware (measured), and **not** a multiple of 512 for
/// MFT-resident files (a 1-byte file reports 8). So this cannot go through
/// `Size::new`, which multiplies a block count: that would round a resident
/// file's 8 bytes down to zero.
pub(crate) fn entry_size(end_of_file: i64, allocation_size: i64) -> Size {
    Size {
        apparent: end_of_file.max(0) as u64,
        real: allocation_size.max(0) as u64,
    }
}

/// A directory's own size, asked of the handle instead of of its parent.
///
/// The listing that discovered this directory reported
/// `AllocationSize = EndOfFile = 0` for it, as a Windows listing does for
/// every subdirectory entry — but NTFS does charge a directory for its index
/// (a `sub/` of 400 long-named files occupies ~192 KiB of INDX blocks), and
/// `FileStandardInfo` on an open handle reports it. That asymmetry is why
/// the scan root, opened by handle in [`open_root`], was the only directory
/// whose self-size was ever right.
///
/// The worker opens every directory anyway in order to enumerate it, so this
/// costs no extra open — only the information query, which the nlink dossier
/// (§2.1) measured at ~1.13 µs against ~46 µs for a by-name open. The
/// opposite regime from the `nlink` trap.
///
/// Errors are the caller's to swallow: the directory's *entries* are already
/// being enumerated successfully, so a refused metadata query costs the
/// directory its own index bytes and nothing else.
pub(crate) fn dir_own_size(handle: &OwnedHandle) -> Result<Size, u32> {
    let raw: HANDLE = handle.as_raw_handle().cast();
    let standard: FILE_STANDARD_INFO = info_by_handle(raw, FileStandardInfo)?;
    Ok(entry_size(standard.EndOfFile, standard.AllocationSize))
}

/// Resolve `threads` into a worker count.
///
/// An explicit (non-zero) `threads` wins unchanged, as on Linux. `0` gets
/// the *unknown-medium* tier, honestly labelled: the Windows analogue of
/// `/sys/block/*/queue/rotational` is
/// `DeviceIoControl(IOCTL_STORAGE_QUERY_PROPERTY,
/// StorageDeviceSeekPenaltyProperty)` → `IncursSeekPenalty`, named here so
/// nobody re-researches it, but it is T2 work and guessing the tier would
/// be worse than admitting the gap.
pub(crate) fn effective_threads(
    threads: usize,
    _root_dev: u64,
    _root_path: &Path,
) -> (usize, String) {
    if threads > 0 {
        return (threads, "explicit".to_string());
    }
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    (
        (cores * 2).min(8),
        "unknown (no seek-penalty probe on Windows yet)".to_string(),
    )
}

/// io_uring is a Linux kernel interface; the Windows caller falls back to
/// the only engine this backend has. Never an error for the scan itself —
/// `scan.rs` warns and continues.
pub(crate) fn probe_uring() -> Result<(), String> {
    Err("io_uring is a Linux interface; the Windows backend has no batched-stat engine".to_string())
}

/// Build shared worker state and per-worker queues, and seed the injector
/// with the root job.
///
/// `use_uring` is meaningless here ([`probe_uring`] always refuses, so
/// `scan.rs` can only pass `false`). `cross_filesystems` is accepted and
/// deliberately ignored: T1 refuses **every** mount point, including the
/// same-volume junctions the flag would legitimately descend, because a
/// junction can point at its own ancestor and this backend has no cycle
/// detection. Refusing is the only setting that cannot loop; a
/// junction-heavy tree under-counts, and that is a documented T1
/// divergence.
///
/// [`Shared::query_link_counts`] is deliberately **not** a parameter here:
/// the signature is shared with the Linux backend, which has no such knob
/// (its link counts ride inside `statx` for free), so `scan.rs` sets the
/// field on the returned struct in a `cfg(windows)` block instead of
/// growing a parameter every non-Windows caller would ignore.
pub(crate) fn start(
    threads: usize,
    root: Root,
    root_dev: u64,
    _use_uring: bool,
    _cross_filesystems: bool,
) -> (Shared, Vec<WorkerQueue<Job>>) {
    let queues: Vec<WorkerQueue<Job>> = (0..threads).map(|_| WorkerQueue::new_fifo()).collect();
    let shared = Shared {
        injector: Injector::new(),
        stealers: queues.iter().map(WorkerQueue::stealer).collect(),
        pending_jobs: AtomicUsize::new(1),
        next_token: AtomicU64::new(ROOT_TOKEN + 1),
        query_link_counts: false,
        nt_stat_supported: AtomicBool::new(true),
        id_folded: AtomicBool::new(false),
        vol: root_dev,
        abort: AtomicBool::new(false),
    };
    shared.injector.push(worker::Job {
        dir: worker::JobDir::Opened(root.handle, Arc::clone(&root.path)),
        token: ROOT_TOKEN,
    });
    (shared, queues)
}

/// Worker main loop (see [`worker::run`]).
pub(crate) fn run(worker_id: usize, local: WorkerQueue<Job>, shared: &Shared, tx: &Sender<Batch>) {
    worker::run(worker_id, local, shared, tx);
}

/// Whether the per-file link-count lookup survived the whole scan.
///
/// `--links` asks for it; this reports whether the filesystem actually
/// answered. `query_nlink` latches
/// [`worker::WorkerShared::nt_stat_supported`] off for the rest of the scan
/// the first time a status says "this build or filesystem does not
/// implement `FileStatInformation`" — an SMB share, a filesystem driver
/// without it — and every file after that silently falls back to `nlink =
/// 1`, which is exactly the value that keeps an entry *out* of the hardlink
/// registry.
///
/// So a latched-off scan deduplicates nothing from that point on, and if it
/// still claimed its link counts were known it would put a fabricated
/// `l: 1` on every entry of a dump and word the summary as "links on the
/// volume". Read after the workers join, so no ordering beyond `Relaxed` is
/// needed. Meaningless (and always `true`) when `--links` is off, since
/// nothing consulted the flag.
pub(crate) fn link_counts_obtained(shared: &Shared) -> bool {
    shared
        .nt_stat_supported
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the filesystem under `path` compresses transparently.
///
/// Always `false` on Windows, and that is the honest answer rather than a
/// missing one: NTFS compression is a **per-file** attribute
/// (`FILE_ATTRIBUTE_COMPRESSED`, which the directory listing hands us for
/// free) and not a mount option, so there is no mount-level property to
/// report. The caveat this drives — "freeable figures are logical, physical
/// reclaim may be smaller" — is a statement about the whole mount, and on
/// Windows no such statement is true.
pub fn path_on_compressed_mount(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Joining onto a canonicalized drive root must not double the
    /// separator: `\\?\` paths are not normalised by the kernel, so
    /// `\\?\C:\\Windows` would name an empty component and fail.
    #[test]
    fn join_does_not_double_the_separator_at_a_drive_root() {
        let root = WidePath::from_path(Path::new(r"\\?\C:\"));
        let joined = root.join(&"Windows".encode_utf16().collect::<Vec<u16>>());
        assert_eq!(joined.to_string(), r"\\?\C:\Windows");
        assert_eq!(joined.0.last(), Some(&0), "must stay NUL-terminated");
    }

    #[test]
    fn join_inserts_the_separator_below_the_root() {
        let dir = WidePath::from_path(Path::new(r"\\?\C:\Windows"));
        let joined = dir.join(&"System32".encode_utf16().collect::<Vec<u16>>());
        assert_eq!(joined.to_string(), r"\\?\C:\Windows\System32");
    }

    /// The unix epoch itself, and the two boundaries a bad timestamp
    /// produces: 0 (the Windows epoch) must land far in the past rather
    /// than wrap into the future.
    #[test]
    fn filetime_converts_to_unix_seconds() {
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        assert_eq!(filetime_to_unix(0), -11_644_473_600);
        assert_eq!(filetime_to_unix(116_444_736_010_000_000), 1);
    }

    /// A 1-byte MFT-resident file reports `AllocationSize = 8` — not a
    /// multiple of 512. Routing that through a block count would report it
    /// as occupying nothing.
    #[test]
    fn resident_file_allocation_survives_conversion() {
        let size = entry_size(1, 8);
        assert_eq!(size.apparent, 1);
        assert_eq!(size.real, 8);
    }

    /// Negative sizes are not physically meaningful; the API types are
    /// signed, so clamp rather than reinterpret a negative as ~16 EiB.
    #[test]
    fn negative_sizes_clamp_to_zero() {
        let size = entry_size(-1, -1);
        assert_eq!(size.apparent, 0);
        assert_eq!(size.real, 0);
    }

    /// There is no mount-level compression on Windows, and saying "yes"
    /// would attach a caveat to every scan.
    #[test]
    fn compressed_mount_is_always_false() {
        assert!(!path_on_compressed_mount(Path::new(r"C:\")));
    }
}
