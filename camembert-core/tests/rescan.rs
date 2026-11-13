//! Rescan grafts over a real filesystem: a session scan, a change on disk,
//! a rescan of one directory grafted in — and the result must agree, per
//! directory, with a full scan of the same final state
//! (`docs/design/subtree-rescan-decisions.md` D1).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use camembert_core::scan::{GraftError, ScanOptions, ScanOutcome, Scanner};
use camembert_core::tree::DirId;

fn scanner() -> Scanner {
    Scanner::new(ScanOptions {
        threads: 2,
        ..ScanOptions::default()
    })
}

fn scan(path: &Path) -> ScanOutcome {
    let mut outcome = scanner().scan(path).expect("scan succeeds");
    outcome.finalize_hardlinks();
    outcome
}

fn write(path: &Path, bytes: usize) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, vec![b'x'; bytes]).expect("write test file");
}

fn find_dir(outcome: &ScanOutcome, components: &[&str]) -> DirId {
    let tree = outcome.tree();
    let mut dir = outcome.root();
    for component in components {
        let node = tree
            .children(dir)
            .find(|&id| tree.name(id) == component.as_bytes())
            .unwrap_or_else(|| panic!("component {component} not found"));
        dir = tree.dir_of(node).expect("component is a directory");
    }
    dir
}

/// Rescan `components` under the session root and graft it in, the way
/// the TUI does: seed first, then scan, then graft.
fn rescan(session: &mut ScanOutcome, components: &[&str]) -> camembert_core::scan::GraftReport {
    let dir = find_dir(session, components);
    let seed = session.rescan_link_seed(dir);
    let path = session.path_of(dir);
    let fresh = scanner()
        .with_link_seed(seed)
        .scan(&path)
        .expect("rescan succeeds");
    session.graft_rescan(dir, fresh).expect("graft succeeds")
}

/// Every live directory's subtree aggregates, keyed by path relative to
/// the scan root.
fn aggregates(outcome: &ScanOutcome) -> BTreeMap<PathBuf, (u64, u64, u64, u32)> {
    let tree = outcome.tree();
    let root = outcome.path_of(outcome.root());
    tree.dir_ids()
        .filter(|&dir| !tree.is_removed(tree.dir(dir).node))
        .map(|dir| {
            let meta = tree.dir(dir);
            let rel = outcome
                .path_of(dir)
                .strip_prefix(&root)
                .expect("under the root")
                .to_path_buf();
            (rel, (meta.ta, meta.td, meta.tn, meta.te))
        })
        .collect()
}

fn assert_matches_full_scan(session: &ScanOutcome, root: &Path) {
    let full = scan(root);
    assert_eq!(
        aggregates(session),
        aggregates(&full),
        "per-directory aggregates"
    );
    assert_eq!(session.totals, full.totals, "totals");
    assert_eq!(session.entries, full.entries, "entries");
    assert_eq!(session.dirs, full.dirs, "dirs");
    assert_eq!(session.errors, full.errors, "errors");
    assert_eq!(
        session.hardlink_inodes, full.hardlink_inodes,
        "hardlink inodes"
    );
    assert_eq!(
        session.hardlink_extra_links, full.hardlink_extra_links,
        "hardlink extra links"
    );
}

#[test]
fn a_grafted_subtree_matches_a_full_scan_of_the_same_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("a/x"), 100);
    write(&root.join("a/y/keep"), 7000);
    write(&root.join("b/z"), 3000);
    let mut session = scan(root);

    fs::remove_file(root.join("a/x")).expect("remove");
    write(&root.join("a/new"), 50_000);
    write(&root.join("a/y/deep/er/file"), 12_345);

    let report = rescan(&mut session, &["a"]);
    assert!(!report.replaced_whole);
    assert!(report.after.apparent > report.before.apparent);
    assert_matches_full_scan(&session, root);
}

#[test]
fn a_nested_rescan_moves_every_ancestor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("a/b/c/f"), 4096);
    write(&root.join("a/sibling"), 10);
    let mut session = scan(root);

    write(&root.join("a/b/c/g"), 80_000);
    fs::remove_file(root.join("a/b/c/f")).expect("remove");

    rescan(&mut session, &["a", "b", "c"]);
    assert_matches_full_scan(&session, root);

    // A second rescan of an already grafted directory grafts again.
    write(&root.join("a/b/c/h"), 1);
    rescan(&mut session, &["a", "b"]);
    assert_matches_full_scan(&session, root);
}

#[test]
fn a_hardlink_with_its_canonical_link_inside_counts_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("z_out/f"), 20_000);
    fs::create_dir_all(root.join("a_in")).expect("mkdir");
    fs::hard_link(root.join("z_out/f"), root.join("a_in/g")).expect("hard link");
    let mut session = scan(root);

    write(&root.join("a_in/other"), 10);
    rescan(&mut session, &["a_in"]);
    assert_matches_full_scan(&session, root);
}

#[test]
fn a_hardlink_with_its_canonical_link_outside_counts_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("a_out/f"), 20_000);
    fs::create_dir_all(root.join("z_in")).expect("mkdir");
    fs::hard_link(root.join("a_out/f"), root.join("z_in/g")).expect("hard link");
    let mut session = scan(root);

    write(&root.join("z_in/other"), 10);
    rescan(&mut session, &["z_in"]);
    assert_matches_full_scan(&session, root);
}

/// The seed is load-bearing where links are found by repeat sighting:
/// without it the inside link of a straddling inode looks like a plain
/// file and the graft counts the inode twice.
#[cfg(windows)]
#[test]
fn without_the_seed_a_straddling_link_is_counted_twice() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("a_out/f"), 20_000);
    fs::create_dir_all(root.join("z_in")).expect("mkdir");
    fs::hard_link(root.join("a_out/f"), root.join("z_in/g")).expect("hard link");
    let mut session = scan(root);
    let before = session.totals;

    let dir = find_dir(&session, &["z_in"]);
    assert!(!session.rescan_link_seed(dir).is_empty());
    let fresh = scanner().scan(root.join("z_in")).expect("rescan succeeds");
    session.graft_rescan(dir, fresh).expect("graft succeeds");
    assert!(session.totals.apparent > before.apparent);
}

#[test]
fn removing_the_counted_link_inside_promotes_the_one_outside() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("z_out/f"), 20_000);
    fs::create_dir_all(root.join("a_in")).expect("mkdir");
    fs::hard_link(root.join("z_out/f"), root.join("a_in/g")).expect("hard link");
    let mut session = scan(root);

    fs::remove_file(root.join("a_in/g")).expect("remove the inside link");
    let report = rescan(&mut session, &["a_in"]);
    assert_eq!(report.relinked_groups, 1, "the outside link takes over");
    assert_matches_full_scan(&session, root);
}

#[test]
fn rescanning_the_root_replaces_the_whole_outcome() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("a/x"), 100);
    let mut session = scan(root);

    write(&root.join("b/y"), 9000);
    let report = rescan(&mut session, &[]);
    assert!(report.replaced_whole);
    assert_matches_full_scan(&session, root);
}

#[test]
fn a_removed_directory_refuses_the_graft() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("a/x"), 100);
    let mut session = scan(root);
    let dir = find_dir(&session, &["a"]);
    let fresh = scanner().scan(root.join("a")).expect("rescan succeeds");
    let node = session.dir(dir).node;
    session.apply_removal(node).expect("remove");

    assert_eq!(
        session.graft_rescan(dir, fresh).unwrap_err(),
        GraftError::Removed
    );
}
