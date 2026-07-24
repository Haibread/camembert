//! Freeable phase 2 — extent mapping (FIEMAP) and the selection oracle.
//!
//! Physical space actually reclaimed by deleting a selection: on
//! extent-sharing filesystems (btrfs, XFS reflink) the naive `Σ disk` sum
//! over-promises whenever extents are shared with files outside the
//! selection — snapshots, `cp --reflink` copies, dedup. This module maps a
//! selection's extents with `FS_IOC_FIEMAP` and correlates the shared
//! ones by physical address ([`correlate`]), splitting bytes into honest
//! buckets instead of one optimistic number (decisions D4/D5,
//! `docs/design/freeable2-decisions.md`).
//!
//! ## The honesty contract (D2, attack-b findings 1/4/5/6)
//!
//! - **Units are allocated-logical bytes** — `Σ fe_length`, the same unit
//!   as the existing `disk` column. On `compress`-mounted filesystems the
//!   physical reclaim can be smaller (up to the compression ratio); the
//!   caller detects those mounts via
//!   [`crate::scan::path_on_compressed_mount`] and words the caveat.
//!   FIEMAP cannot expose compressed byte counts unprivileged, so no
//!   figure here ever guesses at them.
//! - **`shared_within` is a ceiling**, never a promise: physical bytes
//!   referenced ≥ 2 times *by the selection* are freed only if the whole
//!   selection goes **and** no referencer this scan cannot see (a
//!   snapshot, an unscanned subvolume) also holds them. Word it "up to".
//! - **`shared_outside` merges two unprivileged-inseparable cases**:
//!   shared with a scanned file outside the selection, or with something
//!   the scan cannot see at all. Separating them needs root
//!   (`LOGICAL_INO`) — no figure rather than a guess.
//! - **The SHARED bit is trusted on kernels ≥ 6.1 only**
//!   ([`shared_bit_reliable`]): before the backref rewrite, concurrent
//!   COW could yield false-unset SHARED, making `exclusive` overstate.
//!   The oracle still runs on older kernels; the caller prints a
//!   may-overstate caveat.
//! - **Failure downgrades, never guesses**: FIEMAP failing on an
//!   extent-capable filesystem lands the file in `unknown` — claiming
//!   "exclusive" for bytes we declined to map would reintroduce the lie
//!   (`--no-fiemap` semantics, attack-b finding 6). ZFS gets no figures
//!   at all, not even the hardlink tier (block cloning is pool-level and
//!   invisible per-file, D5).
//! - **`FIEMAP_FLAG_SYNC` is never set**: it costs ~7× plus an unbounded
//!   writeback tail. Unflushed (delalloc) extents land in `unknown`
//!   instead.
//!
//! Module isolation (phase-1 D8, phase-2 D6): nothing here touches the
//! 32-byte [`crate::tree::Node`], dumps, or diff — plain data in, plain
//! data out.

use std::io;
use std::path::Path;

use rustc_hash::{FxHashMap, FxHashSet};
use rustix::fs::{FileType, Mode, OFlags};
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, opcode};

// ---------------------------------------------------------------------------
// FIEMAP ABI (linux/fiemap.h)
// ---------------------------------------------------------------------------

/// Extents fetched per `FS_IOC_FIEMAP` call. 256 × 56 B ≈ 14 KiB per
/// buffer; files with more extents paginate (the loop in [`map_file`]).
const EXTENT_BATCH: usize = 256;

/// `FIEMAP_MAX_OFFSET`: map to the end of the file.
const FIEMAP_MAX_OFFSET: u64 = u64::MAX;

/// Last extent of the file — stop paginating.
const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
/// Data location unknown.
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
/// Delayed allocation: not yet flushed, no physical address (implies
/// UNKNOWN in the kernel; tested separately for robustness).
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
/// Data packed inline in metadata: `fe_physical` is meaningless for
/// cross-file correlation.
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0000_0200;
/// Extent referenced more than once (reflink, snapshot, dedup — or the
/// same file twice).
const FIEMAP_EXTENT_SHARED: u32 = 0x0000_2000;

