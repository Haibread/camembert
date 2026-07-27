//! Integration tests for the scan engine, against a real temp tree.

use std::fs;
use std::path::Path;

use camembert_core::scan::{ScanOptions, Scanner};
use camembert_core::tree::{DirId, DirState, Kind, NodeId};

#[path = "support/mod.rs"]
mod support;

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
        ..ScanOptions::default()
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

/// Kernel pseudo-filesystems are never descended into, even when crossing
/// filesystem boundaries (their numbers are not disk usage). Gated on a
/// mounted kernfs being visible under /sys; skipped elsewhere (including
/// every non-Linux platform, which has no /sys at all).
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
        ..ScanOptions::default()
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
    if !support::make_unreadable(&locked) {
        support::restore_readable(&locked);
        eprintln!("skipping: cannot make a directory unreadable in this environment");
        return;
    }

    let scanner = Scanner::new(ScanOptions {
        threads: 2,
        cross_filesystems: false,
        ..ScanOptions::default()
    });
    let outcome = scanner.scan(root).unwrap();
    support::restore_readable(&locked);

    assert_eq!(outcome.errors, 1);
    let top = outcome.top_dirs_by_errors(10);
    // Exactly one error site: `locked` itself (the open failure is charged
    // to the unreadable dir), with a direct count of 1 — no ancestor noise.
    assert_eq!(top.len(), 1);
    let (dir, direct) = top[0];
    assert_eq!(direct, 1);
    assert!(outcome.path_of(dir).ends_with("outer/inner/locked"));
}

/// An unreadable directory scans as `DirState::Error`, is charged one `te`,
/// and its (unreachable) contents are never counted as children — the
/// counterpart of `error_report_uses_direct_counts` at the per-directory
/// metadata level rather than the top-level error report.
#[test]
fn unreadable_dir_is_state_error_with_uncounted_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let locked = root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("hidden"), vec![b'!'; 100]).unwrap();
    if !support::make_unreadable(&locked) {
        support::restore_readable(&locked);
        eprintln!("skipping: cannot make a directory unreadable in this environment");
        return;
    }

    let outcome = Scanner::new(ScanOptions::default()).scan(root).unwrap();
    support::restore_readable(&locked);

    assert_eq!(outcome.errors, 1);
    let locked_node = child_by_name(&outcome, outcome.root(), b"locked").unwrap();
    let locked_dir = outcome.tree().dir_of(locked_node).unwrap();
    assert_eq!(outcome.dir(locked_dir).state, DirState::Error);
    assert_eq!(outcome.dir(locked_dir).te, 1);
    assert_eq!(outcome.children_of(locked_dir).count(), 0);
}

/// A name that is not valid UTF-8 (a raw invalid byte on Unix, an unpaired
/// UTF-16 surrogate on Windows — see `support::non_utf8_name`) survives a
/// scan unchanged: the interner stores the platform's own `OsStr` encoding
/// verbatim, never assuming it decodes as `str`.
#[test]
fn non_utf8_name_is_preserved_by_a_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let raw_name = support::non_utf8_name();
    fs::write(root.join(&raw_name), b"1").unwrap();

    let outcome = Scanner::new(ScanOptions::default()).scan(root).unwrap();
    let node = child_by_name(&outcome, outcome.root(), raw_name.as_encoded_bytes());
    assert!(node.is_some(), "non-UTF-8 name must be preserved");
}

