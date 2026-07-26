//! Errno name/severity taxonomy, shared by every layer that touches a scan
//! error reason.
//!
//! A failed read carries a [`ScanErrno`]. Three consumers need to agree
//! about it and must not drift apart:
//!
//! - the scan owner stores it in the tree side-table
//!   ([`crate::tree::Tree::error_reason`]);
//! - the dump format persists it as a **portable, self-describing name**
//!   (`"EACCES"`, not the raw number — `zstdcat dump.cmbt | jq` then reads
//!   plainly, and errno numbers are not identical across architectures);
//! - the TUI shows a human label and orders a per-errno breakdown by
//!   **severity class**, so a failing disk (`EIO`) is never buried under a
//!   pile of benign permission denials (`EACCES`).
//!
//! One table below is the single source of truth for all three.
//!
//! # Why a newtype instead of the OS's errno
//!
//! [`ScanErrno`] is deliberately **not** `rustix::io::Errno` or any other
//! host-defined type. The numbers below are Linux/x86-64 by *definition*,
//! not "whatever this host calls `EACCES`" — a dump is a portable artifact
//! and its decimal fallback (spec §6.4) is only reversible if writer and
//! reader agree on the numbering independently of where they run. Binding
//! the taxonomy to a host type would break that twice over on Windows,
//! where `rustix` maps the names it does keep onto Winsock values
//! (`EACCES` = 10013, not 13) and simply does not define the rest.
//!
//! Host errnos enter through [`From`] at the platform boundary, which is
//! where the host-to-canonical translation belongs. On Unix the numbers
//! coincide, so the conversion is the identity.

use std::borrow::Cow;
use std::fmt;

/// A scan error reason, as a **canonical POSIX errno number in Linux/x86-64
/// numbering**. Same 4 bytes as the host errno it is built from, so the
/// side-table and the worker→owner batches pay nothing for the abstraction.
///
/// Construct one from a host error at the platform boundary (`From`), from
/// the wire with [`from_name`], or from the taxonomy constants below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanErrno(i32);

impl ScanErrno {
    /// The canonical number. Reach for [`name`] instead when the value is
    /// headed for a dump or a screen — the number alone is not portable
    /// prose.
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Wrap a number already known to be in canonical numbering — from the
    /// wire, or from a Unix host where the two coincide. Not a general
    /// "any OS error code" constructor: see the type's note on Windows.
    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }
}

/// The taxonomy's errnos, numbered as on Linux/x86-64. Fixed by the dump
/// format, not by the build host — see the module note.
impl ScanErrno {
    pub const PERM: Self = Self(1);
    pub const NOENT: Self = Self(2);
    pub const IO: Self = Self(5);
    pub const NXIO: Self = Self(6);
    pub const BADF: Self = Self(9);
    pub const NOMEM: Self = Self(12);
    pub const ACCESS: Self = Self(13);
    pub const NODEV: Self = Self(19);
    pub const NOTDIR: Self = Self(20);
    pub const INVAL: Self = Self(22);
    pub const NFILE: Self = Self(23);
    pub const MFILE: Self = Self(24);
    pub const NOSPC: Self = Self(28);
    pub const ROFS: Self = Self(30);
    pub const NAMETOOLONG: Self = Self(36);
    pub const LOOP: Self = Self(40);
    pub const OVERFLOW: Self = Self(75);
    pub const NETDOWN: Self = Self(100);
    pub const NETUNREACH: Self = Self(101);
    pub const CONNRESET: Self = Self(104);
    pub const TIMEDOUT: Self = Self(110);
    pub const HOSTDOWN: Self = Self(112);
    pub const HOSTUNREACH: Self = Self(113);
    pub const STALE: Self = Self(116);
    pub const DQUOT: Self = Self(122);
}

/// Base of the **non-POSIX reason band**. Platform-specific reasons that no
/// errno names honestly are numbered from here upward, so their decimal
/// wire fallback can never be mistaken for a POSIX errno on a reader that
/// does not know the name (POSIX numbering stops well below 4096 on every
/// platform camembert targets; 2^24 leaves that untouchable).
const NON_POSIX_BASE: i32 = 0x0100_0000;

