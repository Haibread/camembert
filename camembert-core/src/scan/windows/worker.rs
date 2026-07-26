//! Scan workers: work-stealing, path-relative directory traversal.
//!
//! Each job is one directory, identified by its token. The work-stealing
//! skeleton (`run`/`find_job`/`process_job`/`send_batch`) is a deliberate
//! near-verbatim port of [`crate::scan::linux::worker`]'s, because the
//! owner's completion protocol (`owner.rs`, [`message`]) depends on the
//! exact ordering it implements: every section fully resolved before it is
//! sent, the self-token release carried by the final section, the
//! `pending_jobs` decrement after the last send.
//!
//! What genuinely differs is the middle:
//!
//! - **One listing call replaces `getdents64` + n × `statx`.**
//!   `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` returns name,
//!   both sizes, attributes, reparse tag and a 128-bit file id per entry,
//!   for zero per-entry syscalls. The rejected alternative
//!   (`CreateFileW` + `GetFileInformationByHandle` + `CloseHandle` per
//!   entry) is 20-200× more expensive per published measurements.
//! - **Only the link count still costs a call**, and only for plain files:
//!   [`query_nlink`] via `NtQueryInformationByName(FileStatInformation)`,
//!   which its own Remarks describe as working "without opening the actual
//!   file". So the per-directory shape stays `openat`-like: 1 open,
//!   ⌈n/~1000⌉ listing calls, n_files link lookups.
//! - **Paths, not descriptors.** There is no `openat`, so a child is opened
//!   by its full `\\?\` path. That loses the Linux backend's immunity to
//!   hostile symlink swaps mid-walk; the compensating guard is that no
//!   reparse point is ever descended (see [`classify`]), so the walk cannot
//!   be redirected out of the tree even if a name is swapped.
//!
//! A scan never reads file *contents*, which is why OneDrive placeholders
//! are safe to enumerate: hydration is keyed to data access, and metadata
//! is not data access. Any future feature that reads bytes (hashing,
//! preview) breaks that invariant and must re-check it.

use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::Sender;
use crossbeam_deque::{Injector, Stealer, Worker as WorkerQueue};
use tracing::{debug, trace, warn};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_STAT_INFORMATION, FileStatInformation, IO_REPARSE_TAG_LX_BLK, IO_REPARSE_TAG_LX_CHR,
    IO_REPARSE_TAG_LX_FIFO, IO_REPARSE_TAG_LX_SYMLINK, NtQueryInformationByName,
};
use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, GetLastError, HANDLE, NTSTATUS, OBJ_DONT_REPARSE,
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER,
    STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_EXTD_DIR_INFO, FileIdExtdDirectoryInfo, GetFileInformationByHandleEx,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_AF_UNIX, IO_REPARSE_TAG_APPEXECLINK, IO_REPARSE_TAG_DEDUP,
    IO_REPARSE_TAG_FILE_PLACEHOLDER, IO_REPARSE_TAG_HSM, IO_REPARSE_TAG_HSM2,
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_ONEDRIVE, IO_REPARSE_TAG_SIS,
    IO_REPARSE_TAG_STORAGE_SYNC, IO_REPARSE_TAG_SYMLINK, IO_REPARSE_TAG_WIM, IO_REPARSE_TAG_WOF,
};

use crate::errno::from_win32;
use crate::tree::{ExcludedReason, Kind};

use super::{WidePath, entry_size, filetime_to_unix, open_dir};
use crate::scan::message::{Batch, BatchEntry, SECTION_CAP, SectionSums};

/// Directory-listing buffer, in bytes. 64 KiB holds ~1000 entries of
/// typical name length, so a big directory costs a handful of calls; the
/// kernel refuses outright if a single entry does not fit, which cannot
/// happen here (one entry is at most 88 + 2 × 255 bytes).
const LISTING_BUF_BYTES: usize = 64 * 1024;

/// Backoff while the queues are empty but jobs are still in flight.
const IDLE_BACKOFF: Duration = Duration::from_micros(200);

/// Byte offset of the variable-length name inside a listing entry.
const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);

/// Reparse tags, renormalised to `u32`.
///
/// The `LX_*` family is declared `i32` in `windows-sys` (they are the four
/// tags whose top bit makes the constant negative) while every other tag is
/// `u32`, and they live in `Wdk_Storage_FileSystem` rather than beside the
/// rest in `Win32_System_SystemServices`. Both are accidents of the
/// metadata, not of Windows; normalising here keeps [`classify`]'s match a
/// plain table.
const TAG_SYMLINK: u32 = IO_REPARSE_TAG_SYMLINK;
const TAG_MOUNT_POINT: u32 = IO_REPARSE_TAG_MOUNT_POINT;
const TAG_APPEXECLINK: u32 = IO_REPARSE_TAG_APPEXECLINK;
const TAG_AF_UNIX: u32 = IO_REPARSE_TAG_AF_UNIX;
const TAG_LX_SYMLINK: u32 = IO_REPARSE_TAG_LX_SYMLINK as u32;
const TAG_LX_FIFO: u32 = IO_REPARSE_TAG_LX_FIFO as u32;
const TAG_LX_CHR: u32 = IO_REPARSE_TAG_LX_CHR as u32;
const TAG_LX_BLK: u32 = IO_REPARSE_TAG_LX_BLK as u32;

