//! End-to-end coverage of the `camembert-mangen` binary
//! ([`src/bin/mangen.rs`](../src/bin/mangen.rs)): the packaging-time man
//! page generator (see the README's Development section, and
//! `src/cli.rs`'s module doc comment for why the `Cli` definitions live
//! in their own file). Run through the real built binary, same style as
//! `tests/cli.rs`.

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_camembert-mangen"))
        .args(args)
        .output()
        .expect("run camembert-mangen")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

#[test]
fn writes_camembert_1_with_the_binary_name_and_a_known_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("man");

    let output = run(&[out_dir.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "{output:?}");

    let page = out_dir.join("camembert.1");
    assert!(page.exists(), "camembert.1 written to OUT_DIR");
    let text = std::fs::read_to_string(&page).expect("read camembert.1");
    assert!(!text.is_empty(), "roff output is non-empty");
    assert!(text.contains("camembert"), "names the binary: {text}");
    assert!(
        text.contains("one\\-filesystem"),
        "documents --one-filesystem: {text}"
    );
}

#[test]
fn also_writes_a_page_per_subcommand() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("man");

    let output = run(&[out_dir.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "{output:?}");

    for name in ["camembert-diff.1", "camembert-import.1"] {
        assert!(out_dir.join(name).exists(), "{name} written to OUT_DIR");
    }
}

#[test]
fn creates_out_dir_when_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("nested").join("man");
    assert!(!out_dir.exists());

    let output = run(&[out_dir.to_str().unwrap()]);
    assert_eq!(code(&output), 0, "{output:?}");
    assert!(out_dir.join("camembert.1").exists());
}

#[test]
fn missing_out_dir_argument_exits_nonzero_with_a_usage_message() {
    let output = run(&[]);
    assert_ne!(code(&output), 0);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("camembert-mangen:"),
        "clear error on stderr: {output:?}"
    );
}

#[test]
fn out_dir_that_cannot_be_created_exits_nonzero_with_a_message_on_stderr() {
    // A file where a directory is expected: create_dir_all must fail.
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker: &Path = dir.path();
    let out_dir = blocker.join("blocked");
    std::fs::write(&out_dir, b"not a directory").unwrap();

    let output = run(&[out_dir.to_str().unwrap()]);
    assert_ne!(code(&output), 0);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("camembert-mangen:"),
        "clear error on stderr: {output:?}"
    );
}