/// Reasons that have no POSIX errno worth borrowing.
impl ScanErrno {
    /// Windows `ERROR_SHARING_VIOLATION` / `ERROR_LOCK_VIOLATION`: another
    /// process (antivirus, Office, a backup agent) holds the file open with
    /// a share mode that excludes us.
    ///
    /// Deliberately **not** `EBUSY`. "Resource busy" describes a device or
    /// a mount, and a user reading it would look for the wrong cause; this
    /// is the error a Windows scan hits most after access-denied, so it is
    /// worth naming for what it is (Windows backend design §8).
    pub const SHARING_VIOLATION: Self = Self(NON_POSIX_BASE + 32);
}

/// Renders as the canonical name (`EACCES`, or the bare number outside the
/// taxonomy) rather than the OS's prose description. A log line that says
/// `EACCES` means the same thing wherever it was written, which is the
/// whole point of the newtype; asking the host to describe a canonical
/// number would reintroduce exactly the numbering assumption being removed.
impl fmt::Display for ScanErrno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&name(*self))
    }
}

/// The Unix platform boundary: the host's numbering *is* the canonical
/// numbering, so this is the identity on the numbers.
///
/// The Windows counterpart belongs here too, and must go through
/// [`std::io::ErrorKind`] rather than `raw_os_error()`: Win32 codes occupy
/// the same small integers as POSIX errnos with different meanings
/// (`ERROR_ACCESS_DENIED` is 5, which is `EIO` here), so mapping raw would
/// report a dying disk for a permission denial.
#[cfg(unix)]
impl From<rustix::io::Errno> for ScanErrno {
    fn from(errno: rustix::io::Errno) -> Self {
        Self(errno.raw_os_error())
    }
}

/// Win32 code → canonical reason, the Windows half of the platform
/// boundary. See [`from_win32`] for why this table exists and why it is
/// consulted before [`std::io::ErrorKind`].
#[cfg(windows)]
const WIN32_TABLE: &[(u32, ScanErrno)] = &[
    // --- the disk or the volume is in trouble ---
    // 1392/1393: the Windows EIO. Precisely what Severity::Alert exists for.
    (1392, ScanErrno::IO),   // ERROR_FILE_CORRUPT
    (1393, ScanErrno::IO),   // ERROR_DISK_CORRUPT
    (39, ScanErrno::NOSPC),  // ERROR_HANDLE_DISK_FULL
    (112, ScanErrno::NOSPC), // ERROR_DISK_FULL
    (55, ScanErrno::NODEV),  // ERROR_DEV_NOT_EXIST
    (21, ScanErrno::NXIO),   // ERROR_NOT_READY — drive present, no media
    // --- resources, locks, network ---
    // The two errors this whole table exists for: std maps both to
    // `ErrorKind::Uncategorized`, so an ErrorKind-only mapping would emit
    // no reason at all for the failure Windows users hit most.
    (32, ScanErrno::SHARING_VIOLATION), // ERROR_SHARING_VIOLATION
    (33, ScanErrno::SHARING_VIOLATION), // ERROR_LOCK_VIOLATION — same cause,
    // same remedy: something else has the file. One row, not two.
    (4, ScanErrno::MFILE),        // ERROR_TOO_MANY_OPEN_FILES
    (8, ScanErrno::NOMEM),        // ERROR_NOT_ENOUGH_MEMORY
    (14, ScanErrno::NOMEM),       // ERROR_OUTOFMEMORY
    (19, ScanErrno::ROFS),        // ERROR_WRITE_PROTECT
    (121, ScanErrno::TIMEDOUT),   // ERROR_SEM_TIMEOUT
    (59, ScanErrno::STALE),       // ERROR_UNEXP_NET_ERR
    (64, ScanErrno::STALE),       // ERROR_NETNAME_DELETED
    (53, ScanErrno::HOSTUNREACH), // ERROR_BAD_NETPATH
    (6, ScanErrno::BADF),         // ERROR_INVALID_HANDLE
    // --- permissions ---
    (5, ScanErrno::ACCESS), // ERROR_ACCESS_DENIED — 5 is EIO in POSIX
    // numbering, which is exactly why this is a lookup and never a cast.
    // --- races, malformed names, loops ---
    (2, ScanErrno::NOENT),         // ERROR_FILE_NOT_FOUND
    (3, ScanErrno::NOENT),         // ERROR_PATH_NOT_FOUND
    (267, ScanErrno::NOTDIR),      // ERROR_DIRECTORY
    (206, ScanErrno::NAMETOOLONG), // ERROR_FILENAME_EXCED_RANGE
    (123, ScanErrno::INVAL),       // ERROR_INVALID_NAME
    (87, ScanErrno::INVAL),        // ERROR_INVALID_PARAMETER
    // Otherwise unreachable: `io::ErrorKind::FilesystemLoop` is still
    // nightly-only at 1.88, so the ErrorKind fallback can never produce
    // ELOOP on its own.
    (1921, ScanErrno::LOOP), // ERROR_CANT_RESOLVE_FILENAME
];

