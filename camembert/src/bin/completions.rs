//! Generates the shell completion scripts from the same `clap` definitions
//! the real binary parses with.
//!
//! ```text
//! cargo run --release --package camembert --bin camembert-completions -- <OUT_DIR>
//! ```
//!
//! writes `<OUT_DIR>/camembert.bash`, `<OUT_DIR>/_camembert` (zsh) and
//! `<OUT_DIR>/camembert.fish`, creating `OUT_DIR` if it doesn't exist yet.
//! Exits non-zero with a message on stderr if any script can't be written
//! (e.g. an unwritable `OUT_DIR`).
//!
//! Three shells and no more: these are the ones the `.deb`/`.rpm` packages
//! install into, and each extra shell is a file a packager then has to find
//! a home for. Anyone wanting elvish or PowerShell can add a `Shell` variant
//! to `SHELLS` — the rest of this generator is shell-agnostic.
//!
//! `src/cli.rs` is pulled in with `#[path]` for the same reason
//! `camembert-mangen` does it — see that binary's module doc comment.

#[path = "../cli.rs"]
mod cli;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::CommandFactory as _;
use clap_complete::Shell;

/// The shells a generated script is produced for, in the order they are
/// written. Names come from `clap_complete`, so the emitted file names match
/// what each shell's completion loader expects to find.
const SHELLS: [Shell; 3] = [Shell::Bash, Shell::Zsh, Shell::Fish];

fn main() -> ExitCode {
    let Some(out_dir) = env::args_os().nth(1) else {
        eprintln!("camembert-completions: usage: camembert-completions <OUT_DIR>");
        return ExitCode::from(2);
    };
    let out_dir = PathBuf::from(out_dir);

    if let Err(err) = std::fs::create_dir_all(&out_dir) {
        eprintln!(
            "camembert-completions: cannot create {}: {err}",
            out_dir.display()
        );
        return ExitCode::from(2);
    }

    let mut command = cli::Cli::command();
    for shell in SHELLS {
        if let Err(err) = clap_complete::generate_to(shell, &mut command, "camembert", &out_dir) {
            eprintln!(
                "camembert-completions: cannot write the {shell} completions to {}: {err}",
                out_dir.display()
            );
            return ExitCode::from(2);
        }
    }

    ExitCode::SUCCESS
}
