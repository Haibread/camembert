//! Generates the `camembert(1)` man page (and one per subcommand) from the
//! same `clap` definitions the real binary parses with.
//!
//! ```text
//! cargo run --release --package camembert --bin camembert-mangen -- <OUT_DIR>
//! ```
//!
//! writes `<OUT_DIR>/camembert.1`, plus `<OUT_DIR>/camembert-diff.1` and
//! `<OUT_DIR>/camembert-import.1` for the subcommands, creating `OUT_DIR`
//! if it doesn't exist yet. Exits non-zero with a message on stderr if
//! any page can't be written (e.g. an unwritable `OUT_DIR`).
//!
//! `src/cli.rs` is pulled in with `#[path]` rather than through a `lib`
//! target: this package intentionally has none (`camembert` is a binary
//! crate), and a man-page generator is exactly the kind of build-time
//! tool that doesn't justify introducing one. See `cli.rs`'s module doc
//! comment for the one consequence that has on its `color`/`theme`
//! fields.

#[path = "../cli.rs"]
mod cli;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::CommandFactory as _;

fn main() -> ExitCode {
    let Some(out_dir) = env::args_os().nth(1) else {
        eprintln!("camembert-mangen: usage: camembert-mangen <OUT_DIR>");
        return ExitCode::from(2);
    };
    let out_dir = PathBuf::from(out_dir);

    if let Err(err) = std::fs::create_dir_all(&out_dir) {
        eprintln!(
            "camembert-mangen: cannot create {}: {err}",
            out_dir.display()
        );
        return ExitCode::from(2);
    }

    if let Err(err) = clap_mangen::generate_to(cli::Cli::command(), &out_dir) {
        eprintln!(
            "camembert-mangen: cannot write man pages to {}: {err}",
            out_dir.display()
        );
        return ExitCode::from(2);
    }

    ExitCode::SUCCESS
}
