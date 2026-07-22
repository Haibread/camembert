//! Integration tests for the scan engine, against a real temp tree.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use camembert_core::scan::{ScanOptions, Scanner};
use camembert_core::tree::{DirId, DirState, Kind, NodeFlags, NodeId};

/// Independently compute (apparent_total, inode_count) with std::fs,
/// counting each `(dev, ino)` with nlink > 1 once and skipping unreadable
/// directories' contents — the same semantics the engine promises.
fn walk_expected(path: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> (u64, u64) {
    let meta = fs::symlink_metadata(path).unwrap();
    let mut apparent = meta.len();
    let mut inodes = 1;
    if meta.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return (apparent, inodes);
        };
        for entry in entries {
            let entry = entry.unwrap();
            let child_meta = fs::symlink_metadata(entry.path()).unwrap();
            if !child_meta.is_dir()
                && child_meta.nlink() > 1
                && !seen.insert((child_meta.dev(), child_meta.ino()))
            {
                continue; // later hardlink: counted once already
            }
            let (a, n) = walk_expected(&entry.path(), seen);
            apparent += a;
            inodes += n;
        }
    }
    (apparent, inodes)
}

fn child_by_name(
    outcome: &camembert_core::scan::ScanOutcome,
    dir: DirId,
    name: &[u8],
) -> Option<NodeId> {
    outcome
        .children_of(dir)
        .find(|&id| outcome.name_of(id) == name)
}

#[test]
fn scan_a_known_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // root/
    //   a/
    //     f1 (1000 B)
    //     f2 (10 B)
    //     sub/           (empty)
    //   b/
    //     big (100000 B)
    //     link -> ../a/f1
    //     hard1 (500 B), hard2 (hardlink to hard1)
    //   locked/          (chmod 000)
    //     hidden (100 B) (unreachable content)
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/f1"), vec![b'x'; 1000]).unwrap();
    fs::write(root.join("a/f2"), vec![b'y'; 10]).unwrap();
    fs::create_dir(root.join("a/sub")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    fs::write(root.join("b/big"), vec![b'z'; 100_000]).unwrap();
    std::os::unix::fs::symlink("../a/f1", root.join("b/link")).unwrap();
    fs::write(root.join("b/hard1"), vec![b'h'; 500]).unwrap();
    fs::hard_link(root.join("b/hard1"), root.join("b/hard2")).unwrap();
    fs::create_dir(root.join("locked")).unwrap();
    fs::write(root.join("locked/hidden"), vec![b'!'; 100]).unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    // Running as root, chmod 000 does not block reads: the unreadable-dir
    // assertions are skipped in that case.
    let runs_as_root = fs::read_dir(root.join("locked")).is_ok();

    let mut seen = std::collections::HashSet::new();
    let (expected_apparent, expected_inodes) = walk_expected(root, &mut seen);

    let scanner = Scanner::new(ScanOptions::default());
    let outcome = scanner.scan(root).unwrap();

    // Restore permissions so TempDir can clean up.
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    // Apparent totals exact, verified against an independent walk.
    assert_eq!(outcome.totals.apparent, expected_apparent);
    assert_eq!(outcome.entries, expected_inodes);
    // 11 nodes: root, a, f1, f2, sub, b, big, link, hard1, hard2, locked.
    assert_eq!(outcome.tree().node_count(), 11);
    // 5 directories carry metadata: root, a, sub, b, locked.
    assert_eq!(outcome.dirs, 5);

    // Hardlink pair: one inode, one extra link, counted once.
    assert_eq!(outcome.hardlink_inodes, 1);
    assert_eq!(outcome.hardlink_extra_links, 1);
    let b_node = child_by_name(&outcome, outcome.root(), b"b").unwrap();
    let b_dir = outcome.tree().dir_of(b_node).unwrap();
    let hard1 = child_by_name(&outcome, b_dir, b"hard1").unwrap();
    let hard2 = child_by_name(&outcome, b_dir, b"hard2").unwrap();
    let extra_flags = [hard1, hard2]
        .iter()
        .filter(|&&id| outcome.node(id).flags().contains(NodeFlags::HARDLINK_EXTRA))
        .count();
    assert_eq!(extra_flags, 1, "exactly one link flagged as extra");

    // Symlink: stored as a symlink with its own size, never followed.
    let link = child_by_name(&outcome, b_dir, b"link").unwrap();
    assert_eq!(outcome.node(link).kind(), Kind::Symlink);
    assert_eq!(outcome.node(link).size().apparent, "../a/f1".len() as u64);
    assert!(
        outcome.tree().dir_of(link).is_none(),
        "symlink to a file must not become a directory"
    );

    // Unreadable dir: state Error, counted in te, contents uncounted.
    let locked_node = child_by_name(&outcome, outcome.root(), b"locked").unwrap();
    if runs_as_root {
        eprintln!("running as root: skipping unreadable-dir assertions");
    } else {
        assert_eq!(outcome.errors, 1);
        let locked_dir = outcome.tree().dir_of(locked_node).unwrap();
        assert_eq!(outcome.dir(locked_dir).state, DirState::Error);
        assert_eq!(outcome.dir(locked_dir).te, 1);
        assert_eq!(outcome.children_of(locked_dir).count(), 0);
    }

    // Everything reachable is Complete.
    assert_eq!(outcome.dir(outcome.root()).state, DirState::Complete);

    // Directory totals: b's subtree = b itself + big + link + one hardlink.
    let b_meta = outcome.dir(b_dir);
    let b_own = fs::symlink_metadata(root.join("b")).unwrap().len();
    assert_eq!(b_meta.ta, b_own + 100_000 + "../a/f1".len() as u64 + 500);
    // b, big, link, and the hardlinked inode once: 4 (the extra link
    // contributes 0 to tn).
    assert_eq!(b_meta.tn, 4);

    // path_of reconstructs full paths.
    assert_eq!(outcome.path_of(b_dir), root.join("b"));

    // Non-UTF-8 names survive end to end (create after the fact scan? no —
    // separate mini-scan below).
    drop(outcome);
    let raw = tmp.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    fs::write(&raw, b"1").unwrap();
    let outcome = Scanner::new(ScanOptions::default()).scan(root).unwrap();
    let a_node = child_by_name(&outcome, outcome.root(), b"caf\xe9");
    assert!(a_node.is_some(), "non-UTF-8 name must be preserved");
}

