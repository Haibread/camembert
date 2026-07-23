//! Freeable phase 1 — the `/proc` sweep and its ledger.
//!
//! Disk space held by **deleted-but-still-open** files: a process opened a
//! file, someone `unlink`ed it, but the inode's blocks stay allocated until
//! the last descriptor closes. `df` counts them as used; `du` cannot see them
//! (they have no path in the tree). This module walks `/proc/[pid]/fd/*`,
//! finds those inodes, and hands the UI a plain-data [`Ledger`] — a scan-level
//! side artifact, never tree nodes or aggregates (decisions D1/D8).
//!
//! ## Ground truth (D3)
//!
//! A file is deleted-open iff `fstatat` **through** the magic symlink
//! `/proc/[pid]/fd/N` yields `st_nlink == 0`. The kernel's `" (deleted)"`
//! readlink suffix is per-dentry and ambiguous (a file literally named
//! `foo (deleted)` produces the same string with `st_nlink == 1`), so it is
//! display-only evidence, never the filter. Sizes are `st_blocks * 512`
//! (allocated, sparse-correct). Entries are deduplicated by `(st_dev,
//! st_ino)`, keeping **all** holder PIDs per inode.
//!
//! ## Magic-symlink stat semantics
//!
//! `/proc/[pid]/fd/N` is a magic symlink (`proc_pid_fd(5)`). `readlinkat`
//! returns the last-known path (raw bytes — not guaranteed UTF-8). `statat`
//! **without** `AT_SYMLINK_NOFOLLOW` dereferences the link straight to the
//! live inode, even after unlink — that is exactly what we want (the target's
//! stat, not the symlink's own). This is the opposite flag choice from the
//! scan walker, which uses `SYMLINK_NOFOLLOW` for `lstat` semantics.
//!
//! ## Scope (D2) and RAM-backed split (D3)
//!
//! The headline counts only entries whose `st_dev` equals the scan root's
//! filesystem (`root_dev`) — a coherent subset of the `statvfs` disk gauge.
//! Deleted-open files on *other* crossed devices are kept but grouped and
//! labeled separately (never summed onto the root gauge). memfd/tmpfs/shm
//! inodes are RAM, not disk, and are reported as one aggregate line — see
//! [`is_ram_backed`] for the exact signal and why.
//!
//! ## Degradation (D7)
//!
//! `/proc` absent or unreadable degrades **silently** to an empty result with
//! zero coverage (`tracing` debug only) — never an error the caller must
//! handle. Every per-pid / per-fd failure is skip-and-continue; there are no
//! panics on any runtime path.
//!
//! ## Cost
//!
//! `O(processes × fds)` `readdir` + `readlink` + `fstatat`, no recursion, one
//! final `(dev,ino)` dedup. The research digest measured 37 ms for 505 procs /
//! 6559 fds in single-threaded Python. This native, syscall-direct sweep runs
//! far under that: **~23 ms** measured on the dev machine (release build, 505
//! procs, 140 readable unprivileged) — well within budget for an off-thread
//! scan-end pass, no parallelism needed. Reproduce with the `#[ignore]`d
//! `bench_sweep_cost` test.

use std::mem::MaybeUninit;

use rustc_hash::FxHashMap;
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{AtFlags, Mode, OFlags, RawDir};
use rustix::io::Errno;
use tracing::{debug, trace};

use crate::size::Size;

/// getdents64 buffer for the `/proc` top-level listing (hundreds of pids).
const PROC_DIRENT_BUF: usize = 32 * 1024;
/// getdents64 buffer for a per-process `fd/` listing.
const FD_DIRENT_BUF: usize = 16 * 1024;
/// `/proc/[pid]/comm` is capped at `TASK_COMM_LEN` (16 incl. NUL); read a
/// little extra to be safe and trim.
const COMM_BUF: usize = 64;

// ---------------------------------------------------------------------------
// Public plain-data types (Send, no lifetimes into /proc)
// ---------------------------------------------------------------------------

/// A process holding an open descriptor to an inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Process id (host PID; container PIDs map through as usual).
    pub pid: u32,
    /// `/proc/[pid]/comm` name, if it could be read (best-effort, lossy
    /// UTF-8). `None` when the process vanished or `comm` was unreadable.
    pub comm: Option<String>,
}

/// One deduplicated deleted-but-open **disk** file (RAM-backed inodes never
/// become a `DeletedEntry`; they are aggregated separately on the ledger).
#[derive(Debug, Clone)]
pub struct DeletedEntry {
    /// `st_dev` of the inode.
    pub dev: u64,
    /// `st_ino` of the inode.
    pub ino: u64,
    /// Allocated bytes, `st_blocks * 512` (sparse-correct).
    pub bytes: u64,
    /// Last-known path from `readlink` — raw bytes, **not** guaranteed UTF-8,
    /// typically ending in `" (deleted)"`. Display-only evidence (D3).
    pub evidence: Vec<u8>,
    /// Every process holding this inode open (deduped by PID).
    pub holders: Vec<Holder>,
}

