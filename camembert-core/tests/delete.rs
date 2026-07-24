//! End-to-end deletion tests over a real filesystem (tempfile): scan,
//! delete through the safe executor, and assert both the disk and the
//! tree's accounting (HANDOFF §5 "Garde-fous de suppression").

use std::fs;
use std::path::Path;

use camembert_core::delete::{self, DeleteTarget, EntryOutcome, InodeId, SkipReason};
use camembert_core::scan::{ScanOptions, ScanOutcome, Scanner};
use camembert_core::tree::{NodeFlags, NodeId};

/// The live `(dev, ino)` of a path, without following a final symlink —
/// what a caller records to anchor a [`DeleteTarget`]'s identity check.
fn identity_of(path: &Path) -> InodeId {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).expect("stat the target");
    InodeId {
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

fn scan(path: &Path) -> ScanOutcome {
    Scanner::new(ScanOptions {
        threads: 2,
        ..ScanOptions::default()
    })
    .scan(path)
    .expect("scan succeeds")
}

fn write(path: &Path, bytes: usize) {
    fs::write(path, vec![b'x'; bytes]).expect("write test file");
}

/// Resolve a node by path components under the scan root.
fn find_node(outcome: &ScanOutcome, components: &[&str]) -> NodeId {
    let tree = outcome.tree();
    let mut dir = outcome.root();
    let mut node = tree.dir(dir).node;
    for (i, component) in components.iter().enumerate() {
        node = tree
            .children(dir)
            .find(|&id| tree.name(id) == component.as_bytes())
            .unwrap_or_else(|| panic!("component {component} not found"));
        if i + 1 < components.len() {
            dir = tree.dir_of(node).expect("intermediate component is a dir");
        }
    }
    node
}

#[test]
fn deleting_files_and_subtrees_updates_disk_and_totals() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("keep.txt"), 4096);
    write(&root.join("junk.txt"), 8192);
    fs::create_dir_all(root.join("sub/deep")).unwrap();
    write(&root.join("sub/a.txt"), 4096);
    write(&root.join("sub/deep/b.txt"), 4096);

    let mut outcome = scan(root);
    assert_eq!(
        outcome.entries, 7,
        "root + keep + junk + sub + a + deep + b"
    );
    let totals_before = outcome.totals;
    let junk = find_node(&outcome, &["junk.txt"]);
    let sub = find_node(&outcome, &["sub"]);

    let report = delete::delete_nodes(&mut outcome, &[junk, sub]);
    assert_eq!((report.deleted, report.failed, report.skipped), (2, 0, 0));
    assert!(report.freed.real > 0);

    // Disk: gone (subtree included), the unmarked file untouched.
    assert!(!root.join("junk.txt").exists());
    assert!(!root.join("sub").exists());
    assert!(root.join("keep.txt").exists());

    // Accounting: totals shrank by exactly what the report claims, the
    // removed rows left children iteration, entries match what survives.
    assert_eq!(outcome.entries, 2, "root + keep");
    assert_eq!(outcome.totals.real, totals_before.real - report.freed.real);
    assert_eq!(
        outcome.totals.apparent,
        totals_before.apparent - report.freed.apparent
    );
    let tree = outcome.tree();
    let remaining: Vec<Vec<u8>> = tree
        .children(outcome.root())
        .map(|id| tree.name(id).to_vec())
        .collect();
    assert_eq!(remaining, [b"keep.txt".to_vec()]);
    assert!(tree.is_removed(junk));
    assert!(tree.is_removed(sub));
}

#[test]
fn marked_descendant_of_a_marked_dir_is_contained() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::create_dir(root.join("sub")).unwrap();
    write(&root.join("sub/file.txt"), 4096);

    let mut outcome = scan(root);
    let sub = find_node(&outcome, &["sub"]);
    let file = find_node(&outcome, &["sub", "file.txt"]);

    // Marked in any order: the executor sorts shallowest-first, so the
    // dir goes first and the file inside reports as contained.
    let report = delete::delete_nodes(&mut outcome, &[file, sub]);
    assert_eq!((report.deleted, report.failed, report.skipped), (2, 0, 0));
    assert!(matches!(report.results[1].outcome, EntryOutcome::Contained));
    assert!(!root.join("sub").exists());
    assert_eq!(outcome.entries, 1, "only the root remains");
}

