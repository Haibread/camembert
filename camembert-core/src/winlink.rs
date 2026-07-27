//! The Windows by-name link-count query, in one place.
//!
//! `NtQueryInformationByName(FileStatInformation)` is the only non-elevated
//! way to learn how many links an NTFS file has
//! (`docs/design/windows-nlink-dossier.md` §2.5 exhausts the alternatives).
//! It costs ~46 µs — 97.6 % of that the `IRP_MJ_CREATE` every registered
//! minifilter gets to inspect — which is why the scan stopped issuing it per
//! entry (dossier §6.1) and why it now lives here instead of inside the
//! worker: **one caller pays it per file walked, another pays it per file a
//! user actually asks about**, and both must go through the same syscall,
//! the same status taxonomy and the same `unsafe` block.
//!
//! This module is the Windows analogue of [`crate::fiemap`]'s isolation on
//! Linux: a thin, safe wrapper over one raw call, `cfg`-gated at
//! `lib.rs` so no other platform can even name it.
//!
//! # Two entry points, one call
//!
//! - [`links_at`] — a directory handle plus a bare leaf name. The scan
//!   worker's shape (it already holds the listing handle), crate-internal.
//! - [`LinkDir`] — open a directory, then query names in it. The
//!   consumption-point shape (a TUI row knows its parent directory and its
//!   name, and holds no handle from the scan).
//!
//! Directory-relative rather than absolute is a *code-sharing* choice, not
//! a performance one: the dossier measured an absolute `\??\` path at
//! 47.91 µs against 48.31 µs directory-relative, i.e. the same call. Going
//! through a handle means [`LinkDir`] and the scan worker exercise one code
//! path, and it sidesteps NT-path syntax (`\??\`, `\??\UNC\…`) entirely.
//!
//! # What a caller must decide for itself
//!
//! Nothing here interprets a link count. In particular this module does
//! **not** know which entries are worth querying — the scan skips
//! directories, reparse points and sentinel-id volumes for reasons that are
//! about *registry admission* (see `scan::windows::worker::handle_entry`),
//! and a consumer asking "what is this file" has its own, different rules.

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::path::Path;

use tracing::trace;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_STAT_INFORMATION, FileStatInformation, NtQueryInformationByName,
};
use windows_sys::Win32::Foundation::{
    NTSTATUS, OBJ_DONT_REPARSE, STATUS_ACCESS_DENIED, STATUS_DELETE_PENDING,
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER,
    STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SHARING_VIOLATION, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// What one by-name query returned. Three outcomes, never two: "the
/// filesystem cannot do this" and "this file said no" are different facts
/// and a caller that shows them the same way is lying about one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCount {
    /// The filesystem answered.
    Known {
        /// `NumberOfLinks`: links that exist **on the volume**, whether or
        /// not any given walk saw them.
        links: u32,
        /// The 64-bit file id, for callers that need to reconcile the
        /// answer with an identity they already hold. `0` and `-1` are the
        /// documented "this volume issues no unique id" sentinels
        /// (FAT/exFAT/UDF, some SMB servers) — the same pair
        /// `scan::windows::worker::is_sentinel_id` refuses.
        file_id: i64,
        /// `FILE_ATTRIBUTE_*` as of the query.
        attributes: u32,
    },
    /// This build or filesystem does not implement `FileStatInformation`
    /// at all. Retrying costs a syscall and cannot succeed; a scan-wide
    /// consumer should latch this and stop asking.
    Unsupported,
    /// This particular file said no. Another file in the same directory may
    /// well answer.
    Failed(QueryFailure),
}

/// Why one file's query failed, in the terms a user can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryFailure {
    /// `STATUS_ACCESS_DENIED` — the common one on `C:\Windows`.
    Denied,
    /// The name is gone (or being deleted) since it was listed.
    Vanished,
    /// Something else holds the file in a way that refuses the create.
    Sharing,
    /// Anything else, including a failure to open the directory itself.
    Other,
}