/// The Windows platform boundary: a raw Win32 code (`GetLastError`) into a
/// canonical reason.
///
/// The table is consulted **before** [`std::io::ErrorKind`] because the two
/// errors a Windows scan hits most after access-denied —
/// `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION` — both land in
/// `ErrorKind::Uncategorized`, so an `ErrorKind`-only mapping would drop
/// exactly the reasons users need. It does not reintroduce the hazard the
/// module note warns about: a table is a lookup, not a reinterpretation of
/// a Win32 number as an errno.
///
/// Unmapped codes fall through to `ErrorKind`, and anything `ErrorKind`
/// cannot name either becomes an opaque numeric reason — visible as
/// [`Severity::Fault`], never silently demoted to noise.
#[cfg(windows)]
pub(crate) fn from_win32(code: u32) -> ScanErrno {
    if let Some(&(_, errno)) = WIN32_TABLE.iter().find(|&&(win32, _)| win32 == code) {
        return errno;
    }
    match std::io::Error::from_raw_os_error(code as i32).kind() {
        std::io::ErrorKind::PermissionDenied => ScanErrno::ACCESS,
        std::io::ErrorKind::NotFound => ScanErrno::NOENT,
        std::io::ErrorKind::OutOfMemory => ScanErrno::NOMEM,
        std::io::ErrorKind::TimedOut => ScanErrno::TIMEDOUT,
        std::io::ErrorKind::InvalidInput => ScanErrno::INVAL,
        std::io::ErrorKind::ConnectionReset => ScanErrno::CONNRESET,
        std::io::ErrorKind::HostUnreachable => ScanErrno::HOSTUNREACH,
        std::io::ErrorKind::NetworkUnreachable => ScanErrno::NETUNREACH,
        std::io::ErrorKind::NetworkDown => ScanErrno::NETDOWN,
        // Keep the raw code rather than inventing a POSIX meaning for it;
        // `name()` renders it as a decimal, which `from_name` reverses.
        _ => ScanErrno::from_raw(NON_POSIX_BASE + code as i32),
    }
}

/// Severity class of a scan error, declared **most alarming first** — the
/// derived [`Ord`] follows declaration order, so sorting by severity lists
/// hardware failures before noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Hardware or filesystem failure that must never be buried: `EIO`
    /// (the disk may be failing), `ENOSPC`, `ENODEV`, `ENXIO`.
    Alert,
    /// A mount, resource or network fault worth acting on: `ESTALE` (broken
    /// network mount), `EMFILE`/`ENFILE`, `ENOMEM`, `EROFS`, `ETIMEDOUT`, …
    Fault,
    /// Permission denied — benign, the classic "rerun as root": `EACCES`,
    /// `EPERM`.
    Denied,
    /// Noise from broken symlinks, races and malformed names: `ELOOP`,
    /// `ENOENT`, `ENOTDIR`, `ENAMETOOLONG`, `EINVAL`.
    Noise,
}

/// One taxonomy row: an errno, its canonical name, a terse human label, and
/// its severity class.
struct Entry {
    errno: ScanErrno,
    name: &'static str,
    label: &'static str,
    severity: Severity,
}