impl DeletedEntry {
    /// The evidence path as lossy UTF-8, for display.
    pub fn evidence_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.evidence)
    }
}

/// Deleted-open files found on one non-root device (D2: kept, labeled,
/// excluded from the root gauge).
#[derive(Debug, Clone)]
pub struct DeviceGroup {
    /// `st_dev` shared by every entry in the group.
    pub dev: u64,
    /// Entries on this device, largest first.
    pub entries: Vec<DeletedEntry>,
}

impl DeviceGroup {
    /// Sum of allocated bytes across the group.
    pub fn freeable_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }
}

/// How much of the process table the sweep could actually read (D6/D7). The
/// UI turns this into "N of M processes readable — run as root for the full
/// view" so an absent finding is never mistaken for a clean bill of health.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Processes observed alive enough to attempt (a process that vanished
    /// mid-sweep, `ENOENT`/`ESRCH`, is benign and excluded from this count).
    pub seen: u32,
    /// Processes whose `fd/` table we could open (subset of `seen`).
    pub readable: u32,
}

impl Coverage {
    /// Processes seen but not readable (permission-gated). Never underflows.
    pub fn unreadable(&self) -> u32 {
        self.seen.saturating_sub(self.readable)
    }

    /// True when at least one process was seen but could not be read — the
    /// results may miss a holder.
    pub fn is_partial(&self) -> bool {
        self.readable < self.seen
    }
}

/// Scan-level side artifact: deleted-but-open files split by scope, the
/// RAM-backed aggregate, and coverage. Plain data, `Send`, no `/proc`
/// lifetimes — the UI consumes it via the existing snapshot machinery.
#[derive(Debug, Clone)]
pub struct Ledger {
    root_dev: u64,
    root_fs: Vec<DeletedEntry>,
    other_devices: Vec<DeviceGroup>,
    ram_backed_bytes: u64,
    ram_backed_count: u32,
    coverage: Coverage,
}

impl Ledger {
    /// The scan root's filesystem device the headline is scoped to (D2).
    pub fn root_dev(&self) -> u64 {
        self.root_dev
    }

    /// Deleted-open files on the root filesystem, largest first — the
    /// headline set.
    pub fn root_fs_entries(&self) -> &[DeletedEntry] {
        &self.root_fs
    }

    /// Deleted-open files on other crossed devices, grouped per device
    /// (excluded from the root gauge; labeled in the panel).
    pub fn other_device_groups(&self) -> &[DeviceGroup] {
        &self.other_devices
    }

    /// Total allocated bytes of RAM-backed (memfd/tmpfs/shm) deleted-open
    /// inodes — reported as "RAM-backed, not disk", never as disk freeable.
    pub fn ram_backed_bytes(&self) -> u64 {
        self.ram_backed_bytes
    }

    /// Number of distinct RAM-backed inodes.
    pub fn ram_backed_count(&self) -> u32 {
        self.ram_backed_count
    }

    /// Process-table coverage for the honest footer (D6/D7).
    pub fn coverage(&self) -> Coverage {
        self.coverage
    }

    /// The headline number: allocated bytes freeable by closing files on the
    /// **root filesystem** (D2). Coherent with the `statvfs` gauge.
    pub fn root_fs_freeable_bytes(&self) -> u64 {
        self.root_fs.iter().map(|e| e.bytes).sum()
    }

    /// Allocated bytes freeable on non-root crossed devices (panel only).
    pub fn other_device_freeable_bytes(&self) -> u64 {
        self.other_devices
            .iter()
            .map(DeviceGroup::freeable_bytes)
            .sum()
    }

    /// Count of distinct deleted-open inodes on the root filesystem.
    pub fn root_fs_entry_count(&self) -> usize {
        self.root_fs.len()
    }

    /// True when nothing freeable was found anywhere (root, other devices, or
    /// RAM). A degraded (no-`/proc`) sweep is always empty.
    pub fn is_empty(&self) -> bool {
        self.root_fs.is_empty() && self.other_devices.is_empty() && self.ram_backed_count == 0
    }
}

/// One indexed open file: its last-known `readlink` evidence path
/// (display-only, not guaranteed UTF-8 — see [`DeletedEntry::evidence`])
/// alongside its holders. Kept next to `(dev, ino)` in [`OpenFileIndex`] so
/// a path-prefix containment query ("is anything open under this marked
/// *directory*?") doesn't need a second data structure or a re-walk of
/// `/proc` — a marked file is matched directly by its own `(dev, ino)`
/// ([`OpenFileIndex::holders`]), but a marked directory has no single
/// inode to look up, so the UI instead scans [`OpenFileIndex::iter`] for
/// evidence paths that fall under it (D6 amendment: the primary
/// real-world case — a marked data directory with files still open
/// somewhere inside it — was invisible to a `(dev, ino)`-only check).
#[derive(Debug, Clone)]
struct IndexedOpenFile {
    evidence: Vec<u8>,
    holders: Vec<Holder>,
}