impl QueryFailure {
    /// One short clause, meant to be appended after "unknown".
    pub fn label(self) -> &'static str {
        match self {
            Self::Denied => "access denied",
            Self::Vanished => "the entry is gone",
            Self::Sharing => "the file is locked by another process",
            Self::Other => "the query failed",
        }
    }

    fn of(status: NTSTATUS) -> Self {
        match status {
            STATUS_ACCESS_DENIED => Self::Denied,
            STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_DELETE_PENDING => {
                Self::Vanished
            }
            STATUS_SHARING_VIOLATION => Self::Sharing,
            _ => Self::Other,
        }
    }

    /// The same three reasons, from the Win32 side: [`LinkDir::open`] fails
    /// with a `GetLastError` code rather than an `NTSTATUS`, and a denied
    /// **directory** is by far the likeliest way a user meets any of this
    /// (a per-file ACL does not refuse `FileStatInformation`, which is a
    /// traverse-only metadata query — measured).
    ///
    /// The numbers are Win32's, deliberately not routed through
    /// [`crate::errno`]: that taxonomy answers "what went wrong with the
    /// scan", which is a different question with a different vocabulary,
    /// and borrowing it here would put a scan-error severity on a card line
    /// that is not a scan error.
    pub fn of_win32(code: Option<i32>) -> Self {
        match code {
            Some(5) => Self::Denied,        // ERROR_ACCESS_DENIED
            Some(2 | 3) => Self::Vanished,  // FILE_NOT_FOUND / PATH_NOT_FOUND
            Some(32 | 33) => Self::Sharing, // SHARING_ / LOCK_VIOLATION
            _ => Self::Other,
        }
    }
}

/// Statuses that mean "this build or filesystem does not implement the
/// call", as opposed to "this particular file said no".
///
/// Public because the distinction is the whole point of [`LinkCount`]'s
/// three-way split and both consumers reason about it; kept as a named
/// predicate so the list lives in exactly one place.
pub fn is_unsupported_status(status: NTSTATUS) -> bool {
    matches!(
        status,
        STATUS_NOT_IMPLEMENTED
            | STATUS_NOT_SUPPORTED
            | STATUS_INVALID_PARAMETER
            | STATUS_INVALID_INFO_CLASS
            | STATUS_INVALID_DEVICE_REQUEST
    )
}

/// `NtQueryInformationByName(FileStatInformation)` against an open
/// directory handle and a bare relative leaf name.
///
/// `OBJECT_ATTRIBUTES.RootDirectory` giving a directory-relative lookup is
/// documented nowhere on that page; it was measured working.
/// `OBJ_DONT_REPARSE` is set for explicitness only — the call was measured
/// to report a symlink and a junction as themselves with *and* without it,
/// so no safety argument rests on the flag.
pub(crate) fn links_at(dir: BorrowedHandle<'_>, name_w: &[u16]) -> LinkCount {
    let Ok(byte_len) = u16::try_from(name_w.len() * 2) else {
        // A name too long for UNICODE_STRING's u16 length cannot exist
        // under the 255-character component limit, but the cast has to be
        // checked rather than assumed.
        return LinkCount::Failed(QueryFailure::Other);
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
    // borrowed from a live handle (`BorrowedHandle` is what proves it). The
    // output buffer is `stat`'s own storage and the length passed is exactly
    // its size, so the kernel writes nothing outside it. The two SECURITY_*
    // pointers are null, which the structure documents as "none".
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
        trace!(
            status = format!("{status:#010x}"),
            "link-count lookup failed"
        );
        if is_unsupported_status(status) {
            return LinkCount::Unsupported;
        }
        return LinkCount::Failed(QueryFailure::of(status));
    }
    LinkCount::Known {
        links: stat.NumberOfLinks,
        file_id: stat.FileId,
        attributes: stat.FileAttributes,
    }
}

/// A directory held open so names inside it can be queried by
/// [`LinkDir::links_of`].
///
/// The handle is the only reason this type exists: a query needs a
/// `RootDirectory`, and opening one per name would double an already
/// create-bound cost. Callers that ask about several entries of one
/// directory should therefore open it once.
pub struct LinkDir {
    handle: File,
}

