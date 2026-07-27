mod cli;
mod config;
mod ui;

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::Parser;
use tracing::{debug, error, info};
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use camembert_core::diff::{self, DiffOptions, DiffReport};
use camembert_core::dump::read::DumpReader;
use camembert_core::dump::{self, DumpMeta, encode_name};
use camembert_core::flat;
use camembert_core::ncdu;
use camembert_core::query;
use camembert_core::scan::{ScanOptions, Scanner};
use camembert_core::size::{HumanSize, SignedHumanSize};

use cli::{Cli, Command, DiffArgs, ImportArgs, ScanArgs};

/// Converts the CLI-only mirror ([`cli::ColorModeArg`]) into the real
/// `ui` type. Kept here (rather than in `cli.rs`) because it needs `ui`
/// in scope, which the `camembert-mangen` binary does not link — see the
/// module doc comment on `cli.rs`.
impl From<cli::ColorModeArg> for ui::caps::ColorMode {
    fn from(arg: cli::ColorModeArg) -> Self {
        match arg {
            cli::ColorModeArg::Auto => Self::Auto,
            cli::ColorModeArg::Always => Self::Always,
            cli::ColorModeArg::Never => Self::Never,
        }
    }
}

/// Converts the CLI-only mirror ([`cli::ThemeNameArg`]) into the real
/// `ui` type; see [`From<cli::ColorModeArg>`] above for why this lives
/// in `main.rs` rather than `cli.rs`.
impl From<cli::ThemeNameArg> for ui::theme::ThemeName {
    fn from(arg: cli::ThemeNameArg) -> Self {
        match arg {
            cli::ThemeNameArg::TokyoNight => Self::TokyoNight,
            cli::ThemeNameArg::Light => Self::Light,
            cli::ThemeNameArg::HighContrast => Self::HighContrast,
        }
    }
}

