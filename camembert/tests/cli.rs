//! End-to-end CLI tests: scan → dump → mutate → dump → diff, plus the
//! ncdu import pipeline — through the real binary, real filesystem, real
//! exit codes.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::Value;

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_camembert"));
    // Deterministic environment: no ambient config bleeding in.
    for var in [
        "SCAN_PATH",
        "THREADS",
        "CROSS_FILESYSTEMS",
        "TOP",
        "NO_UI",
        "OUTPUT",
        "FILTER",
        "JSON_OUTPUT",
        "THRESHOLD",
        "LOG_FILTER",
        "LOG_FILE",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().expect("run camembert")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Write `len` bytes so the file's on-disk size is deterministic enough
/// for classification (exact block counts are filesystem-dependent, so
/// assertions compare classifications and signs, not raw byte counts).
fn write_file(path: &Path, len: usize) {
    fs::write(path, vec![0x61u8; len]).expect("write file");
}

fn set_mtime(path: &Path, secs_ago: u64) {
    let file = fs::File::options().write(true).open(path).expect("open");
    file.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .expect("set mtime");
}

/// Scan `tree` in summary mode and dump it to `out`.
fn scan_to_dump(tree: &Path, out: &Path) {
    let output = run(&[
        tree.to_str().unwrap(),
        "--no-ui",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 0, "scan failed: {output:?}");
    assert!(out.exists(), "dump written");
}

/// The full lifecycle fixture: scan, mutate (grow, shrink, delete, add,
/// touch, file→dir), scan again. Returns (old dump, new dump).
fn diff_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let tree = root.join("tree");
    fs::create_dir_all(tree.join("logs")).unwrap();
    write_file(&tree.join("logs/app.log"), 8 * 1024);
    write_file(&tree.join("logs/old.log"), 16 * 1024);
    write_file(&tree.join("config"), 4 * 1024);
    write_file(&tree.join("shrinker"), 64 * 1024);
    write_file(&tree.join("touched"), 4 * 1024);
    set_mtime(&tree.join("touched"), 3600);
    let old = root.join("old.cmbt");
    scan_to_dump(&tree, &old);

    write_file(&tree.join("logs/app.log"), 512 * 1024); // grown
    write_file(&tree.join("shrinker"), 4 * 1024); // shrunk
    fs::remove_file(tree.join("logs/old.log")).unwrap(); // removed
    write_file(&tree.join("logs/new.log"), 32 * 1024); // added
    set_mtime(&tree.join("touched"), 60); // touched (same size)
    fs::remove_file(tree.join("config")).unwrap(); // file -> dir
    fs::create_dir(tree.join("config")).unwrap();
    write_file(&tree.join("config/inner"), 4 * 1024);
    let new = root.join("new.cmbt");
    scan_to_dump(&tree, &new);
    (old, new)
}

#[test]
fn diff_classifies_and_orders_a_real_mutation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (old, new) = diff_fixture(dir.path());

    let output = run(&["diff", old.to_str().unwrap(), new.to_str().unwrap()]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    // Two added: new.log, plus the file inside the type-changed dir.
    assert!(text.contains("added 2,"), "added entries: {text}");
    assert!(text.contains("removed 1,"), "one removed entry: {text}");
    assert!(text.contains("grown 1,"), "{text}");
    assert!(text.contains("shrunk 1,"), "{text}");
    assert!(text.contains("touched 1,"), "{text}");
    assert!(text.contains("type-changed 1"), "config file->dir: {text}");
    assert!(text.contains("dirs: +1/-0"), "config dir added: {text}");
    assert!(text.contains("Top 20 directories by growth:"), "{text}");
    assert!(text.contains("Top 20 entries by growth:"), "{text}");

    // app.log grew by ~504 KiB — the largest entry delta, listed first.
    let entries_at = text.find("Top 20 entries by growth:").unwrap();
    let first_entry = text[entries_at..].lines().nth(1).expect("first entry row");
    assert!(
        first_entry.contains("grown") && first_entry.contains("app.log"),
        "biggest growth first: {first_entry}"
    );
    assert!(first_entry.trim_start().starts_with('+'), "{first_entry}");
}

#[test]
fn diff_json_schema_and_summary_deltas() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (old, new) = diff_fixture(dir.path());

    let output = run(&[
        "diff",
        old.to_str().unwrap(),
        new.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&output), 0);
    let lines: Vec<Value> = stdout(&output)
        .lines()
        .map(|l| serde_json::from_str(l).expect("JSON line"))
        .collect();

    let summary = &lines[0];
    assert_eq!(summary["t"], "summary");
    for key in [
        "oldRoot",
        "newRoot",
        "diskDelta",
        "apparentDelta",
        "entryDelta",
        "added",
        "removed",
        "grown",
        "shrunk",
        "touched",
        "typeChanged",
        "dirsAdded",
        "dirsRemoved",
    ] {
        assert!(summary.get(key).is_some(), "summary key {key}");
    }
    assert!(
        summary["diskDelta"].as_i64().unwrap() > 0,
        "the tree grew overall"
    );
    assert_eq!(summary["typeChanged"], 1);

    let dirs: Vec<&Value> = lines.iter().filter(|l| l["t"] == "dir").collect();
    let entries: Vec<&Value> = lines.iter().filter(|l| l["t"] == "entry").collect();
    assert!(!dirs.is_empty() && !entries.is_empty());
    for dir_line in &dirs {
        for key in ["path", "change", "diskDelta", "apparentDelta", "entryDelta"] {
            assert!(dir_line.get(key).is_some(), "dir key {key}");
        }
    }
    for entry in &entries {
        for key in ["path", "change", "diskDelta", "apparentDelta"] {
            assert!(entry.get(key).is_some(), "entry key {key}");
        }
    }
    let changes: Vec<&str> = entries
        .iter()
        .map(|e| e["change"].as_str().unwrap())
        .collect();
    for expected in [
        "added",
        "removed",
        "grown",
        "shrunk",
        "touched",
        "typeChanged",
    ] {
        assert!(changes.contains(&expected), "missing change {expected}");
    }

    // --top bounds both lists.
    let output = run(&[
        "diff",
        old.to_str().unwrap(),
        new.to_str().unwrap(),
        "--json",
        "--top",
        "2",
    ]);
    let bounded = stdout(&output);
    assert_eq!(
        bounded
            .lines()
            .filter(|l| l.contains("\"t\":\"entry\""))
            .count(),
        2
    );
}