/// Every currently-open file keyed by `(st_dev, st_ino)` → its evidence
/// path and holders, unfiltered by link count (D4/D6). Powers the
/// pre-deletion open-file warning two ways: a marked *file* is matched
/// directly by [`Self::holders`]; a marked *directory* has its contents
/// found by scanning [`Self::iter`]'s evidence paths for a path-prefix
/// match against the directory (the same longest-prefix/path-boundary
/// logic the freeable panel's ancestor grouping uses). Carries its own
/// [`Coverage`] so the warning can repeat the panel's caveat when the
/// sweep was partial (attack A serious finding) — that same caveat also
/// covers this index's path-based containment check: a process in a
/// different mount namespace can hold an fd whose readlink evidence
/// doesn't textually match the path the UI marked (different bind mount,
/// chroot, container view, …), which is a false-negative the coverage
/// line's honesty umbrella already exists to admit rather than paper over
/// with false certainty.
#[derive(Debug, Clone, Default)]
pub struct OpenFileIndex {
    by_inode: FxHashMap<(u64, u64), IndexedOpenFile>,
    coverage: Coverage,
}

impl OpenFileIndex {
    /// Holders of the given inode, or `None` if it is not currently open by
    /// any readable process.
    pub fn holders(&self, dev: u64, ino: u64) -> Option<&[Holder]> {
        self.by_inode
            .get(&(dev, ino))
            .map(|entry| entry.holders.as_slice())
    }

    /// Every indexed open file as `(evidence, dev, ino, holders)`. The
    /// pre-deletion warning's marked-directory containment check filters
    /// this for evidence paths that fall under the marked directory —
    /// there is no single inode to look up for "is anything open inside
    /// this directory", so an iterator over everything the sweep saw is
    /// the right shape: the process/fd-bounded walk already keeps this to
    /// at most a few thousand short paths, cheap to hold and scan.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], u64, u64, &[Holder])> {
        self.by_inode.iter().map(|(&(dev, ino), entry)| {
            (
                entry.evidence.as_slice(),
                dev,
                ino,
                entry.holders.as_slice(),
            )
        })
    }

    /// Number of distinct open inodes indexed.
    pub fn len(&self) -> usize {
        self.by_inode.len()
    }

    /// True when no open file was indexed (degraded sweep, or nothing open).
    pub fn is_empty(&self) -> bool {
        self.by_inode.is_empty()
    }

    /// Process-table coverage, so the warning can carry the same honesty
    /// caveat as the panel.
    pub fn coverage(&self) -> Coverage {
        self.coverage
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run one `/proc` sweep and build the deleted-but-open [`Ledger`] scoped to
/// `root_dev` (the scan root's filesystem `st_dev`, D2).
///
/// Walks `/proc/[pid]/fd/*`, keeps only inodes with `st_nlink == 0` (D3),
/// dedupes by `(st_dev, st_ino)` keeping all holder PIDs, classifies each into
/// root-filesystem / other-device / RAM-backed, and records process coverage.
///
/// Never fails: a missing or unreadable `/proc` yields an empty ledger with
/// zero coverage and a debug trace (D7). Runs off the UI thread at scan end.
pub fn sweep(root_dev: u64) -> Ledger {
    let collected = collect(Filter::DeletedOnly);
    build_ledger(collected, root_dev)
}

/// Run one `/proc` sweep and build the [`OpenFileIndex`] of **all** open
/// files (D4/D6) for the pre-deletion open-file warning. Same walk as
/// [`sweep`], but without the `st_nlink == 0` filter.
///
/// Never fails: a missing or unreadable `/proc` yields an empty index with
/// zero coverage and a debug trace (D7).
pub fn open_file_index() -> OpenFileIndex {
    let collected = collect(Filter::AllOpen);
    let mut by_inode: FxHashMap<(u64, u64), IndexedOpenFile> =
        FxHashMap::with_capacity_and_hasher(collected.entries.len(), Default::default());
    for entry in collected.entries {
        by_inode.insert(
            (entry.dev, entry.ino),
            IndexedOpenFile {
                evidence: entry.evidence,
                holders: entry.holders,
            },
        );
    }
    OpenFileIndex {
        by_inode,
        coverage: collected.coverage,
    }
}

// ---------------------------------------------------------------------------
// RAM-backed classification
// ---------------------------------------------------------------------------

/// Whether an fd's backing store is RAM (tmpfs/shmem/anon), not the scanned
/// disk, decided from the **readlink evidence prefix**.
///
/// Signal chosen — and why not `st_dev` major 0:
///
/// - `memfd_create(2)` files always resolve to `/memfd:<name> (deleted)`;
/// - POSIX shared memory lives under `/dev/shm/` (a tmpfs mount);
/// - SysV shared memory resolves to `/SYSV<key> (deleted)`;
/// - eventfd/timerfd/signalfd/io_uring/perf and friends resolve to
///   `anon_inode:…` (or bracketed `[eventfd]`-style names on old kernels),
///   and sockets/pipes to `socket:[…]` / `pipe:[…]`.
///
/// Every one of these is a synthetic kernel name for an inode that never
/// occupied a block on the scanned filesystem. The tempting alternative —
/// "`st_dev` major == 0 ⇒ RAM" — is **wrong** here: btrfs subvolumes, and any
/// filesystem mounted on an anonymous block device, also carry major-0
/// `st_dev`s (`get_anon_bdev`), so a major-0 test would misfile a real
/// on-disk deleted file (e.g. a sibling btrfs subvolume's WAL segment) as
/// "RAM, not disk". The prefix test has no such false positive.
///
/// Its known blind spot — a plain file on a tmpfs-mounted `/tmp` or `/run`,
/// whose readlink is an ordinary `/tmp/…` path — is deliberately left to fall
/// into the "other device" bucket rather than be silently reclassified: a
/// labeled other-filesystem line is more honest than a guess (D2's honesty
/// stance).
fn is_ram_backed(evidence: &[u8]) -> bool {
    const PREFIXES: &[&[u8]] = &[
        b"/memfd:",
        b"/dev/shm/",
        b"/SYSV",
        b"anon_inode:",
        b"socket:[",
        b"pipe:[",
    ];
    if PREFIXES.iter().any(|p| evidence.starts_with(p)) {
        return true;
    }
    // Old-kernel bracketed anon inodes: "[eventfd]", "[timerfd]", "[signalfd]".
    evidence.first() == Some(&b'[')
}

// ---------------------------------------------------------------------------
// Internal walk (shared by both entry points)
// ---------------------------------------------------------------------------

/// Which fds a walk keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    /// Only `st_nlink == 0` inodes (the deleted-open ledger).
    DeletedOnly,
    /// Every open file (the open-file index).
    AllOpen,
}