/// A directory's size must not depend on whether it is the scan root.
///
/// Portable, and it means something on both platforms — but it was written
/// for Windows, where it failed. A directory listing there reports
/// `AllocationSize = EndOfFile = 0` for every subdirectory entry, so a
/// directory scanned as a *child* contributed nothing while the same
/// directory scanned as a *root* — opened by handle, and asked with
/// `FileStandardInfo` — reported its real index allocation. On the fixture
/// below that was 0 B against ~192 KiB, i.e. `camembert sub` and
/// `camembert parent` disagreed about `sub` by two orders of magnitude, and
/// counting directory inodes is precisely what the README claims separates
/// camembert from `du -sb`.
///
/// The names are long and there are many of them on purpose: NTFS keeps a
/// small directory index resident inside the MFT record and allocates INDX
/// blocks only once it outgrows that, so a handful of short names would make
/// both answers 0 and the test would pass while measuring nothing. 400
/// entries of ~38 characters allocate 48 blocks of 4 KiB — verified
/// independently by reading the directory's own `:$I30:$INDEX_ALLOCATION`
/// stream, which is a different object reached through a different call.
#[test]
fn directory_size_does_not_depend_on_being_the_scan_root() {
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("sub");
    fs::create_dir(&sub).unwrap();
    for i in 0..400 {
        fs::write(
            sub.join(format!("entry-with-a-fairly-long-name-{i:04}.txt")),
            b"x",
        )
        .unwrap();
    }

    let as_root = Scanner::new(ScanOptions::default()).scan(&sub).unwrap();
    let root_own = as_root.node(as_root.dir(as_root.root()).node).size();

    let as_child = Scanner::new(ScanOptions::default())
        .scan(tmp.path())
        .unwrap();
    let sub_node = child_by_name(&as_child, as_child.root(), b"sub").expect("sub is a child");
    let child_own = as_child.node(sub_node).size();

    assert_eq!(
        child_own, root_own,
        "the same directory reports different sizes as a child and as a root"
    );
    assert_ne!(
        root_own.real, 0,
        "the fixture is meant to force a non-resident directory index; a zero here \
         means the test can no longer tell the two answers apart"
    );

    // And the subtree the parent attributes to it is the subtree it claims
    // for itself — the same property one level up, where the correction has
    // to have reached every ancestor's aggregate and not just the node.
    let sub_dir = as_child.tree().dir_of(sub_node).expect("sub was scanned");
    assert_eq!(
        (as_child.dir(sub_dir).ta, as_child.dir(sub_dir).td),
        (
            as_root.dir(as_root.root()).ta,
            as_root.dir(as_root.root()).td
        ),
        "subtree aggregates disagree between the two scans"
    );
}

/// A symlink is stored as its own node — `Kind::Symlink`, never descended
/// into as a directory. Skipped where the platform cannot create a symlink
/// at all (Windows without Developer Mode or an elevated process).
///
/// The *size* a symlink reports is deliberately not asserted here: on Unix
/// it is the byte length of the link's target text, but the Windows backend
/// reports a reparse point's own `EndOfFile`/`AllocationSize` (see
/// `scan/windows/worker.rs::entry_size`), which is not the same quantity —
/// asserting a specific cross-platform value would be guessing without a
/// Windows box to confirm it against.
#[test]
fn symlink_is_stored_without_following() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("target.txt"), b"hello").unwrap();
    if !support::symlink_file("target.txt", root.join("link")) {
        eprintln!(
            "skipping: cannot create a symlink in this environment \
             (needs Developer Mode or an elevated process on Windows)"
        );
        return;
    }

    let outcome = Scanner::new(ScanOptions::default()).scan(root).unwrap();
    let link = child_by_name(&outcome, outcome.root(), b"link").unwrap();
    assert_eq!(outcome.node(link).kind(), Kind::Symlink);
    assert!(
        outcome.tree().dir_of(link).is_none(),
        "symlink to a file must not become a directory"
    );
}