/// Tags whose entry is an **ordinary file that happens to be stored
/// elsewhere** — Compact-OS/WIM compression, server dedup, hierarchical
/// storage, OneDrive placeholders. Their sizes are real and their contents
/// are reachable; blanket-skipping `FILE_ATTRIBUTE_REPARSE_POINT` would
/// silently drop a large slice of `C:\Windows` (WOF) and every OneDrive
/// file, which is the single most expensive mistake available here.
const ORDINARY_TAGS: &[u32] = &[
    IO_REPARSE_TAG_WOF,
    IO_REPARSE_TAG_WIM,
    IO_REPARSE_TAG_DEDUP,
    IO_REPARSE_TAG_SIS,
    IO_REPARSE_TAG_HSM,
    IO_REPARSE_TAG_HSM2,
    IO_REPARSE_TAG_ONEDRIVE,
    IO_REPARSE_TAG_STORAGE_SYNC,
    IO_REPARSE_TAG_FILE_PLACEHOLDER,
];

/// The cloud (OneDrive/CLD) tag family, `0x9000?01A`: sixteen tags that
/// differ only in one nibble, which is why they are matched by mask rather
/// than listed. Mirrors the `IsReparseTagCloud` macro.
fn is_cloud_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == 0x9000_001A
}

/// How a job reaches its directory.
pub(crate) enum JobDir {
    /// Already open (the scan root), with its own `\\?\` path.
    Opened(OwnedHandle, Arc<WidePath>),
    /// To be opened as `parent \ name`. The `Arc` is the Linux worker's
    /// `JobFd::At(Arc<OwnedFd>, …)` analogue with the opposite resource
    /// profile: no handle is held while children are queued, so there is no
    /// `RLIMIT_NOFILE` pathology — memory grows with queued-but-unopened
    /// directories instead.
    At(Arc<WidePath>, Vec<u16>),
}

pub(crate) struct Job {
    pub dir: JobDir,
    pub token: u64,
}

/// State shared by all workers (and the owner, for `abort`).
///
/// Differs from the Linux `WorkerShared` by exactly four fields:
/// `statx_supported` becomes [`WorkerShared::nt_stat_supported`],
/// `use_uring` disappears, `dev` becomes the scan-wide [`WorkerShared::vol`]
/// (no per-entry volume exists in a listing, and T1 never crosses one), and
/// [`WorkerShared::id_folded`] is added.
pub(crate) struct WorkerShared {
    pub injector: Injector<Job>,
    pub stealers: Vec<Stealer<Job>>,
    /// Jobs pushed but not fully processed. Workers exit when it reaches 0.
    pub pending_jobs: AtomicUsize,
    /// Flat unique directory tokens; one `fetch_add` per *directory*.
    pub next_token: AtomicU64,
    /// `NtQueryInformationByName(FileStatInformation)` availability.
    ///
    /// Measured working against a directory handle with a bare relative
    /// name on Windows Server 2022 / NTFS, which proves it works on *that*
    /// build and filesystem — not on Windows 10 1607, not on an SMB share.
    /// Flipped off for the rest of the scan on a status that means "this
    /// build/filesystem does not implement it"; a per-file failure (a
    /// denied file, a name that raced away) only costs that entry its link
    /// count.
    pub nt_stat_supported: AtomicBool,
    /// A 128-bit file id was folded rather than used verbatim, i.e. the
    /// volume is not NTFS-shaped and inode identity is probabilistic for
    /// this scan (see [`fold_file_id`]).
    pub id_folded: AtomicBool,
    /// Volume serial number: the `st_dev` analogue, constant for the whole
    /// scan because T1 refuses every mount point.
    pub vol: u64,
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
    // One listing buffer per worker, not per directory: 64 KiB reused for
    // every job this worker takes. `u64` elements give the 8-byte alignment
    // `FILE_ID_EXTD_DIR_INFO` needs for its `i64` fields.
    let mut buf: Vec<u64> = vec![0; LISTING_BUF_BYTES / size_of::<u64>()];
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
        let ok = process_job(job, &local, shared, tx, &mut buf);
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

/// Per-job context shared by the enumeration and the per-entry integration.
struct JobCtx<'a> {
    /// The directory being enumerated.
    handle: OwnedHandle,
    /// Its full `\\?\` path — the prefix of every child job's path.
    path: Arc<WidePath>,
    /// The directory's token (section identity).
    token: u64,
    local: &'a WorkerQueue<Job>,
    shared: &'a WorkerShared,
}