/// `struct fiemap` (the fixed 32-byte header; the extent array follows in
/// memory — [`FiemapBuf`]).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FiemapHeader {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
}

/// `struct fiemap_extent` (56 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

impl FiemapExtent {
    const ZERO: Self = Self {
        fe_logical: 0,
        fe_physical: 0,
        fe_length: 0,
        fe_reserved64: [0; 2],
        fe_flags: 0,
        fe_reserved: [0; 3],
    };
}

/// The ioctl argument: header + inline extent array, exactly the layout
/// the kernel writes into.
#[repr(C)]
struct FiemapBuf {
    header: FiemapHeader,
    extents: [FiemapExtent; EXTENT_BATCH],
}

/// `FS_IOC_FIEMAP = _IOWR('f', 11, struct fiemap)` — the opcode size is
/// the fixed header's, per the kernel definition; the extent array's
/// length travels in `fm_extent_count`.
struct FiemapIoctl<'a> {
    buf: &'a mut FiemapBuf,
}

// SAFETY: the opcode matches `_IOWR('f', 11, struct fiemap)`; the pointer
// is a live, properly `#[repr(C)]`-laid-out buffer whose trailing extent
// array has the `fm_extent_count` capacity the header declares, so the
// kernel's writes stay in bounds.
unsafe impl Ioctl for FiemapIoctl<'_> {
    type Output = ();
    const IS_MUTATING: bool = true;

    fn opcode(&self) -> Opcode {
        opcode::read_write::<FiemapHeader>(b'f', 11)
    }

    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        std::ptr::from_mut(self.buf).cast()
    }

    unsafe fn output_from_ptr(
        _out: IoctlOutput,
        _extract: *mut core::ffi::c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Per-file extent map
// ---------------------------------------------------------------------------

/// A physical byte range on one device, correlatable across files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysRange {
    /// First physical byte (`fe_physical`).
    pub start: u64,
    /// Length in bytes (`fe_length` — allocated-logical, D2).
    pub len: u64,
}

/// One regular file's extents, bucketed for the oracle. All byte figures
/// are allocated-**logical** (`Σ fe_length`): on compressed mounts the
/// physical footprint can be smaller, and no field here claims otherwise.
#[derive(Debug, Clone)]
pub struct FileMap {
    /// `st_dev` — physical addresses only correlate within one device.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// Bytes in extents with `SHARED` unset and a usable mapping: exactly
    /// one referencer filesystem-wide, freed by deleting this file
    /// (understates physical reclaim on bookends, never overstates on
    /// kernels ≥ 6.1 — see [`shared_bit_reliable`]).
    pub exclusive: u64,
    /// Physical ranges of `SHARED`, non-inline extents — the correlation
    /// input. Inline extents never appear here (their `fe_physical` is
    /// metadata-relative garbage).
    pub shared: Vec<PhysRange>,
    /// Bytes in **all** `SHARED` extents, inline included. The excess
    /// over `shared` (the unpushed inline part) cannot be correlated and
    /// lands in "shared outside or unseen".
    pub shared_logical: u64,
    /// Delalloc/unknown-flagged bytes: not yet flushed or unmappable —
    /// honestly unknown, never guessed into a bucket.
    pub unknown: u64,
}