#[test]
fn empty_directory_scans_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let outcome = Scanner::new(ScanOptions::default())
        .scan(tmp.path())
        .unwrap();
    assert_eq!(outcome.entries, 1); // the root itself
    assert_eq!(outcome.dirs, 1);
    assert_eq!(outcome.errors, 0);
    assert_eq!(outcome.dir(outcome.root()).state, DirState::Complete);
}

#[test]
fn scanning_a_file_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("f");
    fs::write(&file, b"x").unwrap();
    let err = Scanner::new(ScanOptions::default())
        .scan(&file)
        .unwrap_err();
    assert!(matches!(
        err,
        camembert_core::scan::ScanError::NotADirectory { .. }
    ));
}

#[test]
fn stress_scan_is_deterministic_across_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Wide: 40 dirs x 250 files = 10_000 files. Sizes vary per file.
    for d in 0..40 {
        let dir = root.join(format!("wide-{d:02}"));
        fs::create_dir(&dir).unwrap();
        for f in 0..250 {
            fs::write(dir.join(format!("f{f:03}")), vec![b'.'; (d * 7 + f) % 97]).unwrap();
        }
    }
    // Deep: a 30-level chain with one file per level.
    let mut deep = root.join("deep");
    for level in 0..30 {
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("leaf"), vec![b'd'; level * 3]).unwrap();
        deep = deep.join("next");
    }

    let scanner = Scanner::new(ScanOptions {
        threads: 8,
        cross_filesystems: false,
    });
    let mut reference: Option<(u64, u64, u64, u64, u64)> = None;
    for run in 0..4 {
        let outcome = scanner.scan(root).unwrap();
        let fingerprint = (
            outcome.totals.apparent,
            outcome.totals.real,
            outcome.entries,
            outcome.dirs,
            outcome.errors,
        );
        match &reference {
            None => reference = Some(fingerprint),
            Some(expected) => {
                assert_eq!(&fingerprint, expected, "run {run} diverged");
            }
        }
        // 1 root + 40 wide dirs + 10_000 files + 30 deep dirs + 30 leaves.
        assert_eq!(outcome.entries, 1 + 40 + 10_000 + 30 + 30);
        assert_eq!(outcome.dirs, 1 + 40 + 30);
        assert_eq!(outcome.errors, 0);
    }
}

/// Kernel pseudo-filesystems are never descended into, even with
/// `--cross-filesystems` (their numbers are not disk usage). Gated on a
/// mounted kernfs being visible under /sys; skipped elsewhere.
#[test]
fn kernfs_mounts_are_excluded_even_when_crossing() {
    // /sys/kernel/debug (debugfs) and /sys/kernel/tracing (tracefs) are
    // kernfs mount points inside sysfs on any normal Linux box.
    if !Path::new("/sys/kernel/debug").is_dir() {
        eprintln!("skipping: no /sys/kernel/debug on this system");
        return;
    }
    let scanner = Scanner::new(ScanOptions {
        threads: 4,
        cross_filesystems: true,
    });
    let outcome = scanner.scan(Path::new("/sys/kernel")).unwrap();
    assert!(
        outcome.excluded_kernfs >= 1,
        "expected at least one kernfs exclusion under /sys/kernel, got {}",
        outcome.excluded_kernfs
    );
    assert!(outcome.excluded_kernfs <= outcome.excluded_dirs);
}

/// The error report points at the directories where failures actually
/// happened (direct counts), not at their ancestors' subtree rollups.
#[test]
fn error_report_uses_direct_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("outer/inner/locked")).unwrap();
    fs::write(root.join("outer/file"), b"x").unwrap();
    let locked = root.join("outer/inner/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        eprintln!("skipping: running as root, cannot make a dir unreadable");
        return;
    }

    let scanner = Scanner::new(ScanOptions {
        threads: 2,
        cross_filesystems: false,
    });
    let outcome = scanner.scan(root).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(outcome.errors, 1);
    let top = outcome.top_dirs_by_errors(10);
    // Exactly one error site: `locked` itself (the open failure is charged
    // to the unreadable dir), with a direct count of 1 — no ancestor noise.
    assert_eq!(top.len(), 1);
    let (dir, direct) = top[0];
    assert_eq!(direct, 1);
    assert!(outcome.path_of(dir).ends_with("outer/inner/locked"));
}
