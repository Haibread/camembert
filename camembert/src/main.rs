mod ui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use camembert_core::dump::{self, DumpMeta};
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

    /// Write a dump of the scan to this file (camembert-dump v1, `.cmbt`)
    /// once the scan completes; `-` writes it to stdout, summary mode
    /// only (env: OUTPUT)
    ///
    /// The dump is JSON Lines in a seekable zstd container, readable with
    /// stock tools: `zstdcat dump.cmbt | jq`. Quitting the interactive
    /// mode mid-scan cancels the scan and skips the dump. With `-` the
    /// summary text is suppressed so stdout carries only the dump stream.
    #[arg(short = 'o', long = "output", env = "OUTPUT")]
    output: Option<PathBuf>,
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

Dump:
  --output FILE (env: OUTPUT) writes a camembert-dump v1 (.cmbt) after
  the scan: JSON Lines in a seekable zstd container that stock tools
  read directly (zstdcat dump.cmbt | jq). Hardlinked inodes are
  attributed to their canonical (smallest-path) link before writing.
  '-' streams the dump to stdout (summary mode only; the summary text is
  then suppressed). In interactive mode the dump is written when the
  scan completes; quitting mid-scan cancels the scan and skips it.

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

    if interactive && cli.output.as_deref() == Some(Path::new("-")) {
        // Binary dump bytes and a full-screen TUI cannot share stdout.
        eprintln!(
            "camembert: --output - (dump to stdout) requires summary mode; \
             add --no-ui or redirect stdout"
        );
        return ExitCode::FAILURE;
    }

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
        return match ui::run(scanner, &cli.path, cli.output.clone()) {
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

    let mut outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            error!(%err, "scan failed");
            return ExitCode::FAILURE;
        }
    };
    // Canonical hardlink attribution (D2/D3): totals below and any dump
    // are final, not first-seen provisional.
    outcome.finalize_hardlinks();

    let dump_to_stdout = cli.output.as_deref() == Some(Path::new("-"));
    if dump_to_stdout {
        info!("dump streams to stdout: summary text suppressed");
    } else {
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
                "  hardlinked inodes: {} (each counted once)",
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
    }

    if let Some(path) = &cli.output {
        let meta = DumpMeta {
            timestamp: SystemTime::now(),
        };
        let written = if dump_to_stdout {
            dump::write_dump(
                &outcome,
                std::io::BufWriter::new(std::io::stdout().lock()),
                &meta,
            )
        } else {
            dump::write_dump_to_path(&outcome, path, &meta)
        };
        match written {
            Ok(()) => info!(path = %path.display(), "dump written"),
            Err(err) => {
                error!(%err, path = %path.display(), "dump write failed");
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}
