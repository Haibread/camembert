mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use camembert_core::scan::{ScanOptions, Scanner};
use camembert_core::size::HumanSize;

/// Disk usage analyzer: what grew, what is freeable, what is cold.
#[derive(Debug, Parser)]
#[command(version, about, after_help = AFTER_HELP)]
struct Cli {
    /// Directory to scan (env: SCAN_PATH)
    #[arg(env = "SCAN_PATH", default_value = ".")]
    path: PathBuf,

    /// `tracing` filter directive (e.g. `info`, `camembert=debug`) (env: LOG_FILTER)
    ///
    /// Syntax: <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives>
    #[arg(long = "log-filter", env = "LOG_FILTER", default_value = "info")]
    log_filter: String,

    /// Scan worker threads; 0 = auto (2x CPU cores, capped at 8) (env: THREADS)
    #[arg(long, env = "THREADS", default_value_t = 0)]
    threads: usize,

    /// Cross filesystem boundaries instead of stopping at mount points (env: CROSS_FILESYSTEMS)
    #[arg(long, env = "CROSS_FILESYSTEMS")]
    cross_filesystems: bool,

    /// Number of directories in the "top directories by real size" list,
    /// summary mode only (env: TOP)
    #[arg(long, env = "TOP", default_value_t = 20)]
    top: usize,

    /// Disable the interactive UI: scan, then print the summary (env: NO_UI)
    ///
    /// This is also the automatic behavior when stdout is not a terminal
    /// (pipes, redirections).
    #[arg(long = "no-ui", env = "NO_UI")]
    no_ui: bool,

    /// Write diagnostics to this file instead of the default target
    /// (env: LOG_FILE)
    ///
    /// Default target: stderr in summary mode; discarded in interactive
    /// mode, where log output would corrupt the full-screen UI.
    #[arg(long = "log-file", env = "LOG_FILE")]
    log_file: Option<PathBuf>,
}

const AFTER_HELP: &str = "\
Modes:
  Interactive (default when stdout is a terminal): a full-screen browser
  over the scanned tree, navigable WHILE the scan runs — totals fill in
  and re-sort live. Quitting mid-scan cancels the scan. While hardlinks
  were seen and the scan is still running, the footer notes that totals
  are provisional (first-seen attribution, corrected at scan end).
  Diagnostics never touch the screen: they are discarded unless
  --log-file (env: LOG_FILE) points them at a file.

  Summary (--no-ui, env NO_UI, or stdout not a terminal): scan to
  completion, then print totals and the --top largest directories.

Keys (interactive mode):
  Down/j, Up/k     move the cursor
  Enter, l, Right  open the directory under the cursor
  Backspace, h, Left  go back up to the parent
  g / G            jump to the top / bottom
  d                sort by real (disk) size [default, descending]
  a                sort by apparent size
  n                sort by name (raw bytes, ascending)
  m                sort by modification time
  c                sort by item count
                   (pressing the active sort key reverses the direction)
  p                show/hide the apparent-size column
  q, Esc, Ctrl-C   quit (cancels the scan if still running)";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let interactive = !cli.no_ui && std::io::stdout().is_terminal();

    // In interactive mode the terminal belongs to ratatui: tracing output
    // must never reach it (a single log line prints at the raw-mode cursor,
    // right across the UI). Interactive diagnostics go to --log-file when
    // given, and are discarded otherwise; summary mode keeps stderr.
    let writer = match (&cli.log_file, interactive) {
        (Some(path), _) => {
            let file = match std::fs::File::create(path) {
                Ok(file) => file,
                Err(err) => {
                    eprintln!("camembert: cannot open log file {}: {err}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            let file = Arc::new(file);
            BoxMakeWriter::new(move || Arc::clone(&file))
        }
        (None, true) => BoxMakeWriter::new(std::io::sink),
        (None, false) => BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cli.log_filter))
        .with_writer(writer)
        .with_ansi(cli.log_file.is_none())
        .init();

    let scanner = Scanner::new(ScanOptions {
        threads: cli.threads,
        cross_filesystems: cli.cross_filesystems,
    });

    if interactive {
        return match ui::run(scanner, &cli.path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                // The terminal is restored by now; these are the process's
                // dying words, so they must reach the user even when logs
                // are discarded or filed away.
                error!(%err, "interactive UI failed");
                eprintln!("camembert: interactive UI failed: {err}");
                ExitCode::FAILURE
            }
        };
    }
    summary(&cli, &scanner)
}

/// Non-interactive mode: scan to completion, then print the summary on
/// stdout (diagnostics stay on stderr via tracing).
fn summary(cli: &Cli, scanner: &Scanner) -> ExitCode {
    // Progress line on stderr (via tracing) roughly every second while the
    // scan blocks this thread.
    let progress = scanner.progress();
    let done = Arc::new(AtomicBool::new(false));
    let poller = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                if done.load(Ordering::Acquire) {
                    break;
                }
                info!(
                    entries = progress.entries(),
                    dirs = progress.dirs(),
                    errors = progress.errors(),
                    disk = %HumanSize(progress.disk_bytes()),
                    "scanning"
                );
            }
        })
    };

    let outcome = scanner.scan(&cli.path);
    done.store(true, Ordering::Release);
    let _ = poller.join();

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            error!(%err, "scan failed");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "Scanned {} in {:.2}s",
        cli.path.display(),
        outcome.elapsed.as_secs_f64()
    );
    println!(
        "  total: {} real, {} apparent",
        HumanSize(outcome.totals.real),
        HumanSize(outcome.totals.apparent)
    );
    print!(
        "  entries: {} ({} dirs)  errors: {}  excluded (other fs): {}",
        outcome.entries, outcome.dirs, outcome.errors, outcome.excluded_dirs
    );
    if outcome.hardlink_inodes > 0 {
        print!(
            "  hardlinked inodes: {} (provisional first-seen totals)",
            outcome.hardlink_inodes
        );
    }
    println!();
    println!();
    println!("Top {} directories by real size:", cli.top);
    for dir in outcome.top_dirs_by_disk(cli.top) {
        let meta = outcome.dir(dir);
        println!(
            "  {:>10}  {}",
            HumanSize(meta.td).to_string(),
            outcome.path_of(dir).display()
        );
    }

    ExitCode::SUCCESS
}
