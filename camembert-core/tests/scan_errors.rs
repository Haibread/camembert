//! Integration coverage for the scan engine's error paths against a real
//! filesystem: the errno of an unreadable directory surviving end to end
//! (worker → owner → tree side-table), and graceful degradation under file
//! descriptor exhaustion (the HANDOFF "known limitation": RLIMIT_NOFILE
//! pressure must record errors, never hang or panic).

use std::env;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use camembert_core::errno::ScanErrno;
use camembert_core::scan::{ScanOptions, ScanOutcome, Scanner, StatxBackend, StatxEngine};

fn child_by_name(outcome: &ScanOutcome, name: &[u8]) -> camembert_core::tree::NodeId {
    outcome
        .children_of(outcome.root())
        .find(|&id| outcome.name_of(id) == name)
        .unwrap_or_else(|| panic!("child {:?} not found", String::from_utf8_lossy(name)))
}

/// An unreadable directory records the *specific* errno (`EACCES`), not just
/// a generic error flag: the `openat` failure's `errno` must be preserved
/// through the worker's error batch, the owner's integration, and into the
/// tree's `error_reason` side-table — the plumbing the errno-taxonomy work
/// added, exercised here for the first time by a real scan. Both stat
/// engines share the directory-open path, so both must agree.
#[test]
fn unreadable_dir_preserves_eacces_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("locked")).unwrap();
    fs::write(root.join("locked/hidden"), b"secret").unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    // chmod 000 does not block root: skip where the dir stays readable.
    if fs::read_dir(root.join("locked")).is_ok() {
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipping: running as root, cannot make a directory unreadable");
        return;
    }

    for engine in [StatxEngine::Sync, StatxEngine::IoUring] {
        let outcome = Scanner::new(ScanOptions {
            threads: 1,
            statx_engine: engine,
            ..ScanOptions::default()
        })
        .scan(root)
        .unwrap();
        if engine == StatxEngine::IoUring && outcome.statx_backend != Some(StatxBackend::IoUring) {
            eprintln!("skipping io_uring leg: unavailable in this environment");
            continue;
        }

        assert_eq!(outcome.errors, 1, "engine {engine:?}: one unreadable dir");
        let locked = child_by_name(&outcome, b"locked");
        assert_eq!(
            outcome.tree().error_reason(locked),
            Some(ScanErrno::ACCESS),
            "engine {engine:?}: the openat EACCES must reach the tree side-table",
        );
    }

    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
}

// --- fd-exhaustion degradation (self-spawned child) ---------------------

const CHILD_ROOT_ENV: &str = "CAMEMBERT_FD_CHILD_ROOT";
const CHILD_RESULT_ENV: &str = "CAMEMBERT_FD_CHILD_RESULT";
const CHILD_LIMIT_ENV: &str = "CAMEMBERT_FD_CHILD_LIMIT";

/// A wide-and-deep tree: enough concurrently-open directories that eight
/// workers sharing a tiny fd budget must hit `EMFILE` mid-scan.
fn build_pressure_tree(root: &Path) {
    for a in 0..20 {
        let da = root.join(format!("d{a:02}"));
        fs::create_dir(&da).unwrap();
        for b in 0..20 {
            let db = da.join(format!("s{b:02}"));
            fs::create_dir(&db).unwrap();
            for f in 0..5 {
                fs::write(db.join(format!("f{f}")), b"x").unwrap();
            }
        }
    }
}