/// Restore the default disposition of `SIGPIPE`, which the Rust runtime
/// sets to `SIG_IGN` before `main` runs.
///
/// With `SIG_IGN`, a closed stdout turns every `println!` into a write
/// error, and the macro's answer to a write error is a panic: `camembert
/// /srv --no-ui | head -3` printed a stack trace and exited 101 where
/// `du`, `find` and every other filter in the pipeline exit silently.
/// Restoring `SIG_DFL` makes the process die from the signal instead —
/// exit status 141, no trace, the shell's normal end for a pipeline whose
/// reader went away.
#[cfg(unix)]
fn restore_default_sigpipe() {
    // SAFETY: sets one signal to its default disposition, before any
    // thread is spawned and before any handler is installed — nothing to
    // race with, and `SIG_DFL` needs no async-signal-safe handler code.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Windows has no `SIGPIPE` to restore — a closed pipe reader surfaces as
/// a normal `ErrorKind::BrokenPipe` write error there instead of a signal,
/// so there is nothing for this platform to do.
#[cfg(not(unix))]
fn restore_default_sigpipe() {}

fn main() -> ExitCode {
    restore_default_sigpipe();
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

    // camembert.toml is loaded unconditionally now (unlike before flat view
    // landed): --no-ui's summary needs `[patterns]`/`flat_cap` (D5) just as
    // much as the interactive UI needs theme/color/motion, so the one read
    // that used to be interactive-only now serves both modes.
    let file_config = config::load();
    let (flat_config, pattern_warnings) = config::build_flat_config(&file_config);
    // D4: every invalid-pattern reason is already `tracing::warn!`-logged
    // individually (config-level structural issues in `config::parse`,
    // glob-compile issues in `PatternSet::push`) — this is only the
    // one-time combined count the interactive UI surfaces as a startup
    // toast; --no-ui runs have no toast queue, so the log is the whole
    // story there.
    if !pattern_warnings.is_empty() {
        tracing::warn!(
            count = pattern_warnings.len(),
            "invalid flat-view patterns ignored at startup"
        );
    }
    let startup_toasts = if pattern_warnings.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "{} invalid pattern{} ignored — see log",
            pattern_warnings.len(),
            if pattern_warnings.len() == 1 { "" } else { "s" }
        )]
    };

    let scanner = Scanner::new(ScanOptions {
        threads: args.threads,
        cross_filesystems: !args.one_filesystem,
        statx_engine: args.statx_engine.into(),
        link_counts: args.links,
    });

    if interactive {
        // camembert.toml sits below the CLI flag/env var in precedence
        // for all three of theme/color/motion's keys (design slice 6).
        let color = config::resolve_color(args.color.map(Into::into), file_config.color);
        let theme_choice = config::resolve_theme(args.theme.map(Into::into), file_config.theme);
        let no_motion = config::resolve_no_motion(
            args.no_motion,
            std::env::var("NO_MOTION").ok().is_some(),
            file_config.no_motion,
        );
        // Freeable phase 1, D7: flag + env only, presence semantics like
        // NO_MOTION — no camembert.toml key (the decisions doc deliberately
        // keeps this out of the config file).
        let no_proc_sweep = config::resolve_no_proc_sweep(
            args.no_proc_sweep,
            std::env::var("NO_PROC_SWEEP").ok().is_some(),
        );
        // Freeable phase 2, D3: flag + env only, same shape as
        // no_proc_sweep — no camembert.toml key.
        let no_fiemap =
            config::resolve_no_fiemap(args.no_fiemap, std::env::var("NO_FIEMAP").ok().is_some());
        debug!(
            ?color,
            ?theme_choice,
            no_motion,
            no_proc_sweep,
            no_fiemap,
            flat_cap = flat_config.cap,
            "resolved color/theme/motion/proc-sweep/fiemap (CLI > env > camembert.toml, no-proc-sweep/no-fiemap: CLI > env only)"
        );
        let caps = ui::caps::Caps::detect(&ui::caps::TermEnv::from_env(), color);
        let animate = !no_motion;
        // D2: only the interactive UI accumulates a live flat-view summary
        // during the scan (browse-during-scan); --no-ui folds once, after
        // the scan, in `summary` below.
        let scanner = scanner.with_flat(flat_config.clone());
        return match ui::run(
            scanner,
            &args.path,
            args.output.clone(),
            caps,
            animate,
            theme_choice,
            no_proc_sweep,
            no_fiemap,
            flat_config,
            startup_toasts,
            file_config.queries.clone(),
            args.filter.clone(),
        ) {
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
    summary(args, &scanner, &flat_config)
}

/// "Now" in unix seconds for [`query::ApplyOptions::now_unix`] (D7's
/// `--filter`'s `older:`/`newer:` cutoffs) — read once, not inside the
/// fold, for a reproducible result.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Non-interactive mode: scan to completion, then print the summary on
/// stdout (diagnostics stay on stderr via tracing). `flat_config` (D2/D4)
/// is folded once, after the scan, for the top-files section (D5) — no
/// live accumulator here, this run was never browsed. `--filter`/`FILTER`
/// (D7) is parsed *strictly* up front: unlike the interactive palette
/// (broken terms are inert, per the module docs), a query a script is
/// about to rely on must be entirely valid or loudly rejected — every
/// parse error is printed and the scan itself is skipped (exit 2), rather
/// than silently running over a smaller-than-intended query.
/// What the summary's `hardlinked inodes: N` line actually counted.
///
/// The number answers two different questions depending on whether the
/// scan holds a real link count, and the difference is large enough to
/// read as a regression if the line does not say so: on
/// `C:\Windows\System32\drivers` the same tree reports 728 with `--links`
/// and 0 without, while every byte of every total is identical. 728 is
/// "inodes with siblings somewhere on the volume"; 0 is "inodes this scan
/// actually reached twice", and only the second is knowable for free on
/// Windows. See `docs/design/windows-nlink-dossier.md`.
fn hardlink_line_qualifier(outcome: &camembert_core::scan::ScanOutcome) -> &'static str {
    if outcome.link_counts_known {
        "each counted once"
    } else {
        "reached by more than one path in this scan; each counted once. Links \
         outside it were not checked — `--links`"
    }
}