#[test]
fn hardlink_pair_frees_only_with_the_last_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("a"), 8192);
    fs::hard_link(root.join("a"), root.join("b")).unwrap();

    let mut outcome = scan(root);
    assert_eq!(outcome.hardlink_inodes, 1);
    let a = find_node(&outcome, &["a"]);
    let b = find_node(&outcome, &["b"]);

    // Both links must trip the confirmation dialog's warning.
    assert_eq!(delete::hardlink_files_in(&outcome, &[a, b]), 2);
    assert_eq!(delete::hardlink_files_in(&outcome, &[a]), 1);
    // The warning also fires when the links hide inside a marked dir
    // (here: the root's node is not markable, so probe via each file).
    let root_node_probe = delete::hardlink_files_in(&outcome, &[b]);
    assert_eq!(root_node_probe, 1);

    let (extra, first) = if outcome
        .tree()
        .node(a)
        .flags()
        .contains(NodeFlags::HARDLINK_EXTRA)
    {
        (a, b)
    } else {
        (b, a)
    };

    // Deleting the uncounted extra link frees nothing (the inode
    // survives through the other link) and totals stay put.
    let totals_before = outcome.totals;
    let report = delete::delete_nodes(&mut outcome, &[extra]);
    assert_eq!(report.deleted, 1);
    assert_eq!(report.freed.real, 0, "extra link contributed 0");
    assert_eq!(outcome.totals, totals_before);

    // Deleting the last (counted) link frees the inode's space.
    let report = delete::delete_nodes(&mut outcome, &[first]);
    assert_eq!(report.deleted, 1);
    assert!(report.freed.real > 0, "last link frees the space");
    assert!(!root.join("a").exists());
    assert!(!root.join("b").exists());
    assert_eq!(outcome.entries, 1, "only the root remains");
}

#[test]
fn entries_changed_since_the_scan_are_skipped_or_failed_not_deleted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("replaced"), 4096);
    write(&root.join("vanished"), 4096);

    let mut outcome = scan(root);
    let replaced = find_node(&outcome, &["replaced"]);
    let vanished = find_node(&outcome, &["vanished"]);
    let totals_before = outcome.totals;

    // The filesystem moves on after the scan: one file becomes a
    // directory, the other disappears entirely.
    fs::remove_file(root.join("replaced")).unwrap();
    fs::create_dir(root.join("replaced")).unwrap();
    fs::remove_file(root.join("vanished")).unwrap();

    let report = delete::delete_nodes(&mut outcome, &[replaced, vanished]);
    assert_eq!((report.deleted, report.failed, report.skipped), (0, 1, 1));
    let replaced_result = report
        .results
        .iter()
        .find(|result| result.node == replaced)
        .unwrap();
    assert!(matches!(
        replaced_result.outcome,
        EntryOutcome::Skipped(SkipReason::KindChanged)
    ));
    let vanished_result = report
        .results
        .iter()
        .find(|result| result.node == vanished)
        .unwrap();
    assert!(matches!(&vanished_result.outcome, EntryOutcome::Failed(err)
        if err.kind() == std::io::ErrorKind::NotFound));

    // The type-changed entry was NOT deleted, and the tree was not
    // touched for either (nothing tombstoned, totals intact).
    assert!(root.join("replaced").is_dir());
    assert!(!outcome.tree().is_removed(replaced));
    assert!(!outcome.tree().is_removed(vanished));
    assert_eq!(outcome.totals, totals_before);
}

#[test]
fn the_scan_root_itself_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("f"), 4096);

    let mut outcome = scan(root);
    let root_node = outcome.tree().dir(outcome.root()).node;
    let report = delete::delete_nodes(&mut outcome, &[root_node]);
    assert_eq!((report.deleted, report.failed, report.skipped), (0, 0, 1));
    assert!(matches!(
        report.results[0].outcome,
        EntryOutcome::Skipped(SkipReason::OutsideRoot)
    ));
    assert!(
        root.join("f").exists(),
        "nothing under the root was touched"
    );
}

