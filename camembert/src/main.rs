mod ui;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use clap::{Args, Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use camembert_core::diff::{self, DiffOptions, DiffReport};
use camembert_core::dump::read::DumpReader;
use camembert_core::dump::{self, DumpMeta, encode_name};
use camembert_core::ncdu;
use camembert_core::scan::{ScanOptions, Scanner};
use camembert_core::size::{HumanSize, SignedHumanSize, parse_size};

/// Disk usage analyzer: what grew, what is freeable, what is cold.
///
/// Without a subcommand, scans PATH (interactive browser on a terminal,
/// summary otherwise). `diff` compares two dumps; `import` converts an
/// ncdu JSON export into a dump.
#[derive(Debug, Parser)]
#[command(version, about, after_help = AFTER_HELP)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    scan: ScanArgs,

    /// `tracing` filter directive (e.g. `info`, `camembert=debug`) (env: LOG_FILTER)
    ///
    /// Syntax: <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives>
    #[arg(
        long = "log-filter",
        env = "LOG_FILTER",
        default_value = "info",
        global = true
    )]
    log_filter: String,

    /// Write diagnostics to this file instead of the default target
    /// (env: LOG_FILE)
    ///
    /// Default target: stderr, except in the interactive scan mode where
    /// log output would corrupt the full-screen UI and is discarded.
    #[arg(long = "log-file", env = "LOG_FILE", global = true)]
    log_file: Option<PathBuf>,
}

/// Arguments of the default (scan) mode.
#[derive(Debug, Args)]
struct ScanArgs {
    /// Directory to scan (env: SCAN_PATH)
    ///
    /// To scan a directory literally named like a subcommand (`diff`,
    /// `import`), prefix it: `camembert ./diff`.
    #[arg(env = "SCAN_PATH", default_value = ".")]
    path: PathBuf,

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

    /// Color output in the interactive UI: auto, always, never (env: COLOR)
    ///
    /// auto detects the terminal's capabilities (truecolor via COLORTERM,
    /// 256 colors via TERM, 16 colors otherwise) and honors NO_COLOR (set
    /// to any value, even empty, disables color). always ignores NO_COLOR
    /// but is still capped by what the terminal advertises. never renders
    /// monochrome with ASCII bars (the wheel needs color and is hidden).
    #[arg(long, env = "COLOR", value_enum, default_value = "auto")]
    color: ui::caps::ColorMode,

    /// Disable micro-animations in the interactive UI (env: NO_MOTION)
    ///
    /// Bars and the donut wheel then always render at their exact target
    /// value instead of easing in over ~150ms on navigation/sort. Like
    /// `NO_COLOR`, `NO_MOTION` counts if set to any value at all, even
    /// the empty string — this flag and the env var both just mean
    /// "off", so (unlike `--color`) there is no typed value to parse.
    #[arg(long = "no-motion")]
    no_motion: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compare two dumps: what grew, shrank, appeared, disappeared
    ///
    /// Streams both ordered dumps through a constant-memory merge-join
    /// (never loads either tree) and prints the total delta, the top
    /// directories by growth and the top changed entries. Exit codes:
    /// 0 = OK (and growth below --threshold if given), 1 = growth above
    /// --threshold, 2 = error (unreadable/unordered/incomplete dump).
    #[command(after_help = DIFF_AFTER_HELP)]
    Diff(DiffArgs),

    /// Convert an ncdu JSON export (ncdu -o) into a camembert dump
    ///
    /// Streams the ncdu 1.x JSON format (minor versions 0-2; newer minors
    /// import with a warning, unknown fields are ignored) and writes an
    /// ordered .cmbt with hardlinks deduplicated and canonically
    /// attributed. The result diffs cleanly against fresh scans:
    /// `camembert import old-ncdu.json -o old.cmbt && camembert diff
    /// old.cmbt fresh.cmbt`. Exit codes: 0 = OK, 2 = error.
    #[command(after_help = IMPORT_AFTER_HELP)]
    Import(ImportArgs),
}

#[derive(Debug, Args)]
struct DiffArgs {
    /// The older dump (.cmbt)
    old: PathBuf,

    /// The newer dump (.cmbt)
    new: PathBuf,

    /// Number of directories and entries in each top list (env: TOP)
    #[arg(long, env = "TOP", default_value_t = 20)]
    top: usize,