/// The scan-error taxonomy. Covers the errnos a filesystem walk actually
/// hits; anything outside falls back to a numeric name, the OS description,
/// and [`Severity::Fault`] (visible, never silently treated as noise).
const TABLE: &[Entry] = &[
    // --- Alert: the disk or device is in trouble ---
    Entry {
        errno: ScanErrno::IO,
        name: "EIO",
        label: "I/O error — the disk may be failing",
        severity: Severity::Alert,
    },
    Entry {
        errno: ScanErrno::NOSPC,
        name: "ENOSPC",
        label: "no space left on device",
        severity: Severity::Alert,
    },
    Entry {
        errno: ScanErrno::NODEV,
        name: "ENODEV",
        label: "no such device",
        severity: Severity::Alert,
    },
    Entry {
        errno: ScanErrno::NXIO,
        name: "ENXIO",
        label: "no such device or address",
        severity: Severity::Alert,
    },
    // --- Fault: mounts, resources, network ---
    Entry {
        errno: ScanErrno::STALE,
        name: "ESTALE",
        label: "stale file handle — broken network mount",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::MFILE,
        name: "EMFILE",
        label: "too many open files (process limit)",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::NFILE,
        name: "ENFILE",
        label: "too many open files (system limit)",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::NOMEM,
        name: "ENOMEM",
        label: "out of memory",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::OVERFLOW,
        name: "EOVERFLOW",
        label: "value too large for its type",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::ROFS,
        name: "EROFS",
        label: "read-only filesystem",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::TIMEDOUT,
        name: "ETIMEDOUT",
        label: "operation timed out",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::DQUOT,
        name: "EDQUOT",
        label: "disk quota exceeded",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::HOSTUNREACH,
        name: "EHOSTUNREACH",
        label: "host unreachable",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::HOSTDOWN,
        name: "EHOSTDOWN",
        label: "host is down",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::NETUNREACH,
        name: "ENETUNREACH",
        label: "network unreachable",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::NETDOWN,
        name: "ENETDOWN",
        label: "network is down",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::CONNRESET,
        name: "ECONNRESET",
        label: "connection reset",
        severity: Severity::Fault,
    },
    Entry {
        errno: ScanErrno::BADF,
        name: "EBADF",
        label: "bad file descriptor",
        severity: Severity::Fault,
    },
    // Windows-only, and the only row here whose name is not a POSIX errno
    // — see [`ScanErrno::SHARING_VIOLATION`]. Present in the table on every
    // platform on purpose: a dump written on Windows has to decode on
    // Linux, so the taxonomy cannot be `cfg`-dependent.
    Entry {
        errno: ScanErrno::SHARING_VIOLATION,
        name: "WIN_SHARING_VIOLATION",
        label: "file held open by another process (antivirus, backup, …)",
        severity: Severity::Fault,
    },
    // --- Denied: permissions ---
    Entry {
        errno: ScanErrno::ACCESS,
        name: "EACCES",
        label: "permission denied",
        severity: Severity::Denied,
    },
    Entry {
        errno: ScanErrno::PERM,
        name: "EPERM",
        label: "operation not permitted",
        severity: Severity::Denied,
    },
    // --- Noise: races, broken links, malformed names ---
    Entry {
        errno: ScanErrno::LOOP,
        name: "ELOOP",
        label: "too many symbolic-link levels",
        severity: Severity::Noise,
    },
    Entry {
        errno: ScanErrno::NOENT,
        name: "ENOENT",
        label: "no such file — it raced away mid-scan",
        severity: Severity::Noise,
    },
    Entry {
        errno: ScanErrno::NOTDIR,
        name: "ENOTDIR",
        label: "not a directory",
        severity: Severity::Noise,
    },
    Entry {
        errno: ScanErrno::NAMETOOLONG,
        name: "ENAMETOOLONG",
        label: "file name too long",
        severity: Severity::Noise,
    },
    Entry {
        errno: ScanErrno::INVAL,
        name: "EINVAL",
        label: "invalid argument",
        severity: Severity::Noise,
    },
];

fn lookup(errno: ScanErrno) -> Option<&'static Entry> {
    TABLE.iter().find(|e| e.errno == errno)
}

/// Portable name of an errno for the dump wire: the canonical `E…` name
/// when known, otherwise the raw number as a decimal string. Reversible
/// with [`from_name`].
pub fn name(errno: ScanErrno) -> Cow<'static, str> {
    match lookup(errno) {
        Some(e) => Cow::Borrowed(e.name),
        None => Cow::Owned(errno.raw().to_string()),
    }
}