#[test]
fn diff_threshold_drives_the_exit_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (old, new) = diff_fixture(dir.path());
    let (old, new) = (old.to_str().unwrap(), new.to_str().unwrap());

    // The tree grew by roughly 500 KiB: 1K trips, 1G does not.
    let output = run(&["diff", old, new, "--threshold", "1K"]);
    assert_eq!(code(&output), 1, "growth above threshold");
    let output = run(&["diff", old, new, "--threshold", "1G"]);
    assert_eq!(code(&output), 0, "growth below threshold");
    // Shrink direction never trips (growth, not churn).
    let output = run(&["diff", new, old, "--threshold", "1K"]);
    assert_eq!(code(&output), 0, "shrinkage is not growth");
    // Env form.
    let output = bin()
        .args(["diff", old, new])
        .env("THRESHOLD", "1K")
        .output()
        .expect("run");
    assert_eq!(code(&output), 1, "THRESHOLD env variant");
    // Errors exit 2.
    let output = run(&["diff", old, "/definitely/not/there.cmbt"]);
    assert_eq!(code(&output), 2);
    let garbage = dir.path().join("garbage.cmbt");
    fs::write(&garbage, b"not a dump at all").unwrap();
    let output = run(&["diff", old, garbage.to_str().unwrap()]);
    assert_eq!(code(&output), 2);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a camembert dump"),
        "clear error on stderr"
    );
}