/// Hardlink identity accounting (`scan_a_known_tree`) is Unix-only: it
/// cross-checks the engine against an independent walk that dedups by
/// `(dev, ino)` and `nlink`, none of which `std::os::windows::fs::MetadataExt`
/// exposes on stable Rust (HANDOFF "Windows port" known gap 5 — the APIs
/// that would serve as a cross-check, `file_index`/`number_of_links`, are
/// nightly-only). The chmod-000 unreadable directory in the same fixture
/// could be ported (see `unreadable_dir_is_state_error_with_uncounted_contents`
/// above, which already does), but it is entangled with the hardlink
/// verification in one shared scan outcome here, so the whole test stays
/// gated rather than being pulled apart for a partial win — the two new
/// tests above and `non_utf8_name_is_preserved_by_a_scan` /
/// `symlink_is_stored_without_following` already cover the rest of this
/// fixture's ground portably.
/// Hardlink deduplication, against a fixture whose ground truth is known
/// by construction: `n` files of `FILE_BYTES` each, plus one hard link to
/// every one of them. The naive sum is `2 * n * FILE_BYTES`; the right
/// answer is `n * FILE_BYTES`, and being wrong here is being wrong by a
/// factor of two.
///
/// Portable on purpose. It is the guard on the property camembert is the
/// **only** Windows disk-usage tool to have (gdu, dust, diskus, robocopy
/// and `Get-ChildItem` all report the fixture at 2x, two of them while
/// documenting the opposite), and the Windows scan reaches it by a
/// completely different mechanism from the Unix one — `nlink > 1` gating a
/// registry there, a repeat sighting of the listing's file id here
/// (`docs/design/windows-nlink-dossier.md`). One test, both mechanisms.
#[test]
fn hardlinks_are_counted_once_not_twice() {
    const N: usize = 64;
    const FILE_BYTES: usize = 4096;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("unique")).unwrap();
    fs::create_dir(root.join("links")).unwrap();
    for i in 0..N {
        let target = root.join(format!("unique/f{i:03}.bin"));
        fs::write(&target, vec![b'x'; FILE_BYTES]).unwrap();
        if !support::hard_link(&target, root.join(format!("links/l{i:03}.bin"))) {
            eprintln!("skipping: this filesystem does not support hard links");
            return;
        }
    }

    let outcome = Scanner::new(ScanOptions::default()).scan(root).unwrap();

    // Walk the arena: what every link *says* it weighs, and what the
    // engine actually counted (extras contribute 0).
    let mut naive = 0u64;
    let mut counted = 0u64;
    let mut files = 0u64;
    walk_arena(&outcome, outcome.root(), &mut |outcome, id| {
        let node = outcome.node(id);
        if node.kind() == Kind::File {
            files += 1;
            naive += node.size().apparent;
        }
        if !node
            .flags()
            .contains(camembert_core::tree::NodeFlags::HARDLINK_EXTRA)
        {
            counted += node.size().apparent;
        }
    });

    // `walk_arena` deliberately skips the root's own node, and the root
    // directory inode *is* part of `totals.apparent` — that is the whole
    // reason camembert and `du -sb` disagree (README, "Honest numbers").
    // Add it back, exactly as `dir_bytes` does. A directory is never a
    // hardlink extra, so this is unconditional. Windows hid the omission:
    // a scan root's apparent size reads 0 there, so the two sides matched
    // at zero while Linux reported a real 4 KiB directory.
    counted += outcome
        .node(outcome.dir(outcome.root()).node)
        .size()
        .apparent;

    let payload = (N * FILE_BYTES) as u64;
    assert_eq!(files, 2 * N as u64, "every link is still its own entry");
    assert_eq!(
        naive,
        2 * payload,
        "the naive sum double-counts, as it must"
    );
    assert_eq!(
        outcome.totals.apparent, counted,
        "the root total is the sum of what was actually counted"
    );
    assert_eq!(
        outcome.totals.apparent - dir_bytes(&outcome),
        payload,
        "the inode's bytes are counted once, not once per link"
    );
    assert_ne!(
        outcome.totals.apparent - dir_bytes(&outcome),
        2 * payload,
        "the naive sum must not be what came out"
    );

    // Counters: N inodes reached by more than one path, N later links
    // contributing zero, and `entries` (root's `tn`) counting each inode
    // once — 1 root + 2 dirs + N files.
    assert_eq!(outcome.hardlink_inodes, N as u64);
    assert_eq!(outcome.hardlink_extra_links, N as u64);
    assert_eq!(outcome.entries, 3 + N as u64);
}

/// Σ apparent of the directory inodes themselves, which the fixture's
/// arithmetic has to set aside (their size is a filesystem's business:
/// 4096-ish on ext4, 0 for a subdirectory in a Windows listing).
fn dir_bytes(outcome: &camembert_core::scan::ScanOutcome) -> u64 {
    let mut total = 0;
    walk_arena(outcome, outcome.root(), &mut |outcome, id| {
        if outcome.node(id).kind() == Kind::Dir {
            total += outcome.node(id).size().apparent;
        }
    });
    total
        + outcome
            .node(outcome.dir(outcome.root()).node)
            .size()
            .apparent
}

/// Visit every node below `dir` (the root's own node excluded — callers
/// that want it add it, as [`dir_bytes`] does).
fn walk_arena(
    outcome: &camembert_core::scan::ScanOutcome,
    dir: DirId,
    visit: &mut impl FnMut(&camembert_core::scan::ScanOutcome, NodeId),
) {
    for child in outcome.children_of(dir).collect::<Vec<_>>() {
        visit(outcome, child);
        if let Some(sub) = outcome.tree().dir_of(child) {
            walk_arena(outcome, sub, visit);
        }
    }
}

#[cfg(unix)]
mod hardlink_identity {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use camembert_core::tree::NodeFlags;

    use super::*;

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
        // 11 nodes: root, a, f1, f2, sub, b, big, link, hard1, hard2, locked
        // — plus `locked/hidden` when the suite runs as root, since chmod 000
        // does not stop root from descending (a containerized CI runs as root
        // by default; this assertion used to fail there).
        let expected_nodes = if runs_as_root { 12 } else { 11 };
        assert_eq!(outcome.tree().node_count(), expected_nodes);
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
}