/// Accumulator for the section being built: entries, pre-sums, child count.
#[derive(Default)]
struct Section {
    entries: Vec<BatchEntry>,
    sums: SectionSums,
    child_dirs: u32,
}

impl Section {
    fn take(&mut self, dir_token: u64, is_last_section: bool) -> Batch {
        Batch {
            dir_token,
            entries: std::mem::take(&mut self.entries),
            sums: std::mem::take(&mut self.sums),
            is_last_section,
            child_dirs: std::mem::take(&mut self.child_dirs),
            dir_error: None,
        }
    }
}

/// Enumerate one directory, streaming sections to the owner. Returns false
/// only when the owner is gone (send failed).
fn process_job(
    job: Job,
    local: &WorkerQueue<Job>,
    shared: &WorkerShared,
    tx: &Sender<Batch>,
    buf: &mut [u64],
) -> bool {
    let token = job.token;
    let (handle, path) = match job.dir {
        JobDir::Opened(handle, path) => (handle, path),
        JobDir::At(parent, name) => {
            let path = Arc::new(parent.join(&name));
            // `FILE_FLAG_OPEN_REPARSE_POINT` below the root is the
            // path-based walk's substitute for `O_NOFOLLOW`: a name that
            // turned into a symlink between the listing and this open must
            // fail to open as a directory rather than redirect the walk.
            match open_dir(&path, FILE_FLAG_OPEN_REPARSE_POINT) {
                Ok(handle) => (handle, path),
                Err(code) => {
                    debug!(path = %path, code, "directory open failed");
                    return send_batch(tx, shared, error_batch(token, code));
                }
            }
        }
    };
    let ctx = JobCtx {
        handle,
        path,
        token,
        local,
        shared,
    };

    let mut section = Section::default();
    if !enumerate(&ctx, &mut section, buf, tx) {
        return false;
    }

    // Final (possibly empty) section: carries the self-token release.
    // Every entry is fully resolved by construction — the listing is the
    // stat — so the owner never sees a section with outstanding work.
    send_batch(tx, shared, section.take(token, true))
}

/// Drive `GetFileInformationByHandleEx` to exhaustion, integrating each
/// buffer-full of entries.
///
/// The first call fills from the start of the directory and each later one
/// continues, until `ERROR_NO_MORE_FILES` — the enumeration terminator, not
/// an error. Any other failure keeps the entries already seen and counts
/// the directory as partially errored, matching the Linux backend's
/// mid-`getdents` behaviour.
fn enumerate(ctx: &JobCtx<'_>, section: &mut Section, buf: &mut [u64], tx: &Sender<Batch>) -> bool {
    let raw: HANDLE = ctx.handle.as_raw_handle().cast();
    let byte_len = size_of_val(buf);
    loop {
        // SAFETY: `raw` is borrowed from the live `OwnedHandle` in `ctx`,
        // opened with FILE_LIST_DIRECTORY. The buffer is a live `&mut [u64]`
        // and `byte_len` is exactly its size in bytes, so the kernel writes
        // only inside it; `u64` elements guarantee the 8-byte alignment
        // `FILE_ID_EXTD_DIR_INFO` requires.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                raw,
                FileIdExtdDirectoryInfo,
                buf.as_mut_ptr().cast(),
                u32::try_from(byte_len).expect("listing buffer fits in u32"),
            )
        };
        if ok == 0 {
            // SAFETY: last-error read immediately after the failing call.
            let code = unsafe { GetLastError() };
            if code != ERROR_NO_MORE_FILES {
                warn!(
                    token = ctx.token,
                    code, "directory listing failed mid-directory"
                );
                section.sums.errors += 1;
            }
            return true;
        }
        if !walk_buffer(ctx, section, buf, byte_len, tx) {
            return false;
        }
    }
}

