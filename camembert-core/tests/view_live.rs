//! Integration tests for the live-scan view path: snapshots while the
//! scan runs, nav requests over the bus, post-completion navigation on
//! the frozen arena, and cancellation.

use std::fs;
use std::time::{Duration, Instant};

use camembert_core::scan::{ScanOptions, Scanner};
use camembert_core::view::{self, RowState};

fn build_sample(root: &std::path::Path) {
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/f1"), vec![b'x'; 1000]).unwrap();
    fs::write(root.join("a/f2"), vec![b'y'; 10]).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    fs::write(root.join("b/big"), vec![b'z'; 100_000]).unwrap();
}

#[test]
fn live_scan_publishes_and_serves_nav_until_completion() {
    let tmp = tempfile::tempdir().unwrap();
    build_sample(tmp.path());

    let live = Scanner::new(ScanOptions::default()).scan_live(tmp.path());
    let bus = std::sync::Arc::clone(live.bus());

    // The bus is usable immediately: generation 0 placeholder at worst.
    let initial = bus.load();
    assert_eq!(initial.path, tmp.path());

    // Wait for the final root_complete snapshot (forced publish).
    let deadline = Instant::now() + Duration::from_secs(10);
    let final_snap = loop {
        let snap = bus.load();
        if snap.stats.root_complete {
            break snap;
        }
        assert!(Instant::now() < deadline, "no root_complete snapshot");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(final_snap.generation >= 1);
    assert_eq!(final_snap.rows.len(), 2, "a and b at the root");
    assert!(
        final_snap
            .rows
            .iter()
            .all(|r| r.state == RowState::Complete)
    );
    assert!(!final_snap.degraded);
    assert!(!final_snap.hardlinks_seen);

    // Post-completion: the owner thread exits, the outcome hands over the
    // frozen arena, and navigation reads it directly (the documented
    // post-scan mechanism).
    let outcome = live.join().unwrap();
    assert!(!outcome.cancelled);

    let a_row = final_snap
        .rows
        .iter()
        .find(|r| &*r.name == b"a")
        .expect("row for a");
    let a_dir = a_row.dir.expect("a is a scanned directory");
    let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
    assert!(stats.root_complete);
    let a_snap = view::build_snapshot(
        outcome.tree(),
        a_dir,
        final_snap.generation + 1,
        stats,
        outcome.hardlink_inodes > 0,
        false,
    );
    assert_eq!(a_snap.parent, Some(outcome.root()));
    assert_eq!(a_snap.rows.len(), 2);
    let mut names: Vec<&[u8]> = a_snap.rows.iter().map(|r| &*r.name).collect();
    names.sort();
    assert_eq!(names, [b"f1" as &[u8], b"f2"]);
    assert_eq!(a_snap.path, tmp.path().join("a"));
    assert_eq!(
        a_snap.totals.apparent,
        1000 + 10 + fs::symlink_metadata(tmp.path().join("a")).unwrap().len()
    );
}

#[test]
fn nav_request_over_the_bus_is_served() {
    // A tree big enough that the scan does not finish instantly, so the
    // in-flight nav request is exercised; correctness does not depend on
    // the timing, only liveness does.
    let tmp = tempfile::tempdir().unwrap();
    for d in 0..64 {
        let dir = tmp.path().join(format!("d{d:02}"));
        fs::create_dir(&dir).unwrap();
        for f in 0..64 {
            fs::write(dir.join(format!("f{f:02}")), b"x".repeat(f + 1)).unwrap();
        }
    }

    let live = Scanner::new(ScanOptions::default()).scan_live(tmp.path());
    let bus = std::sync::Arc::clone(live.bus());

    // Find any directory row, request it, and wait for its snapshot.
    let deadline = Instant::now() + Duration::from_secs(10);
    let target = loop {
        let snap = bus.load();
        if let Some(row) = snap.rows.iter().find_map(|r| r.dir) {
            break row;
        }
        assert!(Instant::now() < deadline, "no directory row ever published");
        std::thread::sleep(Duration::from_millis(1));
    };
    bus.request(target);
    loop {
        let snap = bus.load();
        if snap.dir == target {
            assert_eq!(snap.rows.len(), 64);
            break;
        }
        if live.is_finished() {
            // Scan finished before the request was served: the owner is
            // gone, which is the documented hand-over point. Serve it from
            // the outcome instead.
            let outcome = live.join().unwrap();
            let snap = view::build_snapshot(
                outcome.tree(),
                target,
                1,
                view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed),
                false,
                false,
            );
            assert_eq!(snap.rows.len(), 64);
            return;
        }
        assert!(Instant::now() < deadline, "nav request never served");
        std::thread::sleep(Duration::from_millis(1));
    }
    live.join().unwrap();
}

#[test]
fn cancellation_returns_a_partial_flagged_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    for d in 0..32 {
        let dir = tmp.path().join(format!("d{d:02}"));
        fs::create_dir(&dir).unwrap();
        for f in 0..32 {
            fs::write(dir.join(format!("f{f:02}")), b"data").unwrap();
        }
    }

    let live = Scanner::new(ScanOptions::default()).scan_live(tmp.path());
    live.cancel();
    let start = Instant::now();
    let outcome = live.join().unwrap();
    // Winds down promptly (bounded by the worker send-retry interval plus
    // scheduling noise; generous margin for CI).
    assert!(start.elapsed() < Duration::from_secs(5));
    // Racy by nature: the scan may legitimately have finished before the
    // cancel landed. Either way the outcome must be coherent.
    if outcome.cancelled {
        assert!(outcome.entries <= 1 + 32 + 32 * 32);
    } else {
        assert_eq!(outcome.entries, 1 + 32 + 32 * 32);
    }
}