    /// Machine output: JSON Lines instead of human text (env: JSON_OUTPUT)
    ///
    /// One `{"t":"summary",...}` object, then one `{"t":"dir",...}` per
    /// top directory and one `{"t":"entry",...}` per top entry; see
    /// --help of the diff subcommand for the field list.
    #[arg(long, env = "JSON_OUTPUT")]
    json: bool,

    /// Exit 1 when total disk growth exceeds this size (env: THRESHOLD)
    ///
    /// Size syntax: a decimal number with an optional binary-multiple
    /// unit K/M/G/T/P (1K = 1024 bytes), `iB`/`B` suffix and fractions
    /// allowed: 500M, 2G, 1.5GiB. Turns the diff into a monitoring
    /// probe: 0 = within budget, 1 = grew too much, 2 = error.
    #[arg(long, env = "THRESHOLD", value_parser = parse_size)]
    threshold: Option<u64>,
}

#[derive(Debug, Args)]
struct ImportArgs {
    /// The ncdu JSON export to convert; `-` reads stdin
    ///
    /// Accepts the output of `ncdu -o` (optionally with `-e` extended
    /// info), gzip NOT handled — decompress first (`zcat x.json.gz |
    /// camembert import - -o x.cmbt`).
    input: PathBuf,

    /// Where to write the camembert dump (.cmbt); `-` writes to stdout
    /// (env: OUTPUT)
    #[arg(short = 'o', long = "output", env = "OUTPUT")]
    output: PathBuf,
}

const AFTER_HELP: &str = "\
Subcommands:
  camembert [PATH]             scan (the default mode, described below)
  camembert diff OLD NEW       compare two dumps (growth, shrinkage, churn)
  camembert import JSON -o OUT convert an ncdu JSON export into a dump
  (see `camembert diff --help` / `camembert import --help`)

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

Look & feel (interactive mode):
  Colors and glyphs adapt to the terminal: truecolor -> 256 -> 16 -> mono
  (NO_COLOR honored, --color overrides), and sextant wheel -> half-block
  wheel -> ASCII bars without a wheel. Terminals narrower than 100
  columns collapse the side wheel panel into a compact mini-donut on the
  header line instead (not clickable, unlike the full panel); zen mode
  (`z`) and the ASCII rung hide the wheel outright regardless of width.
  See the README's \"Look & feel\" section for the exact detection rules.

  Table proportion bars and the donut wheel ease into position over
  ~150ms on navigation or a sort keypress (never longer — a scan's live
  growth is untouched, it already updates continuously); --no-motion
  (env NO_MOTION, any value counts, even empty, same rule as NO_COLOR)
  disables this and snaps both straight to their target value.

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
  e                sort by subtree error count (find what was unreadable)
                   (pressing the active sort key reverses the direction)
  p                show/hide the apparent-size column
  Space            mark/unmark the row under the cursor for deletion,
                   then move down (a marked directory implies its whole
                   subtree; marks persist across navigation)
  u                clear all marks
  v                review marked entries: a scrollable floating list of
                   every marked path with its size; Space unmarks the row
                   under the review cursor, D opens the delete
                   confirmation from there too, v or Esc closes the list
  D                delete the marked entries: opens a confirmation dialog
                   listing count, total size and the first paths;
                   pressing y confirms, any other key cancels
  ?                show the keyboard/mouse cheatsheet; ? or Esc closes it
  z                toggle zen mode: table only (no metric cards, disk
                   gauge or donut wheel) — header, table, footer and the
                   basket strip stay
  q, Esc, Ctrl-C   quit (cancels the scan if still running)

  While any of these floating panels (delete confirmation, review list,
  cheatsheet) is open, every key belongs to it alone; precedence when
  more than one could apply is confirmation > review list > cheatsheet,
  though in practice only one is ever open at a time.

Mouse (interactive mode):
  The mouse is additive: every key above still works, nothing requires
  it. Click a table row to select it; click it again (or double-click
  any row) to open it, matching Enter. The mouse wheel over the table
  scrolls the cursor. Click a donut wheel slice to open that child
  directly. Click a breadcrumb path segment in the header to jump to
  that ancestor directory, same as Backspace repeated. Click the errors
  metric card to sort by subtree error count, same as pressing e. Moving
  the mouse over a table row updates the selection card below the table
  (mtime, item count, % of parent, errors) without disturbing the
  keyboard cursor; moving the keyboard cursor reclaims the card.