/// FIEMAP one regular file into a [`FileMap`].
///
/// Opens `O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK`, verifies a
/// regular file via `fstat`, then runs the mandatory pagination loop
/// ([`EXTENT_BATCH`] extents per call, `fm_start` advanced past the last
/// returned extent, stop on `FIEMAP_EXTENT_LAST` or an empty batch).
/// `FIEMAP_FLAG_SYNC` is **never** set (module docs).
///
/// Errors surface as-is — notably `EOPNOTSUPP`/`ENOTTY` from filesystems
/// without FIEMAP; the caller downgrades the file (to `unknown` on an
/// extent-capable filesystem, per [`correlate`] rule 4).
pub fn map_file(path: &Path) -> io::Result<FileMap> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = rustix::fs::open(path, flags, Mode::empty())?;
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FIEMAP target is not a regular file",
        ));
    }

    let mut map = FileMap {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        exclusive: 0,
        shared: Vec::new(),
        shared_logical: 0,
        unknown: 0,
    };

    // ~14 KiB: boxed so deep call stacks (UI threads) stay small.
    let mut buf = Box::new(FiemapBuf {
        header: FiemapHeader {
            fm_start: 0,
            fm_length: FIEMAP_MAX_OFFSET,
            fm_flags: 0,
            fm_mapped_extents: 0,
            fm_extent_count: 0,
            fm_reserved: 0,
        },
        extents: [FiemapExtent::ZERO; EXTENT_BATCH],
    });

    let mut start: u64 = 0;
    loop {
        buf.header = FiemapHeader {
            fm_start: start,
            fm_length: FIEMAP_MAX_OFFSET,
            // NEVER FIEMAP_FLAG_SYNC (7.3× cost, unbounded writeback
            // tail): delalloc extents go to `unknown` instead.
            fm_flags: 0,
            fm_mapped_extents: 0,
            fm_extent_count: EXTENT_BATCH as u32,
            fm_reserved: 0,
        };
        // SAFETY: `FiemapIoctl` describes exactly this buffer (see its
        // `Ioctl` impl); the fd is open and owned for the duration.
        unsafe { rustix::ioctl::ioctl(&fd, FiemapIoctl { buf: &mut buf })? };

        let returned = (buf.header.fm_mapped_extents as usize).min(EXTENT_BATCH);
        if returned == 0 {
            break;
        }
        let mut saw_last = false;
        for extent in &buf.extents[..returned] {
            bucket_extent(extent, &mut map);
            saw_last |= extent.fe_flags & FIEMAP_EXTENT_LAST != 0;
            start = extent.fe_logical.saturating_add(extent.fe_length);
        }
        if saw_last {
            break;
        }
    }
    Ok(map)
}

/// Bucket one extent per the D4 rules (module docs): delalloc/unknown →
/// `unknown`; shared → `shared_logical` (+ a [`PhysRange`] when
/// correlatable, i.e. not inline); everything else — UNWRITTEN prealloc
/// and ENCODED compressed extents included — counts its logical length
/// as exclusive.
fn bucket_extent(extent: &FiemapExtent, map: &mut FileMap) {
    let flags = extent.fe_flags;
    if flags & (FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_UNKNOWN) != 0 {
        map.unknown += extent.fe_length;
    } else if flags & FIEMAP_EXTENT_SHARED != 0 {
        map.shared_logical += extent.fe_length;
        if flags & FIEMAP_EXTENT_DATA_INLINE == 0 {
            map.shared.push(PhysRange {
                start: extent.fe_physical,
                len: extent.fe_length,
            });
        }
    } else {
        map.exclusive += extent.fe_length;
    }
}

// ---------------------------------------------------------------------------
// Filesystem tiers (D5)
// ---------------------------------------------------------------------------

/// `BTRFS_SUPER_MAGIC`.
const BTRFS_MAGIC: u64 = 0x9123_683E;
/// `XFS_SUPER_MAGIC` ("XFSB").
const XFS_MAGIC: u64 = 0x5846_5342;
/// `ZFS_SUPER_MAGIC`.
const ZFS_MAGIC: u64 = 0x2FC1_2FC1;

/// What sharing machinery a filesystem supports (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsTier {
    /// btrfs / XFS: full FIEMAP extent machinery (`EOPNOTSUPP` on an
    /// individual file downgrades it to `unknown`).
    Extent,
    /// ext family and every other real filesystem: no reflink exists, so
    /// the D4 hardlink rule alone is **exact** — `disk` is exclusive.
    HardlinkOnly,
    /// ZFS: block cloning is pool-level with no per-file API; even the
    /// hardlink tier could overstate. No figure rather than a guess.
    Zfs,
}

