//! Integration tests for the FIEMAP oracle against real reflink fixtures
//! (freeable-2 D6): files are created under `CARGO_TARGET_TMPDIR` — inside
//! `target/`, on the repo's real filesystem (`/tmp` is typically tmpfs,
//! which has no extents) — and every extent-dependent test guard-skips
//! unless that filesystem is extent-capable (btrfs/XFS), the same
//! runtime-probe style as `tests/scan.rs`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use camembert_core::fiemap::{
    self, FileMap, FsTier, LinkStatus, OracleInput, OracleReport, correlate, map_file,
};

/// Fixture root: inside `target/`, on the real filesystem under test.
/// Wiped on entry — `CARGO_TARGET_TMPDIR` persists across runs, and stale
/// fixtures (a surviving hardlink, an already-cloned file) would break
/// re-runs.
fn fixture_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fiemap-fixtures")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write `len` pseudo-random (incompressible) bytes and fsync, so FIEMAP
/// sees allocated extents, not delalloc — and compressed mounts store
/// them ~1:1, keeping `st_blocks` comparable to the logical size.
fn write_random(path: &Path, len: usize) {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        // xorshift64: deterministic, incompressible enough for btrfs.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(len);
    let mut file = fs::File::create(path).unwrap();
    file.write_all(&data).unwrap();
    file.sync_all().unwrap();
}

/// Reflink `src` into a fresh `dest` via FICLONE. `Err` means the
/// filesystem refused (e.g. XFS without `reflink=1`) — callers skip.
fn reflink(src: &Path, dest: &Path) -> std::io::Result<()> {
    let src_file = fs::File::open(src)?;
    let dest_file = fs::File::create(dest)?;
    rustix::fs::ioctl_ficlone(&dest_file, &src_file)?;
    dest_file.sync_all()?;
    Ok(())
}

/// Allocated bytes from a fresh stat (`st_blocks * 512`).
fn disk_of(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

/// Assemble one selected-file oracle input from live fixtures.
fn oracle_input(path: &Path, links: LinkStatus, map: Option<FileMap>) -> OracleInput {
    let meta = fs::metadata(path).unwrap();
    OracleInput {
        dev: meta.dev(),
        ino: meta.ino(),
        disk: meta.blocks() * 512,
        tier: FsTier::Extent,
        links,
        map,
    }
}

/// Filesystem-specific slack: block rounding, tail packing, bookends.
/// ±5% of the expected figure, floored at two 128 KiB compression
/// chunks.
fn assert_approx(actual: u64, expected: u64, what: &str) {
    let slack = (expected / 20).max(256 * 1024);
    assert!(
        actual >= expected.saturating_sub(slack) && actual <= expected + slack,
        "{what}: {actual} not within {slack} of {expected}"
    );
}

/// Guard: skip (returning true) when the fixture filesystem has no
/// extent machinery — the D6 runtime probe.
fn skip_unless_extent_fs(dir: &Path) -> bool {
    if !matches!(fiemap::tier_of(dir), FsTier::Extent) {
        eprintln!("skipping: {} is not on an extent-capable fs", dir.display());
        return true;
    }
    false
}

const FILE_LEN: usize = 4 << 20; // 4 MiB

#[test]
fn reflinked_pair_correlates_as_shared_within() {
    let dir = fixture_dir("reflink-pair");
    if skip_unless_extent_fs(&dir) {
        return;
    }
    let a = dir.join("a.bin");
    let b = dir.join("b.bin");
    write_random(&a, FILE_LEN);
    if let Err(err) = reflink(&a, &b) {
        eprintln!("skipping: FICLONE unsupported here ({err})");
        return;
    }

    // After the clone, A's extents carry FIEMAP_EXTENT_SHARED.
    let map_a = map_file(&a).unwrap();
    assert!(
        !map_a.shared.is_empty(),
        "cloned file reports no shared extents"
    );
    assert_approx(map_a.shared_logical, FILE_LEN as u64, "A shared_logical");

    // Both selected: the union is freed if the whole selection goes.
    let map_b = map_file(&b).unwrap();
    let both = correlate(&[
        oracle_input(&a, LinkStatus::Single, Some(map_a.clone())),
        oracle_input(&b, LinkStatus::Single, Some(map_b)),
    ]);
    assert_approx(both.shared_within, FILE_LEN as u64, "pair shared_within");
    assert!(
        both.exclusive <= 256 * 1024,
        "fully-cloned pair claims {} exclusive bytes",
        both.exclusive
    );
    assert_eq!(both.inodes, 2);
    assert_eq!(both.mapped, 2);

    // A alone: its partner is outside the selection.
    let alone = correlate(&[oracle_input(&a, LinkStatus::Single, Some(map_a))]);
    assert_approx(alone.shared_outside, FILE_LEN as u64, "solo shared_outside");
    assert_eq!(alone.shared_within, 0);
}

#[test]
fn non_cloned_file_is_exclusive() {
    let dir = fixture_dir("exclusive");
    if skip_unless_extent_fs(&dir) {
        return;
    }
    let c = dir.join("c.bin");
    write_random(&c, FILE_LEN);

    let map = map_file(&c).unwrap();
    let report = correlate(&[oracle_input(&c, LinkStatus::Single, Some(map))]);
    assert_approx(report.exclusive, FILE_LEN as u64, "exclusive");
    assert_eq!(report.shared_within, 0);
    // Unshared fresh file: nothing should sit in the outside bucket.
    assert_eq!(report.shared_outside, 0);
}

#[test]
fn hardlink_rule_beats_extent_bucketing() {
    let dir = fixture_dir("hardlink");
    if skip_unless_extent_fs(&dir) {
        return;
    }
    let h1 = dir.join("h1.bin");
    let h2 = dir.join("h2.bin");
    write_random(&h1, FILE_LEN);
    fs::hard_link(&h1, &h2).unwrap();

    // A link survives outside the selection: every byte is pinned, the
    // (perfectly good) extent map goes unused.
    let map = map_file(&h1).unwrap();
    let pinned = correlate(&[oracle_input(&h1, LinkStatus::LinksOutside, Some(map))]);
    assert_approx(pinned.shared_outside, disk_of(&h1), "pinned shared_outside");
    assert_eq!(pinned.exclusive, 0);
    assert_eq!(pinned.hardlink_outside, 1);
    assert_eq!(pinned.mapped, 0);

    // Every link selected (both names, one inode — the caller feeds the
    // oracle ONE input per (dev, ino)): normal extent bucketing.
    let map = map_file(&h1).unwrap();
    let freed = correlate(&[oracle_input(&h1, LinkStatus::AllLinksSelected, Some(map))]);
    assert_approx(freed.exclusive, FILE_LEN as u64, "all-links exclusive");
    assert_eq!(freed.hardlink_outside, 0);
    assert_eq!(freed.mapped, 1);
}

#[test]
fn map_file_rejects_non_regular_files() {
    // No extent-fs guard needed: both refusals fire before the ioctl.
    let dir = fixture_dir("non-regular");

    // A directory opens fine but fails the fstat regular-file check.
    let err = map_file(&dir).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // O_NOFOLLOW turns a symlink into ELOOP at open.
    let target = dir.join("target.bin");
    write_random(&target, 4096);
    let link = dir.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(map_file(&link).is_err(), "O_NOFOLLOW must refuse symlinks");
}

#[test]
fn oracle_report_is_plain_data() {
    // The oracle is pure: an empty selection is an all-zero report even
    // with no filesystem underneath (D6 isolation sanity check).
    assert_eq!(correlate(&[]), OracleReport::default());
}