Deleting (mark-then-confirm, with guard rails):
  Deletion only works once the scan has completed; during the scan the
  mark keys just show a hint. Marks refuse mount points (excluded
  directories) — unreadable (error) directories stay markable, deleting
  one is legitimate cleanup. Before anything is removed, every entry is
  re-checked: it must still exist, still be strictly under the scanned
  root, and its file type (and, for directories, its device) must still
  match what was scanned — anything that changed since the scan is
  skipped, never deleted. Symlinks are removed themselves, never
  followed. Failures (permissions, vanished files) never abort the
  batch: the footer sums them up and details go to the log (--log-file).
  Hardlinks: deleting one link of a multi-link inode only frees space
  when the last link goes; the dialog warns when the selection contains
  hardlinked files. Totals in the header shrink as entries are deleted.

Basket & toasts:
  While at least one entry is marked, a one-line basket strip appears
  above the footer (\"basket: N items, SIZE\") — gone again once nothing
  is marked, so browsing without ever marking anything never sees the
  layout shift. Top-right toast notifications announce things that just
  happened rather than input being validated: a dump written, a deletion
  finishing (with the space freed), the scan itself finishing while you
  keep browsing. Toasts stack and auto-dismiss after a few seconds; they
  never appear over the delete-confirmation dialog.";

const DIFF_AFTER_HELP: &str = "\
Output (default): a summary line (total disk/apparent/entry delta and
change counts), then 'Top N directories by growth' (signed subtree disk
delta from the dump totals — canonical hardlink attribution — biggest
growth first, shrinkage negative) and 'Top N entries by growth'.

Change kinds: added, removed, grown, shrunk, touched (same sizes,
different mtime), type-changed (file <-> symlink/device/directory).