/// A point-in-time stat of the inode behind an fd (the fields we use).
#[derive(Debug, Clone, Copy)]
struct ObservedStat {
    dev: u64,
    ino: u64,
    nlink: u64,
    blocks: u64,
}

/// One deduplicated inode accumulated during a walk.
#[derive(Debug, Clone)]
struct RawEntry {
    dev: u64,
    ino: u64,
    bytes: u64,
    evidence: Vec<u8>,
    is_ram: bool,
    holders: Vec<Holder>,
}

/// Result of a walk before it is shaped into a ledger / index.
#[derive(Debug, Default)]
struct Collected {
    entries: Vec<RawEntry>,
    coverage: Coverage,
}

/// Pure `(dev,ino)` accumulator with holder-merge and the nlink filter —
/// no `/proc` access, so it is unit-testable with synthetic observations.
struct Collector {
    map: FxHashMap<(u64, u64), RawEntry>,
    filter: Filter,
}

impl Collector {
    fn new(filter: Filter) -> Self {
        Self {
            map: FxHashMap::default(),
            filter,
        }
    }

    /// Fold one observed (pid, comm, stat, evidence) tuple in. Applies the
    /// nlink filter, computes `st_blocks*512`, classifies RAM-backed, and
    /// merges the holder (deduped by PID per inode).
    fn observe(&mut self, pid: u32, comm: &Option<String>, st: &ObservedStat, evidence: &[u8]) {
        if self.filter == Filter::DeletedOnly && st.nlink != 0 {
            return;
        }
        let bytes = st.blocks.saturating_mul(Size::BLOCK_UNIT);
        let is_ram = is_ram_backed(evidence);
        let entry = self
            .map
            .entry((st.dev, st.ino))
            .or_insert_with(|| RawEntry {
                dev: st.dev,
                ino: st.ino,
                bytes,
                evidence: evidence.to_vec(),
                is_ram,
                holders: Vec::new(),
            });
        // Holders are added while processing one pid's fds contiguously, so a
        // repeat of this inode within the same pid (e.g. `dup`) lands as the
        // current last holder — a single-element check suffices to dedup.
        if entry.holders.last().is_none_or(|h| h.pid != pid) {
            entry.holders.push(Holder {
                pid,
                comm: comm.clone(),
            });
        }
    }

    fn finish(self) -> Vec<RawEntry> {
        self.map.into_values().collect()
    }
}

/// What opening a process's `fd/` directory meant for coverage accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcOpen {
    /// `fd/` opened: counts toward both `seen` and `readable`.
    Readable,
    /// Permission-gated (`EACCES`/`EPERM`/other): counts toward `seen` only.
    Denied,
    /// Process vanished mid-sweep (`ENOENT`/`ESRCH`): benign, counts nowhere.
    Vanished,
}

