//! Worker → owner batch messages.
//!
//! Workers enumerate one directory per job, accumulate its entries into
//! **sections** of at most [`SECTION_CAP`] entries, pre-sum each section,
//! and send it over the single bounded channel. Giants produce multiple
//! sections (D2); each integrated section becomes one child run.

use rustix::io::Errno;

use crate::tree::{ExcludedReason, Kind};

/// Section flush threshold (entries). One run per flushed section.
pub(crate) const SECTION_CAP: usize = 4096;

/// Pre-computed sums for one section, so the owner integrates with plain
/// adds instead of per-entry work up the ancestor chain (D1 graft).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SectionSums {
    /// Σ apparent bytes over the section's entries.
    pub apparent: u64,
    /// Σ disk bytes (`st_blocks * 512`) over the section's entries.
    pub disk: u64,
    /// Number of entries (inodes seen, hardlink dedup NOT applied — the
    /// owner adjusts against its registry).
    pub count: u64,
    /// Entries whose stat failed.
    pub errors: u32,
}

/// One enumerated entry.
#[derive(Debug)]
pub(crate) struct BatchEntry {
    /// Raw name bytes (no NUL, never `.` / `..`).
    pub name: Vec<u8>,
    pub kind: Kind,
    /// `st_size`; 0 when `error`.
    pub apparent: u64,
    /// `st_blocks * 512`; 0 when `error`.
    pub disk: u64,
    /// mtime, unix seconds (i64 — full range, decided).
    pub mtime: i64,
    /// `st_nlink`; hardlink handling only applies when `> 1` and the entry
    /// is not a directory.
    pub nlink: u32,
    /// Inode number — meaningful when `nlink > 1` (hardlink registry key).
    pub ino: u64,
    /// `st_dev` of the entry (hardlink registry key, other-fs detection).
    pub dev: u64,
    /// stat failed; sizes are zero and only `kind` (from `d_type`, may be
    /// [`Kind::Other`]) is known.
    pub error: bool,
    /// For child directories that will be scanned: the token the worker
    /// assigned to the child's own future batches.
    pub child_token: Option<u64>,
    /// Directory on another filesystem: recorded but not descended into.
    pub excluded: Option<ExcludedReason>,
}

/// One section of one directory's entries.
#[derive(Debug)]
pub(crate) struct Batch {
    /// Identity of the directory these entries belong to. The owner maps
    /// tokens to [`crate::tree::DirId`] on integration; batches for a
    /// not-yet-known token (child scanned before its parent's discovering
    /// section integrated — work stealing) go to the bounded holding map.
    pub dir_token: u64,
    pub entries: Vec<BatchEntry>,
    pub sums: SectionSums,
    /// Completion protocol: the directory's self token drops only when its
    /// last section is integrated AND all its stat results are included in
    /// sections. In this thread-pool implementation stats are synchronous,
    /// so `is_last_section` alone suffices; the future io_uring path must
    /// additionally gate on outstanding-statx == 0 (binding amendment,
    /// Option B's lesson).
    pub is_last_section: bool,
    /// Child dirs discovered in this section (== entries with
    /// `child_token`); carried explicitly so the owner can bump the
    /// pending count without rescanning entries.
    pub child_dirs: u32,
    /// The directory itself could not be opened/read. Terminal:
    /// `is_last_section` is true and `entries` is empty.
    pub dir_error: Option<Errno>,
}
