//! Behavior parity between the two stat engines (sync statx vs
//! io_uring-batched statx): identical trees, sizes, errors, hardlinks —
//! whichever engine a scan runs with.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use camembert_core::scan::{ScanOptions, ScanOutcome, Scanner, StatxBackend, StatxEngine};
use camembert_core::tree::DirId;

/// A fixture exercising every per-entry code path: regular files, empty
/// and nested directories, a symlink, a hardlink pair, an unreadable
/// directory (error taxonomy), a non-UTF-8 name, and a directory large
/// enough to cross both the io_uring burst size (1024) and the section
/// cap (4096).
fn build_fixture(root: &Path) {
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/f1"), vec![b'x'; 1000]).unwrap();
    fs::write(root.join("a/f2"), vec![b'y'; 10]).unwrap();
    fs::create_dir(root.join("a/sub")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    fs::write(root.join("b/big"), vec![b'z'; 100_000]).unwrap();
    std::os::unix::fs::symlink("../a/f1", root.join("b/link")).unwrap();
    fs::write(root.join("b/hard1"), vec![b'h'; 500]).unwrap();
    fs::hard_link(root.join("b/hard1"), root.join("b/hard2")).unwrap();
    fs::write(root.join(std::ffi::OsStr::from_bytes(b"caf\xe9")), b"1").unwrap();
    fs::create_dir(root.join("bulk")).unwrap();
    for i in 0..4500u32 {
        fs::write(
            root.join(format!("bulk/f{i:04}")),
            vec![b'.'; (i % 97) as usize],
        )
        .unwrap();
    }
    fs::create_dir(root.join("locked")).unwrap();
    fs::write(root.join("locked/hidden"), vec![b'!'; 100]).unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
}

fn unlock_fixture(root: &Path) {
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
}

fn scan_with(engine: StatxEngine, threads: usize, root: &Path) -> ScanOutcome {
    Scanner::new(ScanOptions {
        threads,
        statx_engine: engine,
        ..ScanOptions::default()
    })
    .scan(root)
    .expect("a scan never fails over engine choice")
}

/// Deterministic full-tree dump: every node (children sorted by name)
/// with kind, sizes, mtime, flags, and — for directories — state and
/// subtree aggregates. Two scans of the same tree by the same traversal
/// order must produce byte-identical fingerprints.
fn fingerprint(outcome: &ScanOutcome) -> String {
    let mut out = String::new();
    fn walk(outcome: &ScanOutcome, dir: DirId, prefix: &str, out: &mut String) {
        let meta = outcome.dir(dir);
        writeln!(
            out,
            "{prefix}/ state={:?} ta={} td={} tn={} te={}",
            meta.state, meta.ta, meta.td, meta.tn, meta.te
        )
        .unwrap();
        let mut children: Vec<_> = outcome.children_of(dir).collect();
        children.sort_by_key(|&id| outcome.name_of(id).to_vec());
        for id in children {
            let node = outcome.node(id);
            let name = String::from_utf8_lossy(outcome.name_of(id)).into_owned();
            writeln!(
                out,
                "{prefix}/{name} kind={:?} a={} d={} mtime={} flags={:?}",
                node.kind(),
                node.size().apparent,
                node.size().real,
                node.mtime(),
                node.flags(),
            )
            .unwrap();
            if let Some(child_dir) = outcome.tree().dir_of(id) {
                walk(outcome, child_dir, &format!("{prefix}/{name}"), out);
            }
        }
    }
    walk(outcome, outcome.root(), "", &mut out);
    out
}

fn counters(outcome: &ScanOutcome) -> (u64, u64, u64, u64, u64, u64, u64, u64) {
    (
        outcome.totals.apparent,
        outcome.totals.real,
        outcome.entries,
        outcome.dirs,
        outcome.errors,
        outcome.excluded_dirs,
        outcome.hardlink_inodes,
        outcome.hardlink_extra_links,
    )
}

/// Full-tree parity with a single worker: deterministic traversal order
/// on both engines, so the comparison covers everything — per-node
/// sizes/mtimes/flags (including which hardlink is flagged extra),
/// per-directory aggregates and error states, and the summary counters.
#[test]
fn io_uring_and_sync_engines_produce_identical_trees() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_fixture(root);

    let sync = scan_with(StatxEngine::Sync, 1, root);
    let uring = scan_with(StatxEngine::IoUring, 1, root);
    unlock_fixture(root);

    assert_eq!(sync.statx_backend, Some(StatxBackend::Sync));
    if uring.statx_backend != Some(StatxBackend::IoUring) {
        eprintln!("skipping: io_uring unavailable in this environment");
        return;
    }

    assert_eq!(counters(&sync), counters(&uring));
    let fp_sync = fingerprint(&sync);
    let fp_uring = fingerprint(&uring);
    assert_eq!(fp_sync, fp_uring, "engines diverged on the fixture tree");

    // Error taxonomy parity on the unreadable directory: both engines
    // must have recorded it identically (already covered by the
    // fingerprint, asserted explicitly for the error path).
    if fs::read_dir(root.join("locked")).is_err() || sync.errors == 1 {
        assert_eq!(sync.errors, uring.errors);
    }
}

/// Parallel scans cannot promise per-node determinism (first-seen
/// hardlink attribution depends on scheduling), but the aggregate
/// counters are order-independent — they must agree across engines.
#[test]
fn engines_agree_on_aggregates_under_parallel_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_fixture(root);

    let sync = scan_with(StatxEngine::Sync, 8, root);
    let uring = scan_with(StatxEngine::IoUring, 8, root);
    unlock_fixture(root);

    if uring.statx_backend != Some(StatxBackend::IoUring) {
        eprintln!("skipping: io_uring unavailable in this environment");
        return;
    }
    assert_eq!(counters(&sync), counters(&uring));
}

/// The probe fallback path: forcing the sync engine (the same path a
/// denied probe lands on) must report the sync backend and still scan
/// correctly; `auto` must always resolve to *some* backend and never
/// fail the scan.
#[test]
fn engine_selection_is_reported_and_never_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("f"), b"data").unwrap();

    let forced_sync = scan_with(StatxEngine::Sync, 2, root);
    assert_eq!(forced_sync.statx_backend, Some(StatxBackend::Sync));
    assert_eq!(forced_sync.entries, 2);

    let auto = scan_with(StatxEngine::Auto, 2, root);
    assert!(auto.statx_backend.is_some());
    assert_eq!(auto.entries, 2);

    // The auto heuristic: many workers → sync (io_uring batching is
    // slower once workers saturate the cores); ≤ 2 workers → io_uring
    // when the probe succeeds.
    let auto_wide = scan_with(StatxEngine::Auto, 8, root);
    assert_eq!(auto_wide.statx_backend, Some(StatxBackend::Sync));

    // Forcing io_uring must never fail either: on denial it falls back.
    let forced_uring = scan_with(StatxEngine::IoUring, 2, root);
    assert!(forced_uring.statx_backend.is_some());
    assert_eq!(forced_uring.entries, 2);
}