/// Whether the summary prints the hardlink line at all.
///
/// A zero is normally not worth a line, and on a scan that holds real link
/// counts it is genuinely informative: nothing here is hardlinked. Without
/// them it is not. `C:\Windows\System32\drivers` reports 0 by default and
/// 728 under `--links`, so suppressing the line would answer "are these
/// files hardlinked?" with silence — which reads as *no* — on a tree where
/// 96 % of the files have a WinSxS twin. The line is what carries the
/// qualifier saying the question was not asked, so it has to survive the
/// zero that makes it necessary.
fn show_hardlink_line(outcome: &camembert_core::scan::ScanOutcome) -> bool {
    outcome.hardlink_inodes > 0 || !outcome.link_counts_known
}

fn summary(args: &ScanArgs, scanner: &Scanner, flat_config: &flat::FlatConfig) -> ExitCode {
    let filter_query = match &args.filter {
        Some(text) if !text.trim().is_empty() => {
            let parsed = query::parse(text);
            if !parsed.errors.is_empty() {
                eprintln!(
                    "camembert: --filter has {} problem(s) and will not run:",
                    parsed.errors.len()
                );
                for err in &parsed.errors {
                    eprintln!("  {}", err.message);
                }
                return ExitCode::from(2);
            }
            Some(parsed.query)
        }
        _ => None,
    };

    // Progress line on stderr (via tracing) roughly every second while the
    // scan blocks this thread. The poller waits on a channel, not a plain
    // sleep, so a scan that finishes in milliseconds isn't held hostage by
    // a 1 s nap at join time (a bench-visible stall on small trees).
    let progress = scanner.progress();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let poller = std::thread::spawn(move || {
        loop {
            match done_rx.recv_timeout(Duration::from_millis(1000)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => info!(
                    entries = progress.entries(),
                    dirs = progress.dirs(),
                    errors = progress.errors(),
                    disk = %HumanSize(progress.disk_bytes()),
                    "scanning"
                ),
            }
        }
    });

    let outcome = scanner.scan(&args.path);
    let _ = done_tx.send(());
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

    // D7: fold the filter once, over the finalized (canonical-hardlink)
    // tree — same shape as the interactive palette's post-scan apply,
    // minus the debounce (there are no keystrokes to debounce here).
    let filter_result = filter_query.as_ref().map(|q| {
        let hardlinks = query::HardlinkIndex::build(&outcome, 0);
        query::apply(
            outcome.tree(),
            q,
            &flat_config.patterns,
            &hardlinks,
            &query::ApplyOptions {
                cap: flat_config.cap,
                epoch: 0,
                now_unix: unix_now(),
                threads: std::thread::available_parallelism()
                    .map(std::num::NonZeroUsize::get)
                    .unwrap_or(1),
            },
        )
    });

    let dump_to_stdout = args.output.as_deref() == Some(Path::new("-"));

    // The dump goes out before any summary text: it is the deliverable,
    // the summary is chatter. A `camembert … -o dump.cmbt | head` closes
    // stdout mid-summary and kills the process (SIGPIPE, restored to its
    // default disposition in `main`) — writing the dump first means that
    // costs the user nothing but the text they already stopped reading.
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

    if dump_to_stdout {
        info!("dump streams to stdout: summary text suppressed");
    } else {
        println!(
            "Scanned {} in {:.2}s",
            args.path.display(),
            outcome.elapsed.as_secs_f64()
        );
        match &filter_result {
            Some(result) => println!(
                "  matched (--filter {:?}): {} real, {} apparent, {} entries — of {} real scanned",
                args.filter.as_deref().unwrap_or_default(),
                HumanSize(result.matched_disk),
                HumanSize(result.matched_apparent),
                result.matched_entries,
                HumanSize(outcome.totals.real),
            ),
            None => println!(
                "  total: {} real, {} apparent",
                HumanSize(outcome.totals.real),
                HumanSize(outcome.totals.apparent)
            ),
        }
        print!(
            "  entries: {} ({} dirs)  errors: {}  excluded mounts: {} ({} kernfs)",
            outcome.entries,
            outcome.dirs,
            outcome.errors,
            outcome.excluded_dirs,
            outcome.excluded_kernfs
        );
        if show_hardlink_line(&outcome) {
            print!(
                "  hardlinked inodes: {} ({})",
                outcome.hardlink_inodes,
                hardlink_line_qualifier(&outcome)
            );
        }
        println!();
        println!();

        match &filter_result {
            Some(result) => {
                // D7: both lists computed over the match set — never the
                // whole-scan lists sitting silently under a filtered
                // header (the same honesty rule the interactive `b` mode
                // holds itself to under a filter).
                println!("Top {} directories by matched real size:", args.top);
                let mut dirs: Vec<(camembert_core::tree::DirId, u64)> = outcome
                    .tree()
                    .dir_ids()
                    .filter(|&dir| !outcome.tree().is_removed(outcome.tree().dir(dir).node))
                    .map(|dir| (dir, result.dir_total(dir).disk))
                    .collect();
                dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.index().cmp(&b.0.index())));
                for (dir, disk) in dirs.into_iter().take(args.top) {
                    println!(
                        "  {:>10}  {}",
                        HumanSize(disk).to_string(),
                        outcome.path_of(dir).display()
                    );
                }
                println!();
                println!("Top {} matched files by real size:", args.top);
                for file in result.top_files.iter().take(args.top) {
                    let badge = if file.hardlink { " \u{26d3}" } else { "" };
                    println!(
                        "  {:>10}  {}{badge}",
                        HumanSize(file.disk).to_string(),
                        outcome.tree().path_of_node(file.node).display()
                    );
                }
                if result.truncated {
                    println!(
                        "  (top {} of more eligible files shown; flat_cap in camembert.toml)",
                        flat_config.cap
                    );
                }
                let residual_disk = result.residual.disk;
                if residual_disk > 0 {
                    println!(
                        "  (+{} in {} directory inode(s), never matched by any query)",
                        HumanSize(residual_disk),
                        result.residual.dirs
                    );
                }
            }
            None => {
                println!("Top {} directories by real size:", args.top);
                for dir in outcome.top_dirs_by_disk(args.top) {
                    let meta = outcome.dir(dir);
                    println!(
                        "  {:>10}  {}",
                        HumanSize(meta.td).to_string(),
                        outcome.path_of(dir).display()
                    );
                }

                // D5: the flat-view top files, right beside the top-dirs
                // list, reusing --top/TOP the same way (attack finding 8:
                // one flag, two lists, the interactive view's own cap is
                // independent — see --help). Folded once over the
                // finalized tree (canonical hardlink attribution, same as
                // everything above); `-o -` (dump to stdout) already
                // skips this whole branch, so the dump stream is never at
                // risk (attack finding 7).
                println!();
                println!("Top {} files by real size:", args.top);
                let flat_summary =
                    flat::fold(outcome.tree(), &flat_config.patterns, flat_config.cap, 0);
                for file in flat_summary.top_files.iter().take(args.top) {
                    let badge = if file.hardlink { " \u{26d3}" } else { "" };
                    println!(
                        "  {:>10}  {}{badge}",
                        HumanSize(file.disk).to_string(),
                        outcome.tree().path_of_node(file.node).display()
                    );
                }
                if flat_summary.truncated {
                    println!(
                        "  (top {} of more eligible files shown; flat_cap in camembert.toml)",
                        flat_config.cap
                    );
                }
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