/// Walk one filled listing buffer, entry by entry, following
/// `NextEntryOffset`.
///
/// The API reports no filled length, so the chain itself is the only
/// bound the kernel gives; every offset is therefore re-checked against the
/// buffer before it is dereferenced. A driver that returned a corrupt chain
/// would otherwise walk us straight off the end.
fn walk_buffer(
    ctx: &JobCtx<'_>,
    section: &mut Section,
    buf: &[u64],
    byte_len: usize,
    tx: &Sender<Batch>,
) -> bool {
    let base = buf.as_ptr().cast::<u8>();
    let mut offset = 0usize;
    loop {
        if offset + size_of::<FILE_ID_EXTD_DIR_INFO>() > byte_len {
            warn!(
                token = ctx.token,
                offset, "listing entry runs past the buffer"
            );
            return true;
        }
        // SAFETY: `offset` was just bounds-checked against the buffer, so
        // `base + offset` addresses at least a whole `FILE_ID_EXTD_DIR_INFO`
        // inside the live borrow. `read_unaligned` rather than a reference
        // because the alignment of a mid-buffer entry is the kernel's
        // promise, not ours to rely on.
        let info = unsafe {
            base.add(offset)
                .cast::<FILE_ID_EXTD_DIR_INFO>()
                .read_unaligned()
        };
        let name_bytes = info.FileNameLength as usize;
        let name_end = offset + NAME_OFFSET + name_bytes;
        if !name_bytes.is_multiple_of(2) || name_end > byte_len {
            warn!(
                token = ctx.token,
                name_bytes, "listing name runs past the buffer"
            );
            return true;
        }
        let mut name = vec![0u16; name_bytes / 2];
        // SAFETY: the destination is a fresh `Vec<u16>` of exactly
        // `name_bytes` bytes; the source range was bounds-checked above and
        // lies in the same live buffer. The copy is byte-wise, so the
        // source needs no alignment, and the two cannot overlap (one is a
        // fresh allocation).
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add(offset + NAME_OFFSET),
                name.as_mut_ptr().cast::<u8>(),
                name_bytes,
            );
        }

        if !is_dot_entry(&name) {
            handle_entry(ctx, section, &info, &name);
            if section.entries.len() >= SECTION_CAP
                && !send_batch(tx, ctx.shared, section.take(ctx.token, false))
            {
                return false;
            }
        }

        if info.NextEntryOffset == 0 {
            return true;
        }
        offset += info.NextEntryOffset as usize;
    }
}

/// `.` and `..`, which the Windows listing includes exactly as
/// `getdents64` does.
fn is_dot_entry(name: &[u16]) -> bool {
    const DOT: u16 = b'.' as u16;
    matches!(name, [DOT] | [DOT, DOT])
}

/// What a listing entry is, and what to do with it.
struct Class {
    kind: Kind,
    /// Descend into it (implies `kind == Kind::Dir`).
    descend: bool,
    /// Recorded but not descended into. Only ever set for directories: the
    /// owner counts every `excluded` entry as an excluded *directory*.
    excluded: Option<ExcludedReason>,
}

/// Classify an entry from its attributes and reparse tag.
///
/// The rule that matters is the negative one: a reparse point is never
/// descended into, whatever its tag says, because a junction can point at
/// its own ancestor and this backend has no cycle detection. The tag then
/// decides how the entry is *recorded* — as a link, as an excluded mount
/// point, or as the ordinary file it really is.
fn classify(attributes: u32, tag: u32) -> Class {
    let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Class {
            kind: if is_dir { Kind::Dir } else { Kind::File },
            descend: is_dir,
            excluded: None,
        };
    }
    let plain = |kind: Kind| Class {
        kind,
        descend: false,
        excluded: None,
    };
    match tag {
        // lstat parity: the link itself, never its target.
        TAG_SYMLINK | TAG_LX_SYMLINK => plain(Kind::Symlink),
        // A store-app alias. Not a symlink in the POSIX sense, but it is a
        // zero-byte stand-in for something else, which is what a symlink
        // row communicates to a user reading the tree.
        TAG_APPEXECLINK => plain(Kind::Symlink),
        // The WSL device-node family comes free with the tag.
        TAG_LX_FIFO => plain(Kind::Fifo),
        TAG_LX_CHR => plain(Kind::Char),
        TAG_LX_BLK => plain(Kind::Block),
        TAG_AF_UNIX => plain(Kind::Socket),
        // Junction *and* volume mount point share this tag, and T1 refuses
        // both — see `super::start` for why the same-volume case goes too.
        TAG_MOUNT_POINT => Class {
            kind: Kind::Dir,
            descend: false,
            excluded: Some(ExcludedReason::OtherFs),
        },
        tag if ORDINARY_TAGS.contains(&tag) || is_cloud_tag(tag) => Class {
            kind: if is_dir { Kind::Dir } else { Kind::File },
            // A cloud- or WOF-tagged *directory* is a genuine directory on
            // this volume; refusing it would drop the whole OneDrive tree.
            descend: is_dir,
            excluded: None,
        },
        // Unknown tag: record it, never follow it. A directory becomes an
        // excluded boundary; anything else is recorded with its real sizes
        // but an undetermined kind, which is exactly what `Kind::Other`
        // means.
        _ if is_dir => Class {
            kind: Kind::Dir,
            descend: false,
            excluded: Some(ExcludedReason::OtherFs),
        },
        _ => plain(Kind::Other),
    }
}