/// Classify `path`'s filesystem by statfs magic (the
/// `classify_mount` idiom from the scan worker). A statfs failure is
/// [`FsTier::HardlinkOnly`] — the tier that claims nothing about
/// extents.
pub fn tier_of(path: &Path) -> FsTier {
    match rustix::fs::statfs(path) {
        Ok(statfs) => tier_of_magic(statfs.f_type as u64),
        Err(_) => FsTier::HardlinkOnly,
    }
}

/// [`tier_of`] on an already-fetched statfs magic.
fn tier_of_magic(f_type: u64) -> FsTier {
    match f_type {
        BTRFS_MAGIC | XFS_MAGIC => FsTier::Extent,
        ZFS_MAGIC => FsTier::Zfs,
        _ => FsTier::HardlinkOnly,
    }
}

// ---------------------------------------------------------------------------
// Kernel gate (attack-b finding 5)
// ---------------------------------------------------------------------------

/// Whether this kernel's `FIEMAP_EXTENT_SHARED` bit is reliable under
/// concurrent COW: true iff the running kernel is ≥ 6.1 (the btrfs
/// backref rewrite; before it, a racing writer could yield false-unset
/// SHARED and the oracle's `exclusive` could overstate). The oracle
/// still runs below 6.1 — the caller prints a may-overstate caveat.
/// Unparseable release strings are `false` (conservative).
pub fn shared_bit_reliable() -> bool {
    match rustix::system::uname().release().to_str() {
        Ok(release) => release_at_least_6_1(release),
        Err(_) => false,
    }
}

/// Parse a `uname -r` release ("6.1.0-13-amd64", "7.1.4-1-cachyos") and
/// compare `major.minor` against 6.1. Defensive: anything that does not
/// start with `<digits>.<digits>` is `false`.
fn release_at_least_6_1(release: &str) -> bool {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let Some(major) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
        return false;
    };
    let Some(minor) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
        // A bare major ("7") is unambiguous only above the gate.
        return major > 6;
    };
    major > 6 || (major == 6 && minor >= 1)
}

// ---------------------------------------------------------------------------
// The correlation oracle (D4) — pure, in-memory
// ---------------------------------------------------------------------------

/// The D4 hardlink rule's verdict for one inode, decided by the caller
/// from [`crate::scan::ScanOutcome::hardlink_groups`]: which of the
/// inode's links the selection holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStatus {
    /// `nlink == 1` (absent from the hardlink registry).
    Single,
    /// Every link the scan saw is selected **and** the scan saw every
    /// link that exists (`group.nodes.len() as u32 == nlink`).
    AllLinksSelected,
    /// At least one link survives outside the selection (selected or
    /// not, scanned or not): deleting the selection frees nothing of
    /// this inode.
    LinksOutside,
}

/// One distinct `(dev, ino)` of the selection, assembled by the caller.
#[derive(Debug, Clone)]
pub struct OracleInput {
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// `st_blocks * 512` from a **fresh** stat (extent sharing is
    /// volatile; scan-time sizes may be stale).
    pub disk: u64,
    /// The device's [`FsTier`].
    pub tier: FsTier,
    /// The D4 hardlink verdict (caller-computed).
    pub links: LinkStatus,
    /// `Some` only on [`FsTier::Extent`] when [`map_file`] succeeded.
    pub map: Option<FileMap>,
}