impl LinkDir {
    /// Open `dir` for by-name queries.
    ///
    /// Access, sharing and flags mirror what the scan opens a directory
    /// with (`scan::windows::DIR_ACCESS`/`DIR_SHARE`), for the same
    /// reasons: `FILE_LIST_DIRECTORY` because attributes alone are refused
    /// on many directories, all three share bits so merely having a file
    /// open elsewhere does not collide, `FILE_FLAG_BACKUP_SEMANTICS`
    /// because a directory cannot be opened without it, and
    /// `FILE_FLAG_OPEN_REPARSE_POINT` so a name that turned into a link
    /// since the scan fails to open rather than redirecting the query
    /// somewhere else.
    ///
    /// Built on [`std::fs::OpenOptions`] + `OpenOptionsExt` rather than a
    /// second raw `CreateFileW`: same call, no second `unsafe` block.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        let handle = File::options()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(dir)?;
        Ok(Self { handle })
    }

    /// Query one entry of this directory by its bare leaf name.
    pub fn links_of(&self, name: &OsStr) -> LinkCount {
        let name_w: Vec<u16> = name.encode_wide().collect();
        links_at(self.handle.as_handle(), &name_w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only "this build does not implement it" statuses latch the lookup
    /// off; one denied file must not cost every later file its link count.
    #[test]
    fn only_unsupported_statuses_are_unsupported() {
        assert!(is_unsupported_status(STATUS_NOT_IMPLEMENTED));
        assert!(is_unsupported_status(STATUS_NOT_SUPPORTED));
        assert!(is_unsupported_status(STATUS_INVALID_INFO_CLASS));
        assert!(!is_unsupported_status(STATUS_ACCESS_DENIED));
        assert!(!is_unsupported_status(STATUS_OBJECT_NAME_NOT_FOUND));
    }

    /// The three statuses a user can do something about get their own
    /// words; everything else falls back rather than being guessed at.
    #[test]
    fn failures_keep_the_reasons_apart() {
        assert_eq!(QueryFailure::of(STATUS_ACCESS_DENIED), QueryFailure::Denied);
        assert_eq!(
            QueryFailure::of(STATUS_OBJECT_NAME_NOT_FOUND),
            QueryFailure::Vanished
        );
        assert_eq!(
            QueryFailure::of(STATUS_DELETE_PENDING),
            QueryFailure::Vanished
        );
        assert_eq!(
            QueryFailure::of(STATUS_SHARING_VIOLATION),
            QueryFailure::Sharing
        );
        assert_eq!(
            QueryFailure::of(0xC000_0001_u32 as NTSTATUS),
            QueryFailure::Other
        );
    }

    /// The Win32 half of the same taxonomy, for a directory that would not
    /// open. 5 is `ERROR_ACCESS_DENIED` here and `EIO` in POSIX numbering,
    /// which is exactly why this is a table and never a cast.
    #[test]
    fn directory_open_failures_map_to_the_same_reasons() {
        assert_eq!(QueryFailure::of_win32(Some(5)), QueryFailure::Denied);
        assert_eq!(QueryFailure::of_win32(Some(3)), QueryFailure::Vanished);
        assert_eq!(QueryFailure::of_win32(Some(32)), QueryFailure::Sharing);
        assert_eq!(QueryFailure::of_win32(Some(1392)), QueryFailure::Other);
        assert_eq!(QueryFailure::of_win32(None), QueryFailure::Other);
    }

    /// A real query against a real directory: the temp dir this test
    /// creates has exactly one link, and asking for a name that is not
    /// there must fail *that name* rather than the directory.
    #[test]
    fn a_plain_file_reports_one_link_and_a_missing_name_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("solo.txt"), b"x").expect("write");
        let handle = LinkDir::open(dir.path()).expect("open the directory");

        match handle.links_of(OsStr::new("solo.txt")) {
            LinkCount::Known { links, file_id, .. } => {
                assert_eq!(links, 1, "a freshly written file has exactly one link");
                assert_ne!(file_id, 0, "NTFS issues a real file id");
            }
            other => panic!("expected a link count, got {other:?}"),
        }
        assert_eq!(
            handle.links_of(OsStr::new("absent.txt")),
            LinkCount::Failed(QueryFailure::Vanished),
            "a missing name is that name's failure, not the volume's"
        );
    }
}
