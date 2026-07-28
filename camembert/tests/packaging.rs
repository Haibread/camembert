//! The two generator binaries that produce packaging assets.
//!
//! These are not developer conveniences any more: the release workflow runs
//! both to fill the `.deb`/`.rpm` payloads, so a generator that silently
//! stops writing a file (a renamed shell, a `clap_complete` upgrade changing
//! an output name) ships a package missing its completions. The install
//! paths in `Cargo.toml`'s `[package.metadata.deb]` / `[…generate-rpm]`
//! name these exact files, and a package that references a missing asset
//! fails the build — these tests move that failure to `cargo test`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn run(bin: &str, arg: Option<&Path>) -> Output {
    let mut cmd = Command::new(bin);
    if let Some(arg) = arg {
        cmd.arg(arg);
    }
    cmd.output().expect("run the generator")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

/// `OUT_DIR` is created rather than required to exist, and every completion
/// script lands under the name its shell's loader looks for.
#[test]
fn completions_are_written_under_their_expected_names() {
    let tmp = TempDir::new().expect("temp dir");
    // A nested path the generator has to create itself.
    let out = tmp.path().join("does/not/exist/yet");

    let output = run(env!("CARGO_BIN_EXE_camembert-completions"), Some(&out));
    assert_eq!(
        code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for name in ["camembert.bash", "_camembert", "camembert.fish"] {
        let script = out.join(name);
        let body = fs::read_to_string(&script)
            .unwrap_or_else(|err| panic!("read {}: {err}", script.display()));
        assert!(
            body.contains("camembert"),
            "{name} does not mention the binary it completes"
        );
        assert!(!body.is_empty(), "{name} is empty");
    }
}

/// The completions have to cover the subcommands too — a packager shipping
/// only top-level flags would be a regression nobody notices by hand.
#[test]
fn completions_cover_the_subcommands() {
    let tmp = TempDir::new().expect("temp dir");
    let output = run(
        env!("CARGO_BIN_EXE_camembert-completions"),
        Some(tmp.path()),
    );
    assert_eq!(code(&output), 0);

    let bash = fs::read_to_string(tmp.path().join("camembert.bash")).expect("read bash script");
    for subcommand in ["diff", "import"] {
        assert!(
            bash.contains(subcommand),
            "the bash completions never mention the `{subcommand}` subcommand"
        );
    }
}

/// One man page for the binary, one per subcommand.
#[test]
fn man_pages_are_written_for_the_binary_and_each_subcommand() {
    let tmp = TempDir::new().expect("temp dir");
    let out = tmp.path().join("man/man1");

    let output = run(env!("CARGO_BIN_EXE_camembert-mangen"), Some(&out));
    assert_eq!(
        code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for name in ["camembert.1", "camembert-diff.1", "camembert-import.1"] {
        let page = out.join(name);
        let body = fs::read_to_string(&page)
            .unwrap_or_else(|err| panic!("read {}: {err}", page.display()));
        assert!(
            body.starts_with(".ie"),
            "{name} does not look like a roff man page"
        );
    }
}

/// Both generators refuse to guess an output directory.
#[test]
fn a_missing_out_dir_argument_is_a_usage_error() {
    for bin in [
        env!("CARGO_BIN_EXE_camembert-completions"),
        env!("CARGO_BIN_EXE_camembert-mangen"),
    ] {
        let output = run(bin, None);
        assert_eq!(code(&output), 2, "{bin} accepted a missing OUT_DIR");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage:"),
            "{bin} printed no usage line"
        );
    }
}

/// An `OUT_DIR` that cannot be created is reported, not ignored — the
/// release workflow needs a non-zero exit to stop before packaging.
#[test]
fn an_uncreatable_out_dir_exits_non_zero() {
    let tmp = TempDir::new().expect("temp dir");
    // A regular file cannot also be a directory, on any platform.
    let blocker = tmp.path().join("blocker");
    fs::write(&blocker, b"not a directory").expect("write the blocking file");
    let out = blocker.join("sub");

    for bin in [
        env!("CARGO_BIN_EXE_camembert-completions"),
        env!("CARGO_BIN_EXE_camembert-mangen"),
    ] {
        let output = run(bin, Some(&out));
        assert_eq!(code(&output), 2, "{bin} ignored an uncreatable OUT_DIR");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot create"),
            "{bin} did not say it could not create the directory"
        );
    }
}