/// Under a lowered `RLIMIT_NOFILE`, a scan must **degrade**, not die: record
/// `EMFILE` errors and still terminate, without hanging (a worker that never
/// decrements the pending count would stall the owner forever) and without
/// panicking (an unwrap on a syscall error would abort the process).
///
/// `RLIMIT_NOFILE` is process-global, so the limit is lowered in a child
/// process that re-executes this test binary and runs the otherwise-ignored
/// `fd_exhaustion_child` below. The parent watches for a hang with a
/// deadline and reads the child's verdict from a result file (avoiding any
/// stdout pipe-buffer deadlock).
#[test]
fn fd_exhaustion_degrades_without_hanging() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    build_pressure_tree(root);
    let result_path = tmp.path().join("verdict");

    let exe = env::current_exe().expect("current test binary");
    let mut child = Command::new(&exe)
        .args(["--exact", "--ignored", "fd_exhaustion_child"])
        .env(CHILD_ROOT_ENV, root)
        .env(CHILD_RESULT_ENV, &result_path)
        .env(CHILD_LIMIT_ENV, "64")
        .spawn()
        .expect("spawn fd-exhaustion child");

    // Hang detection: the whole point of the test. A degraded scan finishes
    // in well under a second; 90 s is a generous ceiling before we call it a
    // hang, kill the child, and fail loudly.
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let Some(status) = status else {
        panic!(
            "fd-exhaustion scan HUNG (no exit within 90s) — this is a bug: \
             a worker likely failed without releasing its pending-jobs slot, \
             leaving the owner waiting forever",
        );
    };
    assert!(
        status.success(),
        "child exited non-zero ({status:?}) — the scan panicked under fd \
         pressure instead of recording errors and completing",
    );

    let verdict = fs::read_to_string(&result_path)
        .expect("child must record a verdict file when it degrades gracefully");
    assert!(
        verdict.starts_with("GRACEFUL"),
        "unexpected child verdict: {verdict:?}",
    );
    // When the scan completed (rather than failing to even open the root
    // under the pressure), it must have recorded the EMFILE degradation.
    if let Some(rest) = verdict.strip_prefix("GRACEFUL ok errors=") {
        let errors: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!(
            errors > 0,
            "fd pressure did not bite (0 errors) — verdict {verdict:?}",
        );
    }
}

/// The child half of `fd_exhaustion_degrades_without_hanging`. Ignored so it
/// never runs on its own; inert unless the parent set the env below. It
/// lowers `RLIMIT_NOFILE`, then starves itself down to a handful of free
/// descriptors so the scan is guaranteed to meet `EMFILE`, and records
/// whether the scan degraded gracefully.
#[test]
#[ignore = "spawned as a subprocess by fd_exhaustion_degrades_without_hanging"]
fn fd_exhaustion_child() {
    let Ok(root) = env::var(CHILD_ROOT_ENV) else {
        return; // inert under a normal `cargo test --include-ignored`
    };
    let result_path = env::var(CHILD_RESULT_ENV).expect("result path env");
    let limit: u64 = env::var(CHILD_LIMIT_ENV)
        .expect("limit env")
        .parse()
        .expect("limit is a number");

    // Lower the soft NOFILE ceiling (no privilege needed; it is enforced for
    // every uid, so this bites even when the suite runs as root).
    let current = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    rustix::process::setrlimit(
        rustix::process::Resource::Nofile,
        rustix::process::Rlimit {
            current: Some(limit),
            maximum: current.maximum,
        },
    )
    .expect("lower RLIMIT_NOFILE");

    // Starve the descriptor table down to a small margin so mid-scan EMFILE
    // is deterministic regardless of how the scheduler walks the tree.
    let mut hogs: Vec<fs::File> = Vec::new();
    while let Ok(file) = fs::File::open("/dev/null") {
        hogs.push(file);
    }
    // Leave ~8 descriptors: enough to open the root and make progress, far
    // too few for eight workers to avoid EMFILE on a 400-directory tree.
    for _ in 0..8 {
        hogs.pop();
    }

    let verdict = match Scanner::new(ScanOptions {
        threads: 8,
        ..ScanOptions::default()
    })
    .scan(&root)
    {
        Ok(outcome) => format!(
            "GRACEFUL ok errors={} dirs={} entries={}",
            outcome.errors, outcome.dirs, outcome.entries
        ),
        // Failing to open the root under the pressure is still graceful
        // (a clean error, no hang, no panic).
        Err(err) => format!("GRACEFUL err {err}"),
    };

    drop(hogs); // free descriptors so the verdict file can be written
    fs::write(&result_path, verdict).expect("write verdict");
}

/// Non-UTF-8 directory names on the error path: an unreadable directory with
/// raw bytes in its name still records its errno against the right node
/// (the name round-trips through the error batch unchanged).
#[test]
fn unreadable_dir_with_non_utf8_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let raw_name = std::ffi::OsStr::from_bytes(b"lock\xffed");
    let locked = root.join(raw_name);
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("hidden"), b"x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipping: running as root, cannot make a directory unreadable");
        return;
    }

    let outcome = Scanner::new(ScanOptions {
        threads: 1,
        ..ScanOptions::default()
    })
    .scan(root)
    .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let node = child_by_name(&outcome, b"lock\xffed");
    assert_eq!(outcome.tree().error_reason(node), Some(ScanErrno::ACCESS));
}