/// Map an `fd/` open outcome (`None` = success) to its coverage effect.
/// Pure, so the `ENOENT`-is-benign rule is unit-testable.
fn coverage_effect(err: Option<Errno>) -> ProcOpen {
    match err {
        None => ProcOpen::Readable,
        Some(Errno::NOENT) | Some(Errno::SRCH) => ProcOpen::Vanished,
        Some(_) => ProcOpen::Denied,
    }
}

/// Walk `/proc/[pid]/fd/*` once, applying `filter`, returning deduped inodes
/// plus process coverage. Degrades silently to empty on a missing/unreadable
/// `/proc` (D7).
fn collect(filter: Filter) -> Collected {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let proc_fd = match rustix::fs::open("/proc", flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(errno) => {
            debug!(%errno, "/proc unavailable; freeable sweep degraded to empty");
            return Collected::default();
        }
    };

    let pids = enumerate_pids(&proc_fd);
    let mut collector = Collector::new(filter);
    let mut coverage = Coverage::default();
    let mut fd_buf = [MaybeUninit::<u8>::uninit(); FD_DIRENT_BUF];

    for pid in pids {
        // openat-relative to the /proc fd so a vanished pid is a clean ENOENT
        // and there is no symlink-swap surface on a reconstructed path.
        let fd_dir_path = format!("{pid}/fd");
        let open_result = rustix::fs::openat(&proc_fd, fd_dir_path.as_str(), flags, Mode::empty());
        match coverage_effect(open_result.as_ref().err().copied()) {
            ProcOpen::Vanished => {
                trace!(pid, "process vanished mid-sweep; skipping");
                continue;
            }
            ProcOpen::Denied => {
                coverage.seen += 1;
                if let Err(errno) = &open_result {
                    trace!(pid, %errno, "fd/ unreadable");
                }
                continue;
            }
            ProcOpen::Readable => {
                coverage.seen += 1;
                coverage.readable += 1;
            }
        }
        let fd_dir = match open_result {
            Ok(fd) => fd,
            // Unreachable given the match above, but never panic on /proc.
            Err(_) => continue,
        };

        let comm = read_comm(&proc_fd, pid);
        walk_fd_dir(&fd_dir, pid, &comm, &mut collector, &mut fd_buf);
    }

    Collected {
        entries: collector.finish(),
        coverage,
    }
}

/// Enumerate top-level numeric `/proc` entries (processes, i.e. tgids).
fn enumerate_pids(proc_fd: &OwnedFd) -> Vec<u32> {
    let mut buf = [MaybeUninit::<u8>::uninit(); PROC_DIRENT_BUF];
    let mut iter = RawDir::new(proc_fd.as_fd(), &mut buf);
    let mut pids = Vec::new();
    while let Some(dirent) = iter.next() {
        let Ok(dirent) = dirent else {
            // getdents error mid-listing: keep what we have (D7 skip-continue).
            break;
        };
        if let Some(pid) = parse_pid(dirent.file_name().to_bytes()) {
            pids.push(pid);
        }
    }
    pids
}

/// Parse an all-ASCII-digit dir name into a pid, rejecting everything else
/// (`self`, `sys`, `meminfo`, …).
fn parse_pid(name: &[u8]) -> Option<u32> {
    if name.is_empty() || !name.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(name).ok()?.parse().ok()
}

/// Read one process's `fd/` directory, folding each fd's inode into the
/// collector. Every per-fd failure is skip-and-continue.
fn walk_fd_dir(
    fd_dir: &OwnedFd,
    pid: u32,
    comm: &Option<String>,
    collector: &mut Collector,
    buf: &mut [MaybeUninit<u8>],
) {
    let mut iter = RawDir::new(fd_dir.as_fd(), buf);
    while let Some(dirent) = iter.next() {
        let Ok(dirent) = dirent else {
            // getdents error mid fd/: stop this process, keep the rest.
            break;
        };
        let name = dirent.file_name();
        let name_bytes = name.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        // Evidence: the magic symlink's last-known path (raw bytes).
        let evidence = match rustix::fs::readlinkat(fd_dir.as_fd(), name, Vec::new()) {
            Ok(target) => target.into_bytes(),
            // fd closed between readdir and readlink, or unreadable: skip.
            Err(_) => continue,
        };

        // Target stat: follow the magic symlink (no SYMLINK_NOFOLLOW) to the
        // live inode, even after unlink.
        let stat = match rustix::fs::statat(fd_dir.as_fd(), name, AtFlags::empty()) {
            Ok(stat) => stat,
            Err(_) => continue,
        };

        let observed = ObservedStat {
            dev: stat.st_dev,
            ino: stat.st_ino,
            nlink: stat.st_nlink as u64,
            blocks: stat.st_blocks as u64,
        };
        collector.observe(pid, comm, &observed, &evidence);
    }
}