#[test]
fn identical_scans_diff_to_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    write_file(&tree.join("sub/file"), 8 * 1024);
    let a = dir.path().join("a.cmbt");
    let b = dir.path().join("b.cmbt");
    scan_to_dump(&tree, &a);
    scan_to_dump(&tree, &b);

    let output = run(&["diff", a.to_str().unwrap(), b.to_str().unwrap(), "--json"]);
    assert_eq!(code(&output), 0);
    let summary: Value = serde_json::from_str(stdout(&output).lines().next().unwrap()).unwrap();
    assert_eq!(summary["diskDelta"], 0);
    assert_eq!(summary["added"], 0);
    assert_eq!(summary["removed"], 0);
}

const NCDU_FIXTURE: &str = r#"[1,2,{"progname":"ncdu","progver":"1.19","timestamp":1753000000},
  [{"name":"/data","dev":100,"asize":4096,"dsize":4096,"mtime":50},
   {"name":"a.log","asize":1000,"dsize":1024,"mtime":60},
   {"name":"link1","asize":500,"dsize":512,"ino":42,"nlink":2,"hlnkc":true},
   [{"name":"sub","asize":4096,"dsize":4096,"mtime":70},
    {"name":"link2","asize":500,"dsize":512,"ino":42,"nlink":2,"hlnkc":true}]
  ]]"#;

#[test]
fn import_produces_a_dump_that_diffs_to_zero_against_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = dir.path().join("export.json");
    fs::write(&json, NCDU_FIXTURE).unwrap();
    let out = dir.path().join("imported.cmbt");

    let output = run(&[
        "import",
        json.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 0, "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("Imported"), "{text}");
    assert!(text.contains("hardlinked inodes: 1"), "{text}");

    let output = run(&[
        "diff",
        out.to_str().unwrap(),
        out.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&output), 0);
    let summary: Value = serde_json::from_str(stdout(&output).lines().next().unwrap()).unwrap();
    assert_eq!(summary["diskDelta"], 0, "round-trip self-diff is zero");
    assert_eq!(summary["oldRoot"], "/data");
}

#[test]
fn import_reads_stdin_and_writes_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = bin()
        .args(["import", "-", "-o", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(NCDU_FIXTURE.as_bytes())
        .expect("feed stdin");
    let output = child.wait_with_output().expect("wait");
    assert_eq!(code(&output), 0);
    assert!(!output.stdout.is_empty(), "dump bytes on stdout");

    // The streamed dump is a valid dump: diff it against itself.
    let dump = dir.path().join("streamed.cmbt");
    fs::write(&dump, &output.stdout).unwrap();
    let output = run(&["diff", dump.to_str().unwrap(), dump.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "streamed dump is diffable: {output:?}");
}

#[test]
fn import_rejects_junk_with_exit_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let junk = dir.path().join("junk.json");
    fs::write(&junk, "{\"not\": \"an ncdu export\"}").unwrap();
    let out = dir.path().join("out.cmbt");
    let output = run(&[
        "import",
        junk.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 2);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("camembert import:"),
        "clear error on stderr"
    );
    assert!(!out.exists(), "no partial output left behind");
}

#[test]
fn scan_default_mode_is_untouched_by_the_subcommand_split() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree.join("f"), 4 * 1024);

    // Plain positional scan, no subcommand.
    let output = run(&[tree.to_str().unwrap(), "--no-ui", "--top", "3"]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    assert!(text.contains("Scanned"), "{text}");
    assert!(text.contains("Top 3 directories by real size:"), "{text}");
}

// ---- D5: flat-view top files in the --no-ui summary ----

#[test]
fn no_ui_summary_lists_top_files_after_the_top_directories_block() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree.join("big.bin"), 8192);
    write_file(&tree.join("small.bin"), 128);

    let output = run(&[tree.to_str().unwrap(), "--no-ui", "--top", "5"]);
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    let dirs_at = text
        .find("Top 5 directories by real size:")
        .expect("top-dirs block present");
    let files_at = text
        .find("Top 5 files by real size:")
        .expect("top-files block present (D5)");
    assert!(files_at > dirs_at, "files block comes after dirs: {text}");
    assert!(text.contains("big.bin"), "{text}");
}

