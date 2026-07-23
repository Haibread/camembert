//! Captures the git commit at build time for `--version` output.
//!
//! Exposes `CAMEMBERT_GIT_SHA` (read back via `env!` in `main.rs`) as the
//! short commit hash, suffixed `-dirty` when the worktree has uncommitted
//! changes, or `unknown` when `.git` or the `git` binary is unavailable
//! (crates.io / plain source-tarball builds).
//!
//! Only `.git/HEAD` and `.git/refs` are watched via `rerun-if-changed`, so a
//! rebuild refreshes the commit after a commit/checkout/merge without
//! re-running on every source-file edit. One consequence: the `-dirty`
//! suffix can go stale if you edit a tracked file, rebuild without touching
//! `.git`, and don't otherwise trigger a build-script rerun.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"));
    // This crate lives one level below the workspace root (`camembert/`
    // next to `camembert-core/`), which is where `.git` lives.
    let workspace_root = manifest_dir.parent().unwrap_or(&manifest_dir);
    let git_dir = workspace_root.join(".git");

    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());

    let sha = git_sha(workspace_root).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CAMEMBERT_GIT_SHA={sha}");
}

/// Returns the short commit hash, `-dirty`-suffixed if the worktree has
/// uncommitted changes, or `None` if git or the repository isn't available.
fn git_sha(dir: &Path) -> Option<String> {
    let short = run_git(dir, &["rev-parse", "--short", "HEAD"])?;
    let dirty = !run_git(dir, &["status", "--porcelain"])?.is_empty();
    Some(if dirty {
        format!("{short}-dirty")
    } else {
        short
    })
}

fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}