/// The oracle's bucketed answer. Every field documents its honesty
/// direction — that documentation is load-bearing (D2): the UI's wording
/// must match it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OracleReport {
    /// Freed for sure, as far as FIEMAP truth reaches: understates
    /// physical reclaim (bookends, compression) and never overstates on
    /// kernels ≥ 6.1 (below, see [`shared_bit_reliable`] — the caller
    /// caveats). Allocated-logical bytes.
    pub exclusive: u64,
    /// **Ceiling** ("up to", never a promise): physical bytes referenced
    /// ≥ 2 times by the selection, counted once. Freed only if the whole
    /// selection goes **and** no unseen referencer (snapshot, unscanned
    /// file) also holds them — an extent shared by two selected files
    /// plus one external referencer still lands here.
    pub shared_within: u64,
    /// Will **not** be freed by this deletion: shared with a scanned
    /// file outside the selection *or* with something this scan cannot
    /// see — inseparable unprivileged, merged on purpose. Includes
    /// hardlink-pinned bytes ([`LinkStatus::LinksOutside`]) and
    /// uncorrelatable inline-shared bytes. Exact as a "not freed now"
    /// statement.
    pub shared_outside: u64,
    /// Honestly unknown: delalloc plus files FIEMAP failed to map on an
    /// extent-capable filesystem. Could go either way; never folded into
    /// a claim.
    pub unknown: u64,
    /// Distinct `(dev, ino)` considered.
    pub inodes: u64,
    /// Inodes whose [`FileMap`] was actually consumed (extent tier,
    /// FIEMAP succeeded, not hardlink-pinned).
    pub mapped: u64,
    /// Inodes refused by the D4 link rule (their `disk` sits in
    /// `shared_outside`).
    pub hardlink_outside: u64,
    /// Inodes on ZFS: no byte claims at all (D5) — reported only as this
    /// count plus `zfs_bytes` so the UI can say "N files on ZFS not
    /// estimated".
    pub zfs_files: u64,
    /// `Σ disk` of the ZFS inodes — what the naive sum *would* have
    /// claimed; never added to any other bucket.
    pub zfs_bytes: u64,
}

/// Correlate a selection (one [`OracleInput`] per distinct `(dev, ino)` —
/// duplicates are dropped defensively) into an [`OracleReport`].
///
/// Bucketing rules, in priority order per inode:
///
/// 1. [`FsTier::Zfs`] → `zfs_bytes`/`zfs_files` only (D5: no figure
///    rather than a guess).
/// 2. [`LinkStatus::LinksOutside`] → `shared_outside += disk`: a
///    surviving link pins every byte regardless of extent sharing (D4).
/// 3. [`FsTier::HardlinkOnly`] → `exclusive += disk` (exact: no reflink
///    exists there).
/// 4. [`FsTier::Extent`] without a map (FIEMAP failed, `--no-fiemap`) →
///    `unknown += disk` — never "exclusive" on a filesystem we declined
///    to map (attack-b finding 6).
/// 5. [`FsTier::Extent`] with a map → the map's exclusive/unknown pass
///    through; shared extents enter a per-device byte-granularity
///    interval sweep: covered ≥ 2 → `shared_within` (once), covered
///    exactly once → `shared_outside`. A self-reflinked file's two
///    mappings of one range count as two covers — correctly, since
///    deleting the file drops both.
pub fn correlate(files: &[OracleInput]) -> OracleReport {
    let mut report = OracleReport::default();
    let mut seen: FxHashSet<(u64, u64)> = FxHashSet::default();
    let mut per_device: FxHashMap<u64, Vec<PhysRange>> = FxHashMap::default();

    for file in files {
        if !seen.insert((file.dev, file.ino)) {
            debug_assert!(
                false,
                "correlate: duplicate (dev={}, ino={}) in oracle input",
                file.dev, file.ino
            );
            continue;
        }
        report.inodes += 1;

        // Rule 1: ZFS says nothing, not even the hardlink tier.
        if file.tier == FsTier::Zfs {
            report.zfs_files += 1;
            report.zfs_bytes += file.disk;
            continue;
        }
        // Rule 2: a surviving link pins every byte.
        if file.links == LinkStatus::LinksOutside {
            report.hardlink_outside += 1;
            report.shared_outside += file.disk;
            continue;
        }
        match (file.tier, &file.map) {
            // Rule 3: no reflink on this filesystem — disk is exact.
            (FsTier::HardlinkOnly, _) => report.exclusive += file.disk,
            // Rule 4: extent-capable but unmapped — honest unknown.
            (FsTier::Extent, None) => report.unknown += file.disk,
            // Rule 5: FIEMAP truth.
            (FsTier::Extent, Some(map)) => {
                report.mapped += 1;
                report.exclusive += map.exclusive;
                report.unknown += map.unknown;
                // Inline-shared bytes have no correlatable address: they
                // can never be proven shared-within, so they land in the
                // merged outside-or-unseen bucket.
                let pushed: u64 = map.shared.iter().map(|r| r.len).sum();
                report.shared_outside += map.shared_logical.saturating_sub(pushed);
                per_device
                    .entry(file.dev)
                    .or_default()
                    .extend(map.shared.iter().copied());
            }
            (FsTier::Zfs, _) => unreachable!("rule 1 consumed ZFS inputs"),
        }
    }

    for ranges in per_device.values() {
        let (within, outside) = sweep(ranges);
        report.shared_within += within;
        report.shared_outside += outside;
    }
    report
}