/// Two 128-bit file ids that mean *unknown*, not an inode: all-`0xFF` is
/// "no unique id available" and all-zero is "this filesystem issues none"
/// (FAT, exFAT, UDF). Reading either as an inode would fuse every file on a
/// scanned USB stick into one hardlink group.
fn is_sentinel_id(id: [u8; 16]) -> bool {
    id == [0xFF; 16] || id == [0; 16]
}

/// Collapse a 128-bit file id into the 64-bit inode the tree stores.
///
/// MS-FSCC Appendix B fn.11: on NTFS the low 64 bits *are* the classic file
/// reference number (48-bit MFT index + 16-bit sequence) and the high half
/// MUST be zero, so that case is exact and checkable against `fsutil file
/// queryfileid`. On ReFS the split is different — low half identifies the
/// **parent directory**, high half the file within it — so truncating there
/// would give every sibling the same inode and the hardlink pass would
/// dedup an entire directory away. Hence: fold, never truncate.
///
/// At 10 M hardlinked inodes the fold collides with probability ~2.7e-6,
/// and its failure mode is truncation's — two inodes fused — 2^64 times
/// rarer. `folded` records that it happened so the fact can be reported
/// rather than passed off as NTFS-grade precision.
fn fold_file_id(id: [u8; 16], folded: &AtomicBool) -> u64 {
    let lo = u64::from_le_bytes(id[0..8].try_into().expect("8 bytes"));
    let hi = u64::from_le_bytes(id[8..16].try_into().expect("8 bytes"));
    if hi == 0 {
        return lo;
    }
    if !folded.swap(true, Ordering::Relaxed) {
        warn!(
            "128-bit file ids in use (ReFS-shaped volume): inode identity is folded to 64 bits, \
             so hardlink grouping is probabilistic for this scan"
        );
    }
    lo.rotate_left(31) ^ hi.wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// Integrate one enumerated entry into the section: `BatchEntry`
/// construction, child-job push, pre-sums.
fn handle_entry(
    ctx: &JobCtx<'_>,
    section: &mut Section,
    info: &FILE_ID_EXTD_DIR_INFO,
    name_w: &[u16],
) {
    let class = classify(info.FileAttributes, info.ReparsePointTag);
    let size = entry_size(info.EndOfFile, info.AllocationSize);
    let is_reparse = info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let sentinel = is_sentinel_id(info.FileId.Identifier);
    let ino = if sentinel {
        0
    } else {
        fold_file_id(info.FileId.Identifier, &ctx.shared.id_folded)
    };
    // The link count is the one field the listing does not carry, and the
    // only per-entry call in this backend. Skipped for directories (the
    // owner never reads a directory's nlink), for reparse points (belt and
    // braces: the call is already lstat-shaped, but an entry we will never
    // descend into does not need one), and for volumes that issue no ids,
    // where a link count without an inode to key it on is unusable.
    let nlink = if class.kind == Kind::Dir || is_reparse || sentinel {
        1
    } else {
        query_nlink(&ctx.handle, name_w, &ctx.shared.nt_stat_supported)
    };

    let name = wtf8_name(name_w);
    trace!(
        name = %String::from_utf8_lossy(&name),
        apparent = size.apparent,
        disk = size.real,
        nlink,
        "listing entry"
    );
    let mut entry = BatchEntry {
        name,
        kind: class.kind,
        apparent: size.apparent,
        disk: size.real,
        mtime: filetime_to_unix(info.LastWriteTime),
        nlink,
        ino,
        dev: ctx.shared.vol,
        // Nothing to record: this backend has no per-entry stat that can
        // fail on its own. An entry either came out of the listing whole or
        // its whole directory failed.
        error: None,
        child_token: None,
        excluded: class.excluded,
    };
    if class.descend {
        let child_token = ctx.shared.next_token.fetch_add(1, Ordering::Relaxed);
        entry.child_token = Some(child_token);
        section.child_dirs += 1;
        ctx.shared.pending_jobs.fetch_add(1, Ordering::AcqRel);
        ctx.local.push(Job {
            dir: JobDir::At(Arc::clone(&ctx.path), name_w.to_vec()),
            token: child_token,
        });
    }
    section.sums.apparent += entry.apparent;
    section.sums.disk += entry.disk;
    section.sums.count += 1;
    section.entries.push(entry);
}

/// A UTF-16 listing name as the platform's own `OsStr` bytes (WTF-8).
///
/// Not `String::from_utf16_lossy`: an unpaired surrogate in a name is legal
/// on NTFS, and replacing it would make two distinct names collide in the
/// tree's interner. `OsString::from_wide` preserves it, and the root name
/// `scan.rs` interns comes from the same encoding, so the whole tree is in
/// one alphabet.
fn wtf8_name(name_w: &[u16]) -> Vec<u8> {
    std::ffi::OsString::from_wide(name_w)
        .as_encoded_bytes()
        .to_vec()
}

/// `NtQueryInformationByName(FileStatInformation)` against an open
/// directory handle and a bare relative leaf name.
///
/// `OBJECT_ATTRIBUTES.RootDirectory` giving a directory-relative lookup is
/// documented nowhere on that page; it was measured working. `OBJ_DONT_REPARSE`
/// is set for explicitness only — the call was measured to report a symlink
/// and a junction as themselves with *and* without it, so no safety
/// argument rests on the flag.
///
/// Failure is never an error for the scan: the entry's sizes already came
/// from the listing, and only the link count is lost. `1` is the honest
/// fallback (it is what a non-hardlinked file has, and it keeps the entry
/// out of the hardlink registry rather than fusing it with something).
fn query_nlink(dir: &OwnedHandle, name_w: &[u16], supported: &AtomicBool) -> u32 {
    if !supported.load(Ordering::Relaxed) {
        return 1;
    }
    let Ok(byte_len) = u16::try_from(name_w.len() * 2) else {
        // A name too long for UNICODE_STRING's u16 length cannot exist
        // under the 255-character component limit, but the cast has to be
        // checked rather than assumed.
        return 1;
    };
    let name = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: name_w.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).expect("OBJECT_ATTRIBUTES fits"),
        RootDirectory: dir.as_raw_handle().cast(),
        ObjectName: &raw const name,
        Attributes: OBJ_DONT_REPARSE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut iosb = IO_STATUS_BLOCK::default();
    let mut stat = FILE_STAT_INFORMATION::default();
    // SAFETY: `attributes` borrows `name`, which borrows `name_w`; all
    // three outlive the call, which is synchronous. `RootDirectory` is
    // borrowed from a live `OwnedHandle`. The output buffer is `stat`'s own
    // storage and the length passed is exactly its size, so the kernel
    // writes nothing outside it. The two SECURITY_* pointers are null,
    // which the structure documents as "none".
    let status: NTSTATUS = unsafe {
        NtQueryInformationByName(
            &raw const attributes,
            &raw mut iosb,
            std::ptr::from_mut(&mut stat).cast(),
            u32::try_from(size_of::<FILE_STAT_INFORMATION>()).expect("FILE_STAT_INFORMATION fits"),
            FileStatInformation,
        )
    };
    if status < 0 {
        if is_unsupported_status(status) {
            // The whole scan gives up on link counts: an information class
            // this build or filesystem does not implement will not start
            // working on the next file, and retrying it per entry would
            // double the syscall count for nothing.
            if !supported.swap(false, Ordering::Relaxed) {
                warn!(
                    status = format!("{status:#010x}"),
                    "NtQueryInformationByName(FileStatInformation) unsupported here; \
                     hardlinks will not be deduplicated for this scan"
                );
            }
        } else {
            trace!(
                status = format!("{status:#010x}"),
                "link-count lookup failed"
            );
        }
        return 1;
    }
    stat.NumberOfLinks
}