/// Inverse of [`name`]: parse a wire name back into a [`ScanErrno`]. Accepts
/// a known `E…` name or a decimal raw-errno string; returns `None` for an
/// empty or otherwise unparseable value (readers preserve such reasons
/// opaquely, spec §10).
pub fn from_name(s: &str) -> Option<ScanErrno> {
    if let Some(e) = TABLE.iter().find(|e| e.name == s) {
        return Some(e.errno);
    }
    s.parse::<i32>().ok().map(ScanErrno::from_raw)
}

/// Terse human label for the selection card. Falls back to the OS's own
/// description for errnos outside the taxonomy.
///
/// The fallback asks the *host* to describe a canonical number, which is
/// only sound where the two numberings agree. It needs the same
/// `ErrorKind`-shaped treatment as the `From` impl above when a
/// non-POSIX-numbered platform arrives.
pub fn label(errno: ScanErrno) -> Cow<'static, str> {
    match lookup(errno) {
        Some(e) => Cow::Borrowed(e.label),
        None => Cow::Owned(std::io::Error::from_raw_os_error(errno.raw()).to_string()),
    }
}

/// Severity class of an errno; unknown errnos are [`Severity::Fault`]
/// (surfaced, never demoted to noise).
pub fn severity(errno: ScanErrno) -> Severity {
    lookup(errno).map_or(Severity::Fault, |e| e.severity)
}