#[test]
fn an_impostor_swapped_in_since_the_mark_is_refused_by_identity() {
    // The defense the descriptor-relative walk adds over the old path-based
    // guard: a *real* directory of the same name renamed into the target's
    // place between the mark (identity recorded) and the delete passes the
    // type and same-device checks, yet must still be refused — its
    // `(dev, ino)` is not the one the caller recorded.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::create_dir(root.join("victim")).unwrap();
    write(&root.join("victim/data.txt"), 4096);

    let mut outcome = scan(root);
    let victim = find_node(&outcome, &["victim"]);
    // Recorded at mark time (here: right after the scan), before the swap.
    let recorded = identity_of(&root.join("victim"));
    let totals_before = outcome.totals;

    // The swap: the original directory is moved aside and a *fresh* one of
    // the same name takes its place (a new inode, same device, still a
    // directory — so only the identity check can catch it).
    fs::rename(root.join("victim"), root.join("moved-away")).unwrap();
    fs::create_dir(root.join("victim")).unwrap();
    write(&root.join("victim/impostor.txt"), 4096);
    assert_ne!(
        recorded,
        identity_of(&root.join("victim")),
        "the fresh directory must be a different inode"
    );

    let report = delete::delete_nodes_checked(
        &mut outcome,
        &[DeleteTarget {
            node: victim,
            expected: Some(recorded),
        }],
    );
    assert_eq!((report.deleted, report.failed, report.skipped), (0, 0, 1));
    assert!(matches!(
        report.results[0].outcome,
        EntryOutcome::Skipped(SkipReason::IdentityMismatch)
    ));

    // The impostor and its contents survive untouched, and the tree was not
    // tombstoned or debited for the refused entry.
    assert!(root.join("victim/impostor.txt").exists());
    assert!(!outcome.tree().is_removed(victim));
    assert_eq!(outcome.totals, totals_before);
}

#[test]
fn a_directory_target_replaced_by_a_symlink_is_refused_and_never_followed() {
    // The top-level target the scan recorded as a directory is a symlink by
    // delete time. The no-follow `fstatat` sees the link (not what it points
    // at), so the kind no longer matches and the entry is refused — and
    // crucially the link is never traversed, so its target is untouched.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::create_dir(root.join("target")).unwrap();
    write(&root.join("target/inside.txt"), 4096);
    fs::create_dir(root.join("outside")).unwrap();
    write(&root.join("outside/precious.txt"), 4096);

    let mut outcome = scan(root);
    let target = find_node(&outcome, &["target"]);
    let recorded = identity_of(&root.join("target"));

    // Swap the directory for a symlink pointing at the sibling tree.
    fs::remove_dir_all(root.join("target")).unwrap();
    std::os::unix::fs::symlink(root.join("outside"), root.join("target")).unwrap();

    let report = delete::delete_nodes_checked(
        &mut outcome,
        &[DeleteTarget {
            node: target,
            expected: Some(recorded),
        }],
    );
    assert_eq!((report.deleted, report.failed, report.skipped), (0, 0, 1));
    assert!(matches!(
        report.results[0].outcome,
        EntryOutcome::Skipped(SkipReason::KindChanged)
    ));

    // The symlink was not followed: the sibling tree it pointed at is intact.
    assert!(root.join("outside/precious.txt").exists());
    assert!(!outcome.tree().is_removed(target));
}

#[test]
fn symlinks_are_deleted_without_following() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write(&root.join("target.txt"), 4096);
    std::os::unix::fs::symlink(root.join("target.txt"), root.join("link")).unwrap();

    let mut outcome = scan(root);
    let link = find_node(&outcome, &["link"]);
    let report = delete::delete_nodes(&mut outcome, &[link]);
    assert_eq!((report.deleted, report.failed, report.skipped), (1, 0, 0));
    assert!(!root.join("link").symlink_metadata().is_ok());
    assert!(
        root.join("target.txt").exists(),
        "the link target must survive"
    );
}