/// Byte-granularity boundary sweep over one device's shared ranges:
/// returns `(covered ≥ 2 — counted once, covered exactly once)` in bytes.
fn sweep(ranges: &[PhysRange]) -> (u64, u64) {
    let mut events: Vec<(u64, i32)> = Vec::with_capacity(ranges.len() * 2);
    for range in ranges {
        if range.len == 0 {
            continue;
        }
        events.push((range.start, 1));
        events.push((range.start.saturating_add(range.len), -1));
    }
    // Ties sort ends (-1) before starts (+1): two ranges that merely
    // touch never look overlapped.
    events.sort_unstable();

    let (mut within, mut once) = (0u64, 0u64);
    let mut coverage: i64 = 0;
    let mut prev: u64 = 0;
    for (pos, delta) in events {
        let span = pos - prev;
        match coverage {
            0 => {}
            1 => once += span,
            _ => within += span,
        }
        prev = pos;
        coverage += i64::from(delta);
    }
    debug_assert_eq!(coverage, 0, "sweep events must balance");
    (within, once)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- interval sweep --------------------------------------------------

    fn range(start: u64, len: u64) -> PhysRange {
        PhysRange { start, len }
    }

    #[test]
    fn sweep_empty_input_is_zero() {
        assert_eq!(sweep(&[]), (0, 0));
    }

    #[test]
    fn sweep_disjoint_ranges_are_covered_once() {
        assert_eq!(sweep(&[range(0, 100), range(200, 50)]), (0, 150));
    }

    #[test]
    fn sweep_touching_ranges_do_not_overlap() {
        assert_eq!(sweep(&[range(0, 100), range(100, 100)]), (0, 200));
    }

    #[test]
    fn sweep_exact_overlap_counts_once_as_within() {
        assert_eq!(sweep(&[range(4096, 8192), range(4096, 8192)]), (8192, 0));
    }

    #[test]
    fn sweep_partial_overlap_splits() {
        // [0,100) and [60,160): 40 bytes doubly covered, 120 singly.
        assert_eq!(sweep(&[range(0, 100), range(60, 100)]), (40, 120));
    }

    #[test]
    fn sweep_triple_coverage_still_counts_once() {
        let ranges = [range(0, 100), range(0, 100), range(0, 100)];
        assert_eq!(sweep(&ranges), (100, 0));
    }

    #[test]
    fn sweep_zero_length_ranges_are_ignored() {
        assert_eq!(sweep(&[range(10, 0), range(10, 0)]), (0, 0));
    }

    // --- correlate bucketing ---------------------------------------------

    fn extent_map(dev: u64, ino: u64, exclusive: u64, shared: &[PhysRange]) -> FileMap {
        FileMap {
            dev,
            ino,
            exclusive,
            shared: shared.to_vec(),
            shared_logical: shared.iter().map(|r| r.len).sum(),
            unknown: 0,
        }
    }

    fn input(dev: u64, ino: u64, disk: u64, tier: FsTier) -> OracleInput {
        OracleInput {
            dev,
            ino,
            disk,
            tier,
            links: LinkStatus::Single,
            map: None,
        }
    }

    #[test]
    fn empty_selection_is_an_empty_report() {
        assert_eq!(correlate(&[]), OracleReport::default());
    }

    #[test]
    fn rule_1_zfs_gets_no_byte_claims() {
        let report = correlate(&[input(1, 1, 4096, FsTier::Zfs)]);
        assert_eq!(report.zfs_files, 1);
        assert_eq!(report.zfs_bytes, 4096);
        assert_eq!(report.exclusive, 0);
        assert_eq!(report.shared_within, 0);
        assert_eq!(report.shared_outside, 0);
        assert_eq!(report.unknown, 0);
        assert_eq!(report.inodes, 1);
    }

    #[test]
    fn rule_2_links_outside_pins_all_bytes() {
        let mut file = input(1, 1, 8192, FsTier::HardlinkOnly);
        file.links = LinkStatus::LinksOutside;
        let report = correlate(&[file]);
        assert_eq!(report.shared_outside, 8192);
        assert_eq!(report.hardlink_outside, 1);
        assert_eq!(report.exclusive, 0);
    }

    #[test]
    fn rule_2_beats_the_extent_tier_and_its_map() {
        // Even with a FileMap full of exclusive bytes, a surviving link
        // outside the selection pins the whole inode.
        let mut file = input(1, 1, 8192, FsTier::Extent);
        file.links = LinkStatus::LinksOutside;
        file.map = Some(extent_map(1, 1, 8192, &[]));
        let report = correlate(&[file]);
        assert_eq!(report.shared_outside, 8192);
        assert_eq!(report.hardlink_outside, 1);
        assert_eq!(report.exclusive, 0);
        assert_eq!(report.mapped, 0, "an unused map is not 'mapped'");
    }

    #[test]
    fn rule_3_hardlink_only_tier_is_exact_disk() {
        let report = correlate(&[input(1, 1, 12288, FsTier::HardlinkOnly)]);
        assert_eq!(report.exclusive, 12288);
        assert_eq!(report.unknown, 0);
    }

    #[test]
    fn rule_4_unmapped_extent_tier_is_unknown_never_exclusive() {
        let report = correlate(&[input(1, 1, 12288, FsTier::Extent)]);
        assert_eq!(report.unknown, 12288);
        assert_eq!(report.exclusive, 0, "attack-b finding 6");
        assert_eq!(report.mapped, 0);
    }

    #[test]
    fn rule_5_map_buckets_pass_through() {
        let mut file = input(1, 1, 20480, FsTier::Extent);
        file.map = Some(FileMap {
            dev: 1,
            ino: 1,
            exclusive: 8192,
            shared: vec![range(0, 4096)],
            shared_logical: 4096,
            unknown: 2048,
        });
        let report = correlate(&[file]);
        assert_eq!(report.exclusive, 8192);
        assert_eq!(report.unknown, 2048);
        // Sole cover of its shared range: outside the selection.
        assert_eq!(report.shared_outside, 4096);
        assert_eq!(report.shared_within, 0);
        assert_eq!(report.mapped, 1);
    }

    #[test]
    fn shared_ranges_covered_by_two_selected_files_are_within() {
        let mut a = input(1, 1, 4096, FsTier::Extent);
        a.map = Some(extent_map(1, 1, 0, &[range(1 << 20, 4096)]));
        let mut b = input(1, 2, 4096, FsTier::Extent);
        b.map = Some(extent_map(1, 2, 0, &[range(1 << 20, 4096)]));
        let report = correlate(&[a, b]);
        assert_eq!(report.shared_within, 4096, "counted once, not twice");
        assert_eq!(report.shared_outside, 0);
        assert_eq!(report.mapped, 2);
    }

    #[test]
    fn same_physical_range_on_two_devices_does_not_correlate() {
        let mut a = input(1, 1, 4096, FsTier::Extent);
        a.map = Some(extent_map(1, 1, 0, &[range(1 << 20, 4096)]));
        let mut b = input(2, 1, 4096, FsTier::Extent);
        b.map = Some(extent_map(2, 1, 0, &[range(1 << 20, 4096)]));
        let report = correlate(&[a, b]);
        assert_eq!(report.shared_within, 0);
        assert_eq!(report.shared_outside, 8192);
    }

    #[test]
    fn self_reflink_two_mappings_of_one_range_count_as_two_covers() {
        // One file mapping the same physical range at two offsets:
        // deleting the file drops both references, so the range is
        // genuinely freed-if-selection-goes — shared_within.
        let mut file = input(1, 1, 8192, FsTier::Extent);
        file.map = Some(extent_map(1, 1, 0, &[range(0, 4096), range(0, 4096)]));
        let report = correlate(&[file]);
        assert_eq!(report.shared_within, 4096);
        assert_eq!(report.shared_outside, 0);
    }

    #[test]
    fn inline_shared_bytes_land_in_shared_outside() {
        // shared_logical exceeds the pushed ranges by the inline part.
        let mut file = input(1, 1, 4096, FsTier::Extent);
        file.map = Some(FileMap {
            dev: 1,
            ino: 1,
            exclusive: 0,
            shared: vec![],
            shared_logical: 512,
            unknown: 0,
        });
        let report = correlate(&[file]);
        assert_eq!(report.shared_outside, 512);
        assert_eq!(report.shared_within, 0);
    }

    #[test]
    fn all_links_selected_proceeds_to_extent_bucketing() {
        let mut file = input(1, 1, 4096, FsTier::Extent);
        file.links = LinkStatus::AllLinksSelected;
        file.map = Some(extent_map(1, 1, 4096, &[]));
        let report = correlate(&[file]);
        assert_eq!(report.exclusive, 4096);
        assert_eq!(report.hardlink_outside, 0);
    }

    #[test]
    fn mixed_tier_selection_buckets_independently() {
        let ext4 = input(1, 1, 1000, FsTier::HardlinkOnly);
        let zfs = input(2, 1, 2000, FsTier::Zfs);
        let unmapped = input(3, 1, 3000, FsTier::Extent);
        let mut mapped = input(3, 2, 4000, FsTier::Extent);
        mapped.map = Some(extent_map(3, 2, 4000, &[]));
        let mut pinned = input(4, 1, 5000, FsTier::HardlinkOnly);
        pinned.links = LinkStatus::LinksOutside;

        let report = correlate(&[ext4, zfs, unmapped, mapped, pinned]);
        assert_eq!(report.exclusive, 1000 + 4000);
        assert_eq!(report.unknown, 3000);
        assert_eq!(report.shared_outside, 5000);
        assert_eq!(report.zfs_bytes, 2000);
        assert_eq!(report.zfs_files, 1);
        assert_eq!(report.hardlink_outside, 1);
        assert_eq!(report.inodes, 5);
        assert_eq!(report.mapped, 1);
    }

    // --- tier magics -------------------------------------------------------

    #[test]
    fn tier_magics_classify_per_d5() {
        assert_eq!(tier_of_magic(BTRFS_MAGIC), FsTier::Extent);
        assert_eq!(tier_of_magic(XFS_MAGIC), FsTier::Extent);
        assert_eq!(tier_of_magic(ZFS_MAGIC), FsTier::Zfs);
        assert_eq!(tier_of_magic(0xEF53), FsTier::HardlinkOnly, "ext4");
        assert_eq!(tier_of_magic(0x0), FsTier::HardlinkOnly);
    }

    // --- kernel gate -------------------------------------------------------

    #[test]
    fn kernel_gate_6_1_boundary() {
        assert!(release_at_least_6_1("6.1.0-13-amd64"));
        assert!(release_at_least_6_1("6.1"));
        assert!(release_at_least_6_1("6.12.4-arch1-1"));
        assert!(release_at_least_6_1("7.1.4-1-cachyos"));
        assert!(release_at_least_6_1("10.0.0"));
        assert!(!release_at_least_6_1("6.0.19"));
        assert!(!release_at_least_6_1("5.15.0-generic"));
        assert!(!release_at_least_6_1("5.10.226"));
    }

    #[test]
    fn kernel_gate_is_conservative_on_garbage() {
        assert!(!release_at_least_6_1(""));
        assert!(!release_at_least_6_1("garbage"));
        assert!(!release_at_least_6_1("v6.1"), "leading non-digit");
        assert!(!release_at_least_6_1("6"), "bare 6 could be 6.0");
        assert!(release_at_least_6_1("7"), "bare 7 is unambiguous");
        assert!(!release_at_least_6_1(".1"));
    }
}