/// Collapse an errno histogram into a severity-ordered breakdown: most
/// alarming class first, and by descending count within a class (ties
/// broken by raw errno for determinism). Powers the errors card's one-line
/// summary — a big benign `EACCES` count never outranks a single `EIO`.
pub fn breakdown(
    counts: impl IntoIterator<Item = (ScanErrno, u64)>,
) -> Vec<(ScanErrno, u64, Severity)> {
    let mut rows: Vec<(ScanErrno, u64, Severity)> = counts
        .into_iter()
        .map(|(errno, count)| (errno, count, severity(errno)))
        .collect();
    rows.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then(b.1.cmp(&a.1))
            .then(a.0.raw().cmp(&b.0.raw()))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_round_trip() {
        for e in TABLE {
            assert_eq!(name(e.errno), e.name);
            assert_eq!(from_name(e.name), Some(e.errno));
        }
    }

    /// The wire is the contract: these are the exact strings a v1 dump
    /// carries in `er`, and the exact numbers a v1 dump's decimal fallback
    /// means. Pinned literally so a future edit to the taxonomy cannot
    /// silently rewrite either half.
    #[test]
    fn wire_names_and_numbers_are_pinned() {
        let wire: Vec<(&str, i32)> = TABLE.iter().map(|e| (e.name, e.errno.raw())).collect();
        assert_eq!(
            wire,
            [
                ("EIO", 5),
                ("ENOSPC", 28),
                ("ENODEV", 19),
                ("ENXIO", 6),
                ("ESTALE", 116),
                ("EMFILE", 24),
                ("ENFILE", 23),
                ("ENOMEM", 12),
                ("EOVERFLOW", 75),
                ("EROFS", 30),
                ("ETIMEDOUT", 110),
                ("EDQUOT", 122),
                ("EHOSTUNREACH", 113),
                ("EHOSTDOWN", 112),
                ("ENETUNREACH", 101),
                ("ENETDOWN", 100),
                ("ECONNRESET", 104),
                ("EBADF", 9),
                // Windows backend, added deliberately: additive on the wire
                // (a reader that predates it degrades the unknown name to
                // `None`), and numbered outside POSIX range on purpose.
                ("WIN_SHARING_VIOLATION", 0x0100_0000 + 32),
                ("EACCES", 13),
                ("EPERM", 1),
                ("ELOOP", 40),
                ("ENOENT", 2),
                ("ENOTDIR", 20),
                ("ENAMETOOLONG", 36),
                ("EINVAL", 22),
            ]
        );
    }

    /// The canonical numbering claims to be the host's on Unix. If that
    /// ever stops being true the decimal fallback silently changes meaning
    /// between a scan and its dump, so check it against the host rather
    /// than trusting the comment.
    #[cfg(unix)]
    #[test]
    fn canonical_numbers_match_this_unix_host() {
        use rustix::io::Errno;

        for (host, canonical) in [
            (Errno::IO, ScanErrno::IO),
            (Errno::NOSPC, ScanErrno::NOSPC),
            (Errno::NODEV, ScanErrno::NODEV),
            (Errno::NXIO, ScanErrno::NXIO),
            (Errno::STALE, ScanErrno::STALE),
            (Errno::MFILE, ScanErrno::MFILE),
            (Errno::NFILE, ScanErrno::NFILE),
            (Errno::NOMEM, ScanErrno::NOMEM),
            (Errno::OVERFLOW, ScanErrno::OVERFLOW),
            (Errno::ROFS, ScanErrno::ROFS),
            (Errno::TIMEDOUT, ScanErrno::TIMEDOUT),
            (Errno::DQUOT, ScanErrno::DQUOT),
            (Errno::HOSTUNREACH, ScanErrno::HOSTUNREACH),
            (Errno::HOSTDOWN, ScanErrno::HOSTDOWN),
            (Errno::NETUNREACH, ScanErrno::NETUNREACH),
            (Errno::NETDOWN, ScanErrno::NETDOWN),
            (Errno::CONNRESET, ScanErrno::CONNRESET),
            (Errno::BADF, ScanErrno::BADF),
            (Errno::ACCESS, ScanErrno::ACCESS),
            (Errno::PERM, ScanErrno::PERM),
            (Errno::LOOP, ScanErrno::LOOP),
            (Errno::NOENT, ScanErrno::NOENT),
            (Errno::NOTDIR, ScanErrno::NOTDIR),
            (Errno::NAMETOOLONG, ScanErrno::NAMETOOLONG),
            (Errno::INVAL, ScanErrno::INVAL),
        ] {
            assert_eq!(
                ScanErrno::from(host),
                canonical,
                "host {host} disagrees with the canonical taxonomy"
            );
        }
    }

    /// The newtype must cost nothing where it is stored in bulk: the
    /// worker→owner batches and the tree side-table both hold it by value.
    #[test]
    fn scan_errno_is_no_wider_than_a_host_errno() {
        assert_eq!(size_of::<ScanErrno>(), size_of::<i32>());
        assert_eq!(size_of::<Option<ScanErrno>>(), size_of::<Option<i32>>());
    }

    #[test]
    fn unknown_errno_falls_back_to_number() {
        // ERANGE is deliberately outside the scan taxonomy.
        let errno = ScanErrno::from_raw(34);
        let encoded = name(errno);
        assert_eq!(encoded, "34");
        assert_eq!(from_name(&encoded), Some(errno));
        assert_eq!(
            severity(errno),
            Severity::Fault,
            "unknowns are Fault, not Noise"
        );
    }

    #[test]
    fn from_name_rejects_garbage() {
        assert_eq!(from_name(""), None);
        assert_eq!(from_name("not-an-errno"), None);
    }

    #[test]
    fn severity_orders_alarms_before_noise() {
        assert!(Severity::Alert < Severity::Fault);
        assert!(Severity::Fault < Severity::Denied);
        assert!(Severity::Denied < Severity::Noise);
    }

    #[test]
    fn breakdown_puts_eio_before_a_larger_eacces_count() {
        // The classic case: thousands of benign EACCES, a dozen EIO.
        let rows = breakdown([
            (ScanErrno::ACCESS, 3390),
            (ScanErrno::IO, 12),
            (ScanErrno::LOOP, 2),
        ]);
        let names: Vec<Cow<'static, str>> = rows.iter().map(|&(e, _, _)| name(e)).collect();
        assert_eq!(
            names,
            ["EIO", "EACCES", "ELOOP"],
            "severity class beats count"
        );
    }

    #[test]
    fn breakdown_orders_by_count_within_a_class() {
        // Two Alert-class errnos: the more frequent one comes first.
        let rows = breakdown([(ScanErrno::IO, 3), (ScanErrno::NOSPC, 40)]);
        let names: Vec<Cow<'static, str>> = rows.iter().map(|&(e, _, _)| name(e)).collect();
        assert_eq!(names, ["ENOSPC", "EIO"]);
    }
}