JSON Lines schema (--json, env JSON_OUTPUT), one object per line:
  {\"t\":\"summary\",\"oldRoot\":S,\"newRoot\":S,\"diskDelta\":I,
   \"apparentDelta\":I,\"entryDelta\":I,\"added\":N,\"removed\":N,
   \"grown\":N,\"shrunk\":N,\"touched\":N,\"typeChanged\":N,
   \"dirsAdded\":N,\"dirsRemoved\":N}
  {\"t\":\"dir\",\"path\":S,\"change\":\"added|removed|changed\",
   \"diskDelta\":I,\"apparentDelta\":I,\"entryDelta\":I}
  {\"t\":\"entry\",\"path\":S,\"change\":\"added|removed|grown|shrunk|
   touched|typeChanged\",\"diskDelta\":I,\"apparentDelta\":I}
Paths are percent-encoded like dump names (non-UTF-8 bytes as %XX);
integers with magnitude >= 2^53 are emitted as decimal strings, exactly
like the dump format — parse both.

Monitoring probe: `camembert diff old.cmbt new.cmbt --threshold 2G`
exits 1 when the tree grew by more than 2 GiB (0 otherwise, 2 on error)
without printing anything extra — wire it straight into a check.

Requirements: both dumps must be ordered (header \"ordered\":true — the
default writer output) and complete (their `e` end marker present).
Unordered or truncated dumps are refused with exit code 2.";

const IMPORT_AFTER_HELP: &str = "\
Field mapping (ncdu -> dump): name -> n (raw bytes, re-encoded),
asize/dsize -> a/d, ino/nlink/hlnkc -> i/l with (dev,ino) hardlink
deduplication and canonical smallest-path attribution, read_error ->
err, excluded otherfs/othfs/kernfs -> a never-scanned directory stub
with ex, absent dev inherits the parent's.

Not carried (documented losses): uid/gid/mode (extended info) are
dropped; pattern/frmlink exclusion reasons collapse to ex:\"otherfs\";
mtime is 0 when the export was made without `ncdu -e`; the dev of a
non-hardlinked file is dropped; hlnkc without ino (very old exports)
cannot be deduplicated and counts fully; the ncdu metadata block is
ignored (as ncdu itself documents).

The ncdu export does not guarantee sibling order; the importer sorts
siblings by raw name bytes and computes subtree totals, so the result
is a first-class ordered dump, diffable against any other.";

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        None => run_scan(&cli),
        Some(Command::Diff(args)) => {
            if init_tracing(&cli, false).is_err() {
                return ExitCode::from(2);
            }
            run_diff(args)
        }
        Some(Command::Import(args)) => {
            if init_tracing(&cli, false).is_err() {
                return ExitCode::from(2);
            }
            run_import(args)
        }
    }
}

/// Install the global tracing subscriber. In interactive scan mode the
/// terminal belongs to ratatui: tracing output must never reach it (a
/// single log line prints at the raw-mode cursor, right across the UI),
/// so without --log-file it is discarded; everywhere else stderr is the
/// default target.
fn init_tracing(cli: &Cli, interactive: bool) -> Result<(), ()> {
    let writer = match (&cli.log_file, interactive) {
        (Some(path), _) => {
            let file = match std::fs::File::create(path) {
                Ok(file) => file,
                Err(err) => {
                    eprintln!("camembert: cannot open log file {}: {err}", path.display());
                    return Err(());
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
    Ok(())
}

// ---- default mode: scan ----

/// Whether animation is disabled: the `--no-motion` flag, or `NO_MOTION`
/// present in the environment at all — any value, even the empty
/// string, exactly like `NO_COLOR` (see `ui::caps`). Read directly
/// rather than through clap's `env` attribute: a plain bool flag's typed
/// env parsing cannot express "any value at all, including empty".
fn motion_disabled(cli_flag: bool, env: Option<&str>) -> bool {
    cli_flag || env.is_some()
}

fn run_scan(cli: &Cli) -> ExitCode {
    let args = &cli.scan;
    let interactive = !args.no_ui && std::io::stdout().is_terminal();

    if interactive && args.output.as_deref() == Some(Path::new("-")) {
        // Binary dump bytes and a full-screen TUI cannot share stdout.
        eprintln!(
            "camembert: --output - (dump to stdout) requires summary mode; \
             add --no-ui or redirect stdout"
        );
        return ExitCode::FAILURE;
    }

    if init_tracing(cli, interactive).is_err() {
        return ExitCode::FAILURE;
    }

    let scanner = Scanner::new(ScanOptions {
        threads: args.threads,
        cross_filesystems: args.cross_filesystems,
    });

    if interactive {
        let caps = ui::caps::Caps::detect(&ui::caps::TermEnv::from_env(), args.color);
        let animate = !motion_disabled(args.no_motion, std::env::var("NO_MOTION").ok().as_deref());
        return match ui::run(scanner, &args.path, args.output.clone(), caps, animate) {
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
    summary(args, &scanner)
}

/// Non-interactive mode: scan to completion, then print the summary on
/// stdout (diagnostics stay on stderr via tracing).
fn summary(args: &ScanArgs, scanner: &Scanner) -> ExitCode {
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

    let outcome = scanner.scan(&args.path);
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

    let dump_to_stdout = args.output.as_deref() == Some(Path::new("-"));
    if dump_to_stdout {
        info!("dump streams to stdout: summary text suppressed");
    } else {
        println!(
            "Scanned {} in {:.2}s",
            args.path.display(),
            outcome.elapsed.as_secs_f64()
        );
        println!(
            "  total: {} real, {} apparent",
            HumanSize(outcome.totals.real),
            HumanSize(outcome.totals.apparent)
        );
        print!(
            "  entries: {} ({} dirs)  errors: {}  excluded mounts: {} ({} kernfs)",
            outcome.entries,
            outcome.dirs,
            outcome.errors,
            outcome.excluded_dirs,
            outcome.excluded_kernfs
        );
        if outcome.hardlink_inodes > 0 {
            print!(
                "  hardlinked inodes: {} (each counted once)",
                outcome.hardlink_inodes
            );
        }
        println!();
        println!();
        println!("Top {} directories by real size:", args.top);
        for dir in outcome.top_dirs_by_disk(args.top) {
            let meta = outcome.dir(dir);
            println!(
                "  {:>10}  {}",
                HumanSize(meta.td).to_string(),
                outcome.path_of(dir).display()
            );
        }
    }

    if let Some(path) = &args.output {
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

    // "Comptabiliser l'illisible": when parts of the tree could not be
    // read, say where — an unexplained error count is exactly the kind of
    // dishonest total this tool exists to avoid. (Not with `-o -`: stdout
    // carries the dump stream, nothing else may be printed to it.)
    if outcome.errors > 0 && !dump_to_stdout {
        println!();
        println!(
            "{} entries could not be read; most affected directories:",
            outcome.errors
        );
        for (dir, direct_errors) in outcome.top_dirs_by_errors(10) {
            println!(
                "  {:>6} errors  {}",
                direct_errors,
                outcome.path_of(dir).display()
            );
        }
    }

    ExitCode::SUCCESS
}

// ---- diff ----

fn run_diff(args: &DiffArgs) -> ExitCode {
    let open = |path: &Path| match DumpReader::open(path) {
        Ok(reader) => Ok(reader),
        Err(err) => {
            error!(path = %path.display(), %err, "cannot open dump");
            eprintln!("camembert diff: {}: {err}", path.display());
            Err(())
        }
    };
    let (Ok(old), Ok(new)) = (open(&args.old), open(&args.new)) else {
        return ExitCode::from(2);
    };
    let report = match diff::diff_dumps(old, new, &DiffOptions { top: args.top }) {
        Ok(report) => report,
        Err(err) => {
            error!(%err, "diff failed");
            eprintln!("camembert diff: {err}");
            return ExitCode::from(2);
        }
    };

    if args.json {
        print!("{}", report.to_json_lines());
    } else {
        print_human_report(&report, args.top);
    }

    if let Some(threshold) = args.threshold
        && report.disk_delta > 0
        && report.disk_delta.unsigned_abs() > threshold
    {
        info!(
            disk_delta = report.disk_delta,
            threshold, "growth exceeds the threshold"
        );
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn print_human_report(report: &DiffReport, top: usize) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "diff {} -> {}",
        encode_name(&report.old_root),
        encode_name(&report.new_root)
    );
    let _ = writeln!(
        out,
        "  total: {} disk, {} apparent, {:+} entries",
        SignedHumanSize(report.disk_delta),
        SignedHumanSize(report.apparent_delta),
        report.entry_delta
    );
    let counts = &report.counts;
    let _ = writeln!(
        out,
        "  added {}, removed {}, grown {}, shrunk {}, touched {}, type-changed {} \
         (dirs: +{}/-{})",
        counts.added,
        counts.removed,
        counts.grown,
        counts.shrunk,
        counts.touched,
        counts.type_changed,
        counts.dirs_added,
        counts.dirs_removed
    );

    if !report.top_dirs.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Top {top} directories by growth:");
        for dir in &report.top_dirs {
            let _ = writeln!(
                out,
                "  {:>12}  {}",
                SignedHumanSize(dir.disk_delta).to_string(),
                encode_name(&dir.path)
            );
        }
    }
    if !report.top_entries.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Top {top} entries by growth:");
        for entry in &report.top_entries {
            let _ = writeln!(
                out,
                "  {:>12}  {:<12}  {}",
                SignedHumanSize(entry.disk_delta).to_string(),
                entry.change.as_str(),
                encode_name(&entry.path)
            );
        }
    }
}

// ---- import ----

fn run_import(args: &ImportArgs) -> ExitCode {
    let outcome = if args.input == Path::new("-") {
        ncdu::import(std::io::stdin().lock())
    } else {
        match std::fs::File::open(&args.input) {
            Ok(file) => ncdu::import(std::io::BufReader::new(file)),
            Err(err) => {
                error!(path = %args.input.display(), %err, "cannot open ncdu export");
                eprintln!("camembert import: {}: {err}", args.input.display());
                return ExitCode::from(2);
            }
        }
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            error!(%err, "import failed");
            eprintln!("camembert import: {err}");
            return ExitCode::from(2);
        }
    };

    let meta = DumpMeta {
        timestamp: SystemTime::now(),
    };
    let to_stdout = args.output == Path::new("-");
    let written = if to_stdout {
        dump::write_dump(
            &outcome,
            std::io::BufWriter::new(std::io::stdout().lock()),
            &meta,
        )
    } else {
        dump::write_dump_to_path(&outcome, &args.output, &meta)
    };
    if let Err(err) = written {
        error!(%err, path = %args.output.display(), "dump write failed");
        eprintln!(
            "camembert import: cannot write {}: {err}",
            args.output.display()
        );
        return ExitCode::from(2);
    }
    if !to_stdout {
        println!(
            "Imported {} into {}: {} entries ({} dirs), {} real, {} apparent, {} errors",
            args.input.display(),
            args.output.display(),
            outcome.entries,
            outcome.dirs,
            HumanSize(outcome.totals.real),
            HumanSize(outcome.totals.apparent),
            outcome.errors
        );
        if outcome.hardlink_inodes > 0 {
            println!(
                "  hardlinked inodes: {} (deduplicated, canonically attributed)",
                outcome.hardlink_inodes
            );
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_disabled_matches_the_no_color_truthy_rule() {
        assert!(
            !motion_disabled(false, None),
            "neither set: motion stays on"
        );
        assert!(motion_disabled(true, None), "--no-motion alone");
        assert!(motion_disabled(false, Some("1")), "NO_MOTION=1");
        assert!(
            motion_disabled(false, Some("")),
            "NO_MOTION set to the empty string still counts"
        );
        assert!(motion_disabled(true, Some("0")), "both set: still disabled");
    }
}