#[test]
fn dump_to_stdout_suppresses_the_summary_including_the_top_files_block() {
    // The `-o -` gate (attack finding 7): stdout carries only the dump
    // stream, so neither the top-dirs nor the new top-files text may
    // appear on it — same gate, no new hole introduced by D5.
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree.join("f"), 4096);

    let output = run(&[tree.to_str().unwrap(), "--no-ui", "-o", "-"]);
    assert_eq!(code(&output), 0);
    // zstd frame magic number: stdout is exactly the dump stream, nothing
    // prepended (a stray summary line would land before this and break the
    // magic-number check on any real dump reader, not just this test).
    assert_eq!(
        &output.stdout[..4.min(output.stdout.len())],
        &[0x28, 0xB5, 0x2F, 0xFD][..],
        "stdout starts with the zstd magic number, not summary text"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("Top"), "no summary text of any kind: {text}");
    assert!(!text.contains("Scanned"), "{text}");
}

// ---- D7: --filter/FILTER in the --no-ui summary ----

#[test]
fn filter_summary_shows_matched_totals_and_only_the_matching_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree.join("keep.log"), 8192);
    write_file(&tree.join("skip.bin"), 8192);

    let output = run(&[
        tree.to_str().unwrap(),
        "--no-ui",
        "--top",
        "5",
        "--filter",
        "*.log",
    ]);
    assert_eq!(code(&output), 0, "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("matched (--filter"), "{text}");
    assert!(
        text.contains("Top 5 matched files by real size:"),
        "matched-files header present: {text}"
    );
    assert!(text.contains("keep.log"), "{text}");
    assert!(
        !text.contains("skip.bin"),
        "the non-matching file must never appear: {text}"
    );
}

#[test]
fn filter_with_a_parse_error_exits_2_and_never_scans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree.join("f"), 4096);

    // `;` is a reserved sigil (D1): guaranteed unparseable today.
    let output = run(&[tree.to_str().unwrap(), "--no-ui", "--filter", "a;b"]);
    assert_eq!(code(&output), 2, "{output:?}");
    let text = stdout(&output);
    assert!(
        !text.contains("Scanned"),
        "the scan must not have run: {text}"
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("--filter"), "{err}");
}

#[test]
fn filter_env_var_is_honored_like_the_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree.join("keep.log"), 4096);

    let output = bin()
        .args([tree.to_str().unwrap(), "--no-ui"])
        .env("FILTER", "*.log")
        .output()
        .expect("run camembert");
    assert_eq!(code(&output), 0, "{output:?}");
    assert!(stdout(&output).contains("matched (--filter"));
}

#[test]
fn filter_dump_to_stdout_is_never_filtered() {
    // -o - always dumps the whole, unfiltered scan (D7): the filter only
    // affects the summary text, which is suppressed anyway on this path.
    // Verified by diffing an empty-tree baseline against this dump and
    // checking *both* files (matching and non-matching) show up added —
    // a filtered dump would only ever show the matching one.
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree.join("keep.log"), 4096);
    write_file(&tree.join("skip.bin"), 4096);

    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let empty_dump = dir.path().join("empty.cmbt");
    scan_to_dump(&empty, &empty_dump);

    let output = run(&[
        tree.to_str().unwrap(),
        "--no-ui",
        "-o",
        "-",
        "--filter",
        "*.log",
    ]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert_eq!(
        &output.stdout[..4.min(output.stdout.len())],
        &[0x28, 0xB5, 0x2F, 0xFD][..],
        "stdout is still exactly the dump stream"
    );
    let dump_path = dir.path().join("filtered_scan.cmbt");
    fs::write(&dump_path, &output.stdout).unwrap();

    let diff_output = run(&[
        "diff",
        empty_dump.to_str().unwrap(),
        dump_path.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&diff_output), 0, "{diff_output:?}");
    let text = stdout(&diff_output);
    assert!(text.contains("keep.log"), "{text}");
    assert!(
        text.contains("skip.bin"),
        "the non-matching file is still in the dump, unfiltered: {text}"
    );
}