/// Best-effort read of `/proc/[pid]/comm`, trimmed of its trailing newline.
/// Any failure yields `None` (the holder is still recorded, just unnamed).
fn read_comm(proc_fd: &OwnedFd, pid: u32) -> Option<String> {
    let path = format!("{pid}/comm");
    let fd = rustix::fs::openat(
        proc_fd,
        path.as_str(),
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let mut buf = [0u8; COMM_BUF];
    let n = rustix::io::read(&fd, &mut buf[..]).ok()?;
    let raw = &buf[..n];
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    if raw.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(raw).into_owned())
}

// ---------------------------------------------------------------------------
// Shaping a walk into a Ledger
// ---------------------------------------------------------------------------

/// Classify deduped raw entries into root-fs / other-device / RAM-backed and
/// assemble the [`Ledger`]. Pure — no `/proc` — so classification is
/// unit-testable with synthetic entries.
fn build_ledger(collected: Collected, root_dev: u64) -> Ledger {
    let mut root_fs: Vec<DeletedEntry> = Vec::new();
    let mut other: FxHashMap<u64, Vec<DeletedEntry>> = FxHashMap::default();
    let mut ram_backed_bytes: u64 = 0;
    let mut ram_backed_count: u32 = 0;

    for raw in collected.entries {
        if raw.is_ram {
            ram_backed_bytes = ram_backed_bytes.saturating_add(raw.bytes);
            ram_backed_count += 1;
            continue;
        }
        let entry = DeletedEntry {
            dev: raw.dev,
            ino: raw.ino,
            bytes: raw.bytes,
            evidence: raw.evidence,
            holders: raw.holders,
        };
        if raw.dev == root_dev {
            root_fs.push(entry);
        } else {
            other.entry(raw.dev).or_default().push(entry);
        }
    }

    root_fs.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.ino.cmp(&b.ino)));
    let mut other_devices: Vec<DeviceGroup> = other
        .into_iter()
        .map(|(dev, mut entries)| {
            entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.ino.cmp(&b.ino)));
            DeviceGroup { dev, entries }
        })
        .collect();
    other_devices.sort_by_key(|g| g.dev);

    Ledger {
        root_dev,
        root_fs,
        other_devices,
        ram_backed_bytes,
        ram_backed_count,
        coverage: collected.coverage,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    // --- Pure unit tests (no /proc) ---------------------------------------

    #[test]
    fn ram_backed_prefixes_are_detected() {
        assert!(is_ram_backed(b"/memfd:my-buffer (deleted)"));
        assert!(is_ram_backed(b"/dev/shm/pulse-shm-123"));
        assert!(is_ram_backed(b"/SYSV00001234 (deleted)"));
        assert!(is_ram_backed(b"anon_inode:[eventfd]"));
        assert!(is_ram_backed(b"anon_inode:[io_uring]"));
        assert!(is_ram_backed(b"socket:[45678]"));
        assert!(is_ram_backed(b"pipe:[45678]"));
        assert!(is_ram_backed(b"[eventfd]"));
    }

    #[test]
    fn disk_paths_are_not_ram_backed() {
        // A real deleted disk file — including one on a tmpfs-mounted /tmp,
        // which we deliberately do NOT reclassify (documented blind spot).
        assert!(!is_ram_backed(b"/home/theo/.cache/thing (deleted)"));
        assert!(!is_ram_backed(b"/tmp/#123456 (deleted)"));
        assert!(!is_ram_backed(b"/var/log/app.log (deleted)"));
        // Adversarial: a real file literally named to look synthetic but not
        // matching a prefix.
        assert!(!is_ram_backed(b"/home/user/memfd:notreally"));
    }

    #[test]
    fn dedup_merges_holders_across_pids_and_skips_same_pid_dup() {
        let mut c = Collector::new(Filter::DeletedOnly);
        let st = ObservedStat {
            dev: 58,
            ino: 1834719,
            nlink: 0,
            blocks: 8,
        };
        let comm_a = Some("renderer".to_string());
        let comm_b = Some("gpu".to_string());
        // pid 5775 holds it twice (dup): one holder.
        c.observe(5775, &comm_a, &st, b"/x (deleted)");
        c.observe(5775, &comm_a, &st, b"/x (deleted)");
        // pid 30421 holds the same inode: second holder.
        c.observe(30421, &comm_b, &st, b"/x (deleted)");
        let entries = c.finish();
        assert_eq!(entries.len(), 1, "one inode after dedup");
        let e = &entries[0];
        assert_eq!(e.bytes, 8 * 512);
        assert!(!e.is_ram);
        let pids: Vec<u32> = e.holders.iter().map(|h| h.pid).collect();
        assert_eq!(pids, vec![5775, 30421]);
    }

    #[test]
    fn deleted_only_filter_drops_live_files() {
        let mut c = Collector::new(Filter::DeletedOnly);
        let live = ObservedStat {
            dev: 1,
            ino: 100,
            nlink: 1,
            blocks: 8,
        };
        let deleted = ObservedStat {
            dev: 1,
            ino: 101,
            nlink: 0,
            blocks: 8,
        };
        c.observe(1, &None, &live, b"/live");
        c.observe(1, &None, &deleted, b"/gone (deleted)");
        let entries = c.finish();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ino, 101);
    }

    #[test]
    fn all_open_filter_keeps_live_files() {
        let mut c = Collector::new(Filter::AllOpen);
        let live = ObservedStat {
            dev: 1,
            ino: 100,
            nlink: 3,
            blocks: 8,
        };
        c.observe(1, &None, &live, b"/live");
        assert_eq!(c.finish().len(), 1);
    }

    #[test]
    fn coverage_accounting_distinguishes_enoent_from_eacces() {
        assert_eq!(coverage_effect(None), ProcOpen::Readable);
        assert_eq!(coverage_effect(Some(Errno::NOENT)), ProcOpen::Vanished);
        assert_eq!(coverage_effect(Some(Errno::SRCH)), ProcOpen::Vanished);
        assert_eq!(coverage_effect(Some(Errno::ACCESS)), ProcOpen::Denied);
        assert_eq!(coverage_effect(Some(Errno::PERM)), ProcOpen::Denied);
    }

    #[test]
    fn coverage_partial_and_unreadable() {
        let cov = Coverage {
            seen: 505,
            readable: 140,
        };
        assert_eq!(cov.unreadable(), 365);
        assert!(cov.is_partial());
        let full = Coverage {
            seen: 10,
            readable: 10,
        };
        assert!(!full.is_partial());
        assert_eq!(full.unreadable(), 0);
    }

    #[test]
    fn build_ledger_classifies_by_dev_and_ram() {
        let entries = vec![
            RawEntry {
                dev: 42,
                ino: 1,
                bytes: 4096,
                evidence: b"/root/a (deleted)".to_vec(),
                is_ram: false,
                holders: vec![Holder { pid: 1, comm: None }],
            },
            RawEntry {
                dev: 99,
                ino: 2,
                bytes: 8192,
                evidence: b"/mnt/other/b (deleted)".to_vec(),
                is_ram: false,
                holders: vec![],
            },
            RawEntry {
                dev: 0,
                ino: 3,
                bytes: 16384,
                evidence: b"/memfd:x (deleted)".to_vec(),
                is_ram: true,
                holders: vec![],
            },
        ];
        let ledger = build_ledger(
            Collected {
                entries,
                coverage: Coverage {
                    seen: 3,
                    readable: 3,
                },
            },
            42,
        );
        assert_eq!(ledger.root_fs_entry_count(), 1);
        assert_eq!(ledger.root_fs_freeable_bytes(), 4096);
        assert_eq!(ledger.other_device_groups().len(), 1);
        assert_eq!(ledger.other_device_groups()[0].dev, 99);
        assert_eq!(ledger.other_device_freeable_bytes(), 8192);
        assert_eq!(ledger.ram_backed_bytes(), 16384);
        assert_eq!(ledger.ram_backed_count(), 1);
        assert!(!ledger.is_empty());
    }

    #[test]
    fn empty_degraded_ledger() {
        let ledger = build_ledger(Collected::default(), 42);
        assert!(ledger.is_empty());
        assert_eq!(ledger.coverage(), Coverage::default());
        assert_eq!(ledger.root_fs_freeable_bytes(), 0);
    }

    // --- Integration tests (require a live /proc) -------------------------

    /// True when this kernel exposes the `/proc` interfaces the sweep needs.
    fn proc_available() -> bool {
        std::path::Path::new("/proc/self/fd").exists()
    }

    #[test]
    fn gold_case_deleted_open_file_is_found_then_gone() {
        if !proc_available() {
            eprintln!("skipping gold_case: /proc/self/fd unavailable on this host");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("freeable-gold.bin");
        let mut file = std::fs::File::create(&path).expect("create");
        // Write enough to allocate real blocks (>0 st_blocks).
        file.write_all(&vec![0xABu8; 256 * 1024]).expect("write");
        file.flush().expect("flush");

        let meta = file.metadata().expect("metadata");
        let root_dev = meta.dev();
        let ino = meta.ino();
        let expected_bytes = meta.blocks() * Size::BLOCK_UNIT;
        assert!(expected_bytes > 0, "file should occupy >0 blocks");

        // Unlink while the descriptor stays open: now st_nlink == 0.
        std::fs::remove_file(&path).expect("unlink");

        let ledger = sweep(root_dev);
        let entry = ledger
            .root_fs_entries()
            .iter()
            .find(|e| e.dev == root_dev && e.ino == ino)
            .unwrap_or_else(|| {
                panic!(
                    "our deleted-open inode {ino} not found on dev {root_dev}; \
                     root_fs had {} entries",
                    ledger.root_fs_entry_count()
                )
            });
        assert_eq!(entry.bytes, expected_bytes, "size from st_blocks*512");
        let me = std::process::id();
        assert!(
            entry.holders.iter().any(|h| h.pid == me),
            "our own pid {me} should be a holder, got {:?}",
            entry.holders
        );

        // Close and re-sweep: the inode is freed, no longer found.
        drop(file);
        let after = sweep(root_dev);
        assert!(
            after
                .root_fs_entries()
                .iter()
                .all(|e| !(e.dev == root_dev && e.ino == ino)),
            "closed inode must not reappear"
        );
    }

    #[test]
    fn open_file_index_finds_live_file() {
        if !proc_available() {
            eprintln!("skipping open_file_index test: /proc/self/fd unavailable");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("freeable-open.bin");
        let file = std::fs::File::create(&path).expect("create");
        let meta = file.metadata().expect("metadata");
        let (dev, ino) = (meta.dev(), meta.ino());

        let index = open_file_index();
        let holders = index
            .holders(dev, ino)
            .expect("our still-open, non-deleted file should be indexed");
        let me = std::process::id();
        assert!(
            holders.iter().any(|h| h.pid == me),
            "our pid {me} should hold the open file"
        );

        // D6 amendment: the index also retains the evidence path itself
        // (not just holders), so the pre-deletion warning can match a
        // marked *directory* by path-prefix containment, not only a
        // marked file's own (dev, ino).
        let (evidence, found_dev, found_ino, iter_holders) = index
            .iter()
            .find(|&(_, d, i, _)| d == dev && i == ino)
            .expect("our open file should be reachable through iter() too");
        assert_eq!(found_dev, dev);
        assert_eq!(found_ino, ino);
        assert_eq!(
            evidence,
            path.as_os_str().as_bytes(),
            "the indexed evidence path matches our file's real path"
        );
        assert!(iter_holders.iter().any(|h| h.pid == me));

        drop(file);
    }

    #[test]
    fn memfd_is_classified_ram_backed_not_disk() {
        if !proc_available() {
            eprintln!("skipping memfd test: /proc/self/fd unavailable");
            return;
        }
        use rustix::fs::MemfdFlags;
        let memfd = rustix::fs::memfd_create("camembert-freeable-test", MemfdFlags::empty())
            .expect("memfd_create");
        // Give it real blocks.
        rustix::fs::ftruncate(&memfd, 256 * 1024).expect("ftruncate");
        rustix::io::write(&memfd, &[0x5Au8; 4096]).expect("write");

        let st = rustix::fs::fstat(&memfd).expect("fstat");
        assert_eq!(st.st_nlink, 0, "memfd is deleted-from-birth");
        let (dev, ino) = (st.st_dev, st.st_ino);

        // Pass the memfd's OWN dev as root_dev: RAM classification must still
        // win over the device match, proving the signal is the readlink
        // prefix, not st_dev.
        let ledger = sweep(dev);
        assert!(
            ledger.ram_backed_count() >= 1,
            "at least our memfd should be RAM-backed"
        );
        assert!(
            ledger.ram_backed_bytes() >= 4096,
            "RAM-backed bytes should include our memfd's allocation"
        );
        assert!(
            ledger
                .root_fs_entries()
                .iter()
                .all(|e| !(e.dev == dev && e.ino == ino)),
            "memfd must NOT appear as a root-fs disk entry"
        );
        for group in ledger.other_device_groups() {
            assert!(
                group
                    .entries
                    .iter()
                    .all(|e| !(e.dev == dev && e.ino == ino)),
                "memfd must NOT appear as an other-device disk entry"
            );
        }
        drop(memfd);
    }

    /// Bench-style timing, ignored by default. Run with:
    /// `cargo test -p camembert-core --release freeable::tests::bench_sweep_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "timing/bench; run explicitly with --ignored --nocapture"]
    fn bench_sweep_cost() {
        if !proc_available() {
            eprintln!("skipping bench: /proc/self/fd unavailable");
            return;
        }
        let root_dev = std::fs::metadata("/").map(|m| m.dev()).unwrap_or(0);
        let start = std::time::Instant::now();
        let ledger = sweep(root_dev);
        let elapsed = start.elapsed();
        eprintln!(
            "freeable sweep: {:.2?} — coverage {}/{} procs readable, \
             {} root-fs entries, {} RAM inodes",
            elapsed,
            ledger.coverage().readable,
            ledger.coverage().seen,
            ledger.root_fs_entry_count(),
            ledger.ram_backed_count(),
        );
    }
}
