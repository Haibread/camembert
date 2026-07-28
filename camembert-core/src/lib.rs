//! Core library for camembert: filesystem scanning, aggregation, and size
//! semantics. Frontends (TUI, GUI) depend on this crate and never the other
//! way around.

// Depends on `fiemap::OracleReport` and `freeable::Coverage`, so it can
// only exist where those do: Linux (see the gates below).
#[cfg(target_os = "linux")]
pub mod confidence;
// openat/unlinkat/AT_REMOVEDIR (via `rustix::fs`) are POSIX, not
// Linux-specific — portable to any Unix.
#[cfg(unix)]
pub mod delete;
pub mod diff;
pub mod dump;
pub mod errno;
// `FS_IOC_FIEMAP` is a Linux-only ioctl (linux/fs.h); no BSD/Darwin
// equivalent.
#[cfg(target_os = "linux")]
pub mod fiemap;
pub mod flat;
// Enumerates open files via `/proc/[pid]/fd`, a Linux-only procfs layout.
#[cfg(target_os = "linux")]
pub mod freeable;
pub mod ncdu;
pub mod query;
// `SHQueryRecycleBinW` is the Windows answer to the question `freeable`
// answers on Linux — space already counted as used that no directory tree
// shows. Read-only, and deliberately isolated the same way `freeable` is.
#[cfg(windows)]
pub mod recycle;
pub mod scan;
pub mod size;
pub mod tree;
pub mod view;
// WTF-8 -> UTF-16, the decoder `tree::os_name_from_bytes` needs to hand a
// Windows name back to the filesystem unchanged. Pure and portable, so it
// is compiled and tested everywhere even though only Windows consumes it.
pub mod wtf8;
// `NtQueryInformationByName(FileStatInformation)` is the Win32 half of the
// hardlink story: the scan worker's per-file lookup under `--links`, and
// the TUI's lazy lookup at the point of consumption, share one wrapper.
#[cfg(windows)]
pub mod winlink;