/// Statuses that mean "this build or filesystem does not implement the
/// call", as opposed to "this particular file said no".
fn is_unsupported_status(status: NTSTATUS) -> bool {
    matches!(
        status,
        STATUS_NOT_IMPLEMENTED
            | STATUS_NOT_SUPPORTED
            | STATUS_INVALID_PARAMETER
            | STATUS_INVALID_INFO_CLASS
            | STATUS_INVALID_DEVICE_REQUEST
    )
}

fn error_batch(token: u64, code: u32) -> Batch {
    Batch {
        dir_token: token,
        entries: Vec::new(),
        sums: SectionSums::default(),
        is_last_section: true,
        child_dirs: 0,
        // Win32 code → canonical reason at the platform boundary, through
        // the table that knows `ERROR_ACCESS_DENIED` is 5 and 5 is not EIO.
        dir_error: Some(from_win32(code)),
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

#[cfg(test)]
mod tests {
    //! Unit coverage for the pure seams of the Windows scan path: entry
    //! classification (the one place a wrong answer silently drops a slice
    //! of `C:\Windows`), the file-id fold and its sentinels, and the
    //! terminal error-batch contract.

    use super::*;
    use crate::errno::ScanErrno;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// The ordinary cases: attributes alone decide, and a plain directory
    /// is the only thing descended into.
    #[test]
    fn plain_entries_classify_by_attribute() {
        let file = classify(0x20, 0);
        assert_eq!(file.kind, Kind::File);
        assert!(!file.descend);

        let dir = classify(FILE_ATTRIBUTE_DIRECTORY, 0);
        assert_eq!(dir.kind, Kind::Dir);
        assert!(dir.descend);
        assert!(dir.excluded.is_none());
    }

    /// Compact-OS (WOF) and OneDrive entries are ordinary files. Getting
    /// this wrong by blanket-skipping `FILE_ATTRIBUTE_REPARSE_POINT` would
    /// silently drop much of `C:\Windows` and every cloud file — the single
    /// most expensive mistake in this module.
    #[test]
    fn storage_tiering_tags_stay_ordinary_files() {
        for tag in [
            IO_REPARSE_TAG_WOF,
            IO_REPARSE_TAG_DEDUP,
            IO_REPARSE_TAG_ONEDRIVE,
            0x9000_001A, // IO_REPARSE_TAG_CLOUD
            0x9000_A01A, // IO_REPARSE_TAG_CLOUD_A
        ] {
            let class = classify(FILE_ATTRIBUTE_REPARSE_POINT | 0x20, tag);
            assert_eq!(class.kind, Kind::File, "tag {tag:#010x}");
            assert!(class.excluded.is_none(), "tag {tag:#010x}");
        }
    }

    /// A cloud-backed *directory* is a real directory on this volume;
    /// refusing it would drop the whole OneDrive tree rather than just its
    /// placeholder files.
    #[test]
    fn cloud_directories_are_descended() {
        let class = classify(
            FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
            0x9000_001A,
        );
        assert_eq!(class.kind, Kind::Dir);
        assert!(class.descend);
    }

    /// The cloud family is sixteen tags differing in one nibble; the mask
    /// must accept all of them and nothing adjacent.
    #[test]
    fn cloud_mask_matches_the_family_only() {
        assert!(is_cloud_tag(0x9000_001A));
        assert!(is_cloud_tag(0x9000_F01A));
        assert!(!is_cloud_tag(0x9000_001B));
        assert!(!is_cloud_tag(IO_REPARSE_TAG_WOF));
        assert!(!is_cloud_tag(0));
    }

    /// Links and mount points: recorded, never descended. A junction can
    /// point at its own ancestor and there is no cycle detection, so
    /// `descend` must be false for every one of them.
    #[test]
    fn links_and_mount_points_are_never_descended() {
        let symlink = classify(FILE_ATTRIBUTE_REPARSE_POINT, TAG_SYMLINK);
        assert_eq!(symlink.kind, Kind::Symlink);
        assert!(!symlink.descend);

        let junction = classify(
            FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
            TAG_MOUNT_POINT,
        );
        assert_eq!(junction.kind, Kind::Dir);
        assert!(!junction.descend);
        assert_eq!(junction.excluded, Some(ExcludedReason::OtherFs));
    }

    /// The WSL tags carry a POSIX file type for free.
    #[test]
    fn wsl_tags_map_to_posix_kinds() {
        assert_eq!(
            classify(FILE_ATTRIBUTE_REPARSE_POINT, TAG_LX_SYMLINK).kind,
            Kind::Symlink
        );
        assert_eq!(
            classify(FILE_ATTRIBUTE_REPARSE_POINT, TAG_LX_FIFO).kind,
            Kind::Fifo
        );
        assert_eq!(
            classify(FILE_ATTRIBUTE_REPARSE_POINT, TAG_LX_CHR).kind,
            Kind::Char
        );
        assert_eq!(
            classify(FILE_ATTRIBUTE_REPARSE_POINT, TAG_LX_BLK).kind,
            Kind::Block
        );
        assert_eq!(
            classify(FILE_ATTRIBUTE_REPARSE_POINT, TAG_AF_UNIX).kind,
            Kind::Socket
        );
    }

    /// An unknown tag is recorded, never followed — and `excluded` is only
    /// ever set on a directory, because the owner counts every `excluded`
    /// entry as an excluded *directory*.
    #[test]
    fn unknown_tags_are_recorded_but_not_followed() {
        let file = classify(FILE_ATTRIBUTE_REPARSE_POINT, 0x8000_0099);
        assert_eq!(file.kind, Kind::Other);
        assert!(!file.descend);
        assert!(
            file.excluded.is_none(),
            "a non-directory must never be counted as an excluded directory"
        );

        let dir = classify(
            FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
            0x8000_0099,
        );
        assert_eq!(dir.excluded, Some(ExcludedReason::OtherFs));
        assert!(!dir.descend);
    }

    /// NTFS keeps the high half zero, so the id passes through exactly and
    /// stays comparable with `fsutil file queryfileid`; nothing is flagged
    /// as folded.
    #[test]
    fn ntfs_ids_pass_through_unfolded() {
        let mut id = [0u8; 16];
        id[0..8].copy_from_slice(&0x0001_0000_0000_2A3Cu64.to_le_bytes());
        let folded = AtomicBool::new(false);
        assert_eq!(fold_file_id(id, &folded), 0x0001_0000_0000_2A3C);
        assert!(!folded.load(Ordering::Relaxed));
    }

    /// A ReFS-shaped id — where the low half identifies the *parent
    /// directory* — must not be truncated: siblings differ only in the high
    /// half, and truncating would give them one inode and dedup the
    /// directory away.
    #[test]
    fn refs_siblings_do_not_collapse_onto_one_inode() {
        let mut a = [0u8; 16];
        a[0..8].copy_from_slice(&777u64.to_le_bytes());
        a[8..16].copy_from_slice(&1u64.to_le_bytes());
        let mut b = a;
        b[8..16].copy_from_slice(&2u64.to_le_bytes());

        let folded = AtomicBool::new(false);
        let ia = fold_file_id(a, &folded);
        let ib = fold_file_id(b, &folded);
        assert_ne!(ia, ib, "siblings must not fuse");
        assert!(folded.load(Ordering::Relaxed), "folding must be recorded");
    }

    /// Both "unknown id" sentinels must be refused. Reading either as an
    /// inode turns every file on a FAT/exFAT stick into one hardlink group.
    #[test]
    fn sentinel_ids_are_refused() {
        assert!(is_sentinel_id([0xFF; 16]));
        assert!(is_sentinel_id([0; 16]));
        let mut real = [0u8; 16];
        real[0] = 1;
        assert!(!is_sentinel_id(real));
    }

    /// `.` and `..` are in the Windows listing exactly as in `getdents64`,
    /// and a name that merely starts with a dot is not one of them.
    #[test]
    fn dot_entries_are_skipped_but_dotfiles_are_not() {
        assert!(is_dot_entry(&wide(".")));
        assert!(is_dot_entry(&wide("..")));
        assert!(!is_dot_entry(&wide(".git")));
        assert!(!is_dot_entry(&wide("...")));
        assert!(!is_dot_entry(&wide("")));
    }

    /// Names survive the UTF-16 → WTF-8 hop, including outside the BMP
    /// where a naive per-unit conversion would mangle the surrogate pair.
    #[test]
    fn names_round_trip_through_wtf8() {
        assert_eq!(wtf8_name(&wide("Program Files")), b"Program Files");
        assert_eq!(wtf8_name(&wide("café")), "café".as_bytes());
        assert_eq!(wtf8_name(&wide("emoji-🧀")), "emoji-🧀".as_bytes());
    }

    /// A directory that could not be opened produces a terminal batch: the
    /// reason is carried in `dir_error`, it is the last section, and it
    /// carries no entries or child dirs.
    #[test]
    fn error_batch_is_terminal_and_carries_the_reason() {
        // ERROR_ACCESS_DENIED — the one code whose Win32 number (5) is a
        // different errno (EIO) in POSIX numbering.
        let batch = error_batch(42, 5);
        assert_eq!(batch.dir_token, 42);
        assert_eq!(batch.dir_error, Some(ScanErrno::ACCESS));
        assert!(batch.is_last_section);
        assert!(batch.entries.is_empty());
        assert_eq!(batch.child_dirs, 0);
        assert_eq!(batch.sums.count, 0);
    }

    /// Sharing and lock violations are the errors a Windows scan hits most
    /// after access-denied, and both land in `ErrorKind::Uncategorized` —
    /// they exist in the taxonomy precisely so they are not lost.
    #[test]
    fn sharing_violations_reach_their_own_taxonomy_entry() {
        assert_eq!(
            error_batch(1, 32).dir_error,
            Some(ScanErrno::SHARING_VIOLATION)
        );
        assert_eq!(
            error_batch(1, 33).dir_error,
            Some(ScanErrno::SHARING_VIOLATION)
        );
    }

    /// Only "this build does not implement it" statuses disable the link
    /// lookup for the rest of the scan; one denied file must not cost every
    /// later file its link count.
    #[test]
    fn only_unsupported_statuses_disable_the_link_lookup() {
        assert!(is_unsupported_status(STATUS_NOT_IMPLEMENTED));
        assert!(is_unsupported_status(STATUS_NOT_SUPPORTED));
        assert!(is_unsupported_status(STATUS_INVALID_INFO_CLASS));
        // STATUS_ACCESS_DENIED / STATUS_OBJECT_NAME_NOT_FOUND: per-file.
        assert!(!is_unsupported_status(0xC000_0022_u32 as NTSTATUS));
        assert!(!is_unsupported_status(0xC000_0034_u32 as NTSTATUS));
    }
}
