//! Errno name/severity taxonomy, shared by every layer that touches a scan
//! error reason.
//!
//! A failed read carries a [`rustix::io::Errno`]. Three consumers need to
//! agree about it and must not drift apart:
//!
//! - the scan owner stores the raw [`Errno`] in the tree side-table
//!   ([`crate::tree::Tree::error_reason`]);
//! - the dump format persists it as a **portable, self-describing name**
//!   (`"EACCES"`, not the raw number — `zstdcat dump.cmbt | jq` then reads
//!   plainly, and errno numbers are not identical across architectures);
//! - the TUI shows a human label and orders a per-errno breakdown by
//!   **severity class**, so a failing disk (`EIO`) is never buried under a
//!   pile of benign permission denials (`EACCES`).
//!
//! One table below is the single source of truth for all three.

use std::borrow::Cow;

use rustix::io::Errno;

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
    errno: Errno,
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
        errno: Errno::IO,
        name: "EIO",
        label: "I/O error — the disk may be failing",
        severity: Severity::Alert,
    },
    Entry {
        errno: Errno::NOSPC,
        name: "ENOSPC",
        label: "no space left on device",
        severity: Severity::Alert,
    },
    Entry {
        errno: Errno::NODEV,
        name: "ENODEV",
        label: "no such device",
        severity: Severity::Alert,
    },
    Entry {
        errno: Errno::NXIO,
        name: "ENXIO",
        label: "no such device or address",
        severity: Severity::Alert,
    },
    // --- Fault: mounts, resources, network ---
    Entry {
        errno: Errno::STALE,
        name: "ESTALE",
        label: "stale file handle — broken network mount",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::MFILE,
        name: "EMFILE",
        label: "too many open files (process limit)",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::NFILE,
        name: "ENFILE",
        label: "too many open files (system limit)",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::NOMEM,
        name: "ENOMEM",
        label: "out of memory",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::OVERFLOW,
        name: "EOVERFLOW",
        label: "value too large for its type",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::ROFS,
        name: "EROFS",
        label: "read-only filesystem",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::TIMEDOUT,
        name: "ETIMEDOUT",
        label: "operation timed out",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::DQUOT,
        name: "EDQUOT",
        label: "disk quota exceeded",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::HOSTUNREACH,
        name: "EHOSTUNREACH",
        label: "host unreachable",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::HOSTDOWN,
        name: "EHOSTDOWN",
        label: "host is down",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::NETUNREACH,
        name: "ENETUNREACH",
        label: "network unreachable",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::NETDOWN,
        name: "ENETDOWN",
        label: "network is down",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::CONNRESET,
        name: "ECONNRESET",
        label: "connection reset",
        severity: Severity::Fault,
    },
    Entry {
        errno: Errno::BADF,
        name: "EBADF",
        label: "bad file descriptor",
        severity: Severity::Fault,
    },
    // --- Denied: permissions ---
    Entry {
        errno: Errno::ACCESS,
        name: "EACCES",
        label: "permission denied",
        severity: Severity::Denied,
    },
    Entry {
        errno: Errno::PERM,
        name: "EPERM",
        label: "operation not permitted",
        severity: Severity::Denied,
    },
    // --- Noise: races, broken links, malformed names ---
    Entry {
        errno: Errno::LOOP,
        name: "ELOOP",
        label: "too many symbolic-link levels",
        severity: Severity::Noise,
    },
    Entry {
        errno: Errno::NOENT,
        name: "ENOENT",
        label: "no such file — it raced away mid-scan",
        severity: Severity::Noise,
    },
    Entry {
        errno: Errno::NOTDIR,
        name: "ENOTDIR",
        label: "not a directory",
        severity: Severity::Noise,
    },
    Entry {
        errno: Errno::NAMETOOLONG,
        name: "ENAMETOOLONG",
        label: "file name too long",
        severity: Severity::Noise,
    },
    Entry {
        errno: Errno::INVAL,
        name: "EINVAL",
        label: "invalid argument",
        severity: Severity::Noise,
    },
];

fn lookup(errno: Errno) -> Option<&'static Entry> {
    TABLE.iter().find(|e| e.errno == errno)
}

/// Portable name of an errno for the dump wire: the canonical `E…` name
/// when known, otherwise the raw number as a decimal string. Reversible
/// with [`from_name`].
pub fn name(errno: Errno) -> Cow<'static, str> {
    match lookup(errno) {
        Some(e) => Cow::Borrowed(e.name),
        None => Cow::Owned(errno.raw_os_error().to_string()),
    }
}

/// Inverse of [`name`]: parse a wire name back into an [`Errno`]. Accepts a
/// known `E…` name or a decimal raw-errno string; returns `None` for an
/// empty or otherwise unparseable value (readers preserve such reasons
/// opaquely, spec §10).
pub fn from_name(s: &str) -> Option<Errno> {
    if let Some(e) = TABLE.iter().find(|e| e.name == s) {
        return Some(e.errno);
    }
    s.parse::<i32>().ok().map(Errno::from_raw_os_error)
}

/// Terse human label for the selection card. Falls back to the OS's own
/// description for errnos outside the taxonomy.
pub fn label(errno: Errno) -> Cow<'static, str> {
    match lookup(errno) {
        Some(e) => Cow::Borrowed(e.label),
        None => Cow::Owned(errno.to_string()),
    }
}

/// Severity class of an errno; unknown errnos are [`Severity::Fault`]
/// (surfaced, never demoted to noise).
pub fn severity(errno: Errno) -> Severity {
    lookup(errno).map_or(Severity::Fault, |e| e.severity)
}

/// Collapse an errno histogram into a severity-ordered breakdown: most
/// alarming class first, and by descending count within a class (ties
/// broken by raw errno for determinism). Powers the errors card's one-line
/// summary — a big benign `EACCES` count never outranks a single `EIO`.
pub fn breakdown(counts: impl IntoIterator<Item = (Errno, u64)>) -> Vec<(Errno, u64, Severity)> {
    let mut rows: Vec<(Errno, u64, Severity)> = counts
        .into_iter()
        .map(|(errno, count)| (errno, count, severity(errno)))
        .collect();
    rows.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then(b.1.cmp(&a.1))
            .then(a.0.raw_os_error().cmp(&b.0.raw_os_error()))
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

    #[test]
    fn unknown_errno_falls_back_to_number() {
        // ERANGE is deliberately outside the scan taxonomy.
        let errno = Errno::RANGE;
        let encoded = name(errno);
        assert_eq!(encoded, errno.raw_os_error().to_string());
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
        let rows = breakdown([(Errno::ACCESS, 3390), (Errno::IO, 12), (Errno::LOOP, 2)]);
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
        let rows = breakdown([(Errno::IO, 3), (Errno::NOSPC, 40)]);
        let names: Vec<Cow<'static, str>> = rows.iter().map(|&(e, _, _)| name(e)).collect();
        assert_eq!(names, ["ENOSPC", "EIO"]);
    }
}
