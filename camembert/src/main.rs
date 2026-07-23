mod config;
mod ui;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::{Args, Parser, Subcommand};
use tracing::{debug, error, info};
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use camembert_core::diff::{self, DiffOptions, DiffReport};
use camembert_core::dump::read::DumpReader;
use camembert_core::dump::{self, DumpMeta, encode_name};
use camembert_core::flat;
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

    /// Number of entries in the "top directories" and "top files" (D5)
    /// lists, summary mode only (env: TOP)
    ///
    /// One flag governs both lists; the interactive `t` mode's own cap is
    /// the separate `flat_cap` config-file key (default 1000) and is not
    /// affected by this flag.
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
    /// Defaults to auto when neither this, COLOR, nor camembert.toml's
    /// `color` key set anything; see the README's Configuration section
    /// for the full precedence (this flag > COLOR > camembert.toml >
    /// auto).
    #[arg(long, env = "COLOR", value_enum)]
    color: Option<ui::caps::ColorMode>,

    /// Theme for the interactive UI: tokyo-night, light, high-contrast
    /// (env: THEME)
    ///
    /// tokyo-night (default) is the truecolor-first dark palette; light
    /// is a Tokyo-Night-"day"-style variant for a light background;
    /// high-contrast maximizes contrast (no mid-greys), usable on either.
    /// Precedence: this flag > THEME > camembert.toml's `theme` key >
    /// an OSC 11 terminal background query (bounded to ~150ms, skipped
    /// outright on a non-tty or TERM=dumb; auto-picks light when the
    /// terminal reports one) > tokyo-night. See the README's
    /// Configuration section for camembert.toml's full format and path.
    #[arg(long, env = "THEME", value_enum)]
    theme: Option<ui::theme::ThemeName>,

    /// Disable micro-animations in the interactive UI (env: NO_MOTION)
    ///
    /// Bars and the donut wheel then always render at their exact target
    /// value instead of easing in over ~150ms on navigation/sort. Like
    /// `NO_COLOR`, `NO_MOTION` counts if set to any value at all, even
    /// the empty string — this flag and the env var both just mean
    /// "off", so (unlike `--color`) there is no typed value to parse.
    /// camembert.toml's `no_motion = true` has the same effect when
    /// neither this flag nor NO_MOTION is set.
    #[arg(long = "no-motion")]
    no_motion: bool,

    /// Disable the freeable `/proc` sweep in the interactive UI (env:
    /// NO_PROC_SWEEP)
    ///
    /// Skips both the scan-end sweep that powers the disk gauge's
    /// "· X.X GiB freeable" suffix, the `f` panel and its toast, and the
    /// pre-deletion open-file check `D` normally runs before the delete
    /// confirmation — for paranoid environments and containers with a
    /// masked /proc. Like NO_MOTION, any value at all counts as set, even
    /// the empty string; there is no camembert.toml key for this (see the
    /// README's Freeable section).
    #[arg(long = "no-proc-sweep")]
    no_proc_sweep: bool,
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
  completion, then print totals, the --top largest directories, and the
  --top largest files (D5; same flag, both lists) -- suppressed like the
  rest of the summary text when --output - streams the dump to stdout.

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

Themes (--theme, env THEME):
  tokyo-night (default, truecolor-first dark palette), light (a
  Tokyo-Night-\"day\"-style variant for a light background), high-contrast
  (maximizes contrast, avoids mid-greys, usable on either background).
  Errors always render in the same coral family and the amber signature
  accent stays recognizably amber in every theme (the exact shade may
  adjust per theme for contrast).

  Precedence: --theme > THEME > camembert.toml's `theme` key > an OSC 11
  terminal background query > tokyo-night. The OSC 11 step only runs
  when nothing above it chose a theme: it asks the terminal for its
  background color at startup (before the alternate screen opens),
  waits up to ~150ms, and auto-picks light if the reported color's
  relative luminance is > 0.5; a terminal that never answers, is not a
  tty, or reports TERM=dumb is treated as dark (today's default,
  unchanged). This never blocks longer than the timeout and never
  consumes more than that narrow window of stdin.

Config file (camembert.toml):
  Path: $XDG_CONFIG_HOME/camembert/camembert.toml, or
  ~/.config/camembert/camembert.toml when XDG_CONFIG_HOME is unset.
  A missing file is silently fine. All keys are optional:

    theme = \"tokyo-night\" | \"light\" | \"high-contrast\"
    color = \"auto\" | \"always\" | \"never\"
    no_motion = true | false
    flat_cap = 1000        # flat top-files cap (t mode); default shown

    [patterns]             # label = \"glob\"; file order = precedence,
    logs = \"*.log\"         # after the built-in presets (node_modules/,
    build = \"dist/\"        # .git/, target/, __pycache__/, .cache/,
                            # .venv/, *.log, *.tmp); reusing a preset's
                            # label replaces it in place (D1/D4).

  Pattern syntax: a basename glob matched against one path component
  (never a full path). Only * (zero or more bytes) and ? (exactly one
  byte) are special; every other character -- including { } [ ] -- is
  literal, not a brace/character class. A trailing / marks a directory
  pattern, which claims its whole matched subtree (D1); without one, the
  pattern matches non-directory entries only.

  Precedence for theme/color/no_motion: the matching CLI flag > its env
  var > this file > the built-in default (tokyo-night/auto/motion
  enabled) — except `theme`, where the OSC 11 query above still gets a
  turn between the config file and the default. flat_cap and [patterns]
  are config-file only, no CLI flag or env var.

  Parsing is per-key resilient: broken TOML *syntax* falls back to
  every default (unchanged from before flat_cap/[patterns] existed), but
  a bad individual value -- an invalid theme, a non-numeric flat_cap, a
  [patterns] entry whose value isn't a string, or a [patterns] table
  that isn't a table at all -- is warned about and defaulted on its own,
  never resetting the other keys or the other pattern entries. An
  invalid glob spec is skipped the same way. Every case logs a warning
  (see --log-file); the interactive UI additionally shows a one-time
  startup toast (\"N invalid patterns ignored — see log\") when any
  pattern was dropped either way.

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
  Enter, l, Right  open the directory under the cursor (tree mode); in
                   flat mode, jump to the row's containing directory in
                   tree view instead (cursor lands on the file); a no-op
                   in breakdown mode for now (see Flat view below)
  Backspace, h, Left  go back up to the parent (tree mode only)
  g / G            jump to the top / bottom
  d                sort by real (disk) size [default, descending]
  a                sort by apparent size
  n                sort by name (tree: raw bytes; flat: basename; breakdown:
                   label)
  m                sort by modification time (tree mode only)
  c                sort by item count (tree: subtree items; breakdown:
                   group entry count; not applicable in flat mode)
  e                sort by subtree error count (tree mode only)
                   (pressing the active sort key reverses the direction;
                   a key with no meaning in the active mode flashes
                   \"not available in this view\" instead of applying)
  p                show/hide the apparent-size column
  t                flat top files across the whole scan (see Flat view
                   below); press t again to return to the tree
  b                pattern breakdown (see Flat view below); press b again
                   to return to the tree
  Space            mark/unmark the row under the cursor for deletion,
                   then move down (a marked directory implies its whole
                   subtree; marks persist across navigation; works in
                   tree and flat mode, not breakdown mode)
  u                clear all marks
  v                review marked entries: a scrollable floating list of
                   every marked path with its size; Space unmarks the row
                   under the review cursor, D opens the delete
                   confirmation from there too, v or Esc closes the list
  D                delete the marked entries: opens a confirmation dialog
                   listing count, total size and the first paths;
                   pressing y confirms, any other key cancels
  f                freeable files: deleted-but-open files still holding
                   disk space (see Freeable below); f or Esc closes it
  ?                show the keyboard/mouse cheatsheet; ? or Esc closes it
  z                toggle zen mode: table only (no metric cards, disk
                   gauge or donut wheel) — header, table, footer and the
                   basket strip stay
  Esc              contextual: closes an open modal first; else leaves a
                   flat/breakdown mode back to the tree; only quits when
                   already in tree view with nothing open
  q, Ctrl-C        quit unconditionally (cancels the scan if still
                   running), regardless of mode or open modal

  While any of these floating panels (delete confirmation, review list,
  freeable panel, cheatsheet) is open, every key belongs to it alone;
  precedence when more than one could apply is confirmation > review
  list > freeable panel > cheatsheet, though in practice only one is
  ever open at a time.

Flat view & pattern breakdown (t/b, docs/design/flat-view-decisions.md):
  Two extra in-place table modes; cards/gauge/basket/footer stay put.
  't' lists the largest regular files across the whole scan (path,
  size, a hardlink badge), capped at flat_cap entries (config file,
  default 1000) with a footer note when the cap was hit. 'b' lists
  pattern groups (basename globs; see Config file below) with total
  size, entry count and % of scan, plus a trailing \"(uncategorized)\"
  row. Groups are a DISJOINT partition (D1): a directory matching a
  dir-pattern claims its whole subtree, so nothing nested re-counts
  into its own group, and the list/donut never sum past 100%. Both
  modes work during the scan (badged \"provisional\", live accumulator);
  post-scan figures are exact and recompute immediately after every
  deletion, including one performed from inside the mode itself.

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
  Open-file advisory: pressing D also refreshes a /proc check (unless
  --no-proc-sweep) matched against the marked selection two ways: a
  marked file's own (dev, ino), and, for a marked directory, any open
  file found anywhere underneath it — so marking a directory whose
  individual files are what a process actually holds open still warns,
  not just marking the file itself. Adds a line naming the busiest few
  holders — advisory only, it never blocks y. When that check only saw
  part of the process table (permission-gated /proc/[pid]/fd entries),
  the line says so rather than staying silent (the same caveat also
  covers a holder in a different mount namespace whose path doesn't
  textually match the marked directory), so an absent warning is never
  mistaken for a clean bill of health on a shared machine.

Freeable (deleted-but-open files):
  A process can keep a file's blocks allocated after every path to it is
  unlinked (df counts them, du/camembert's tree cannot see them — no
  path to attribute them to). Once the scan completes, one /proc sweep
  (skippable with --no-proc-sweep/NO_PROC_SWEEP) finds such files and,
  when the root filesystem's freeable total is at least 100 MiB AND at
  least 1% of that filesystem's capacity, shows a one-time toast (\"X.X
  GiB freeable by closing files — press f\") and a clickable \"· X.X GiB
  freeable\" suffix on the disk gauge. f opens the panel: each entry's
  last-known path, holder PID(s)/process name, and allocated size,
  grouped display-only under the deepest still-existing directory; a
  coverage line (\"N of M processes readable — run as root for the full
  view\") whenever /proc access was partial.

  What phase 1 covers and doesn't: scoped to the scan root's own
  filesystem only (the same one the disk gauge describes) — a btrfs
  layout split across several subvolume-mounted `st_dev`s shares one
  pool underneath, so the count under-reports there; files held open on
  a *different* crossed filesystem (--cross-filesystems) still show up
  in the panel, labeled by device, but are never added to the gauge.
  Holders visible only via mmap (no open file descriptor) are invisible
  without CAP_SYS_ADMIN and are not counted. memfd/tmpfs/shm-backed
  inodes are RAM, not disk, and are reported as one separate line rather
  than folded into the disk total. Nothing here is written to a dump
  (--output): open-file state is process state, stale the instant the
  sweep finishes, so a loaded dump simply has no freeable data.

Basket & toasts:
  While at least one entry is marked, a one-line basket strip appears
  above the footer (\"basket: N items, SIZE\") — gone again once nothing
  is marked, so browsing without ever marking anything never sees the
  layout shift. Top-right toast notifications announce things that just
  happened rather than input being validated: a dump written, a deletion
  finishing (with the space freed), the scan itself finishing while you
  keep browsing, and the freeable-sweep toast described above. Toasts
  stack and auto-dismiss after a few seconds; they never appear over the
  delete-confirmation dialog.";

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
        cross_filesystems: args.cross_filesystems,
    });

    if interactive {
        // camembert.toml sits below the CLI flag/env var in precedence
        // for all three of theme/color/motion's keys (design slice 6).
        let color = config::resolve_color(args.color, file_config.color);
        let theme_choice = config::resolve_theme(args.theme, file_config.theme);
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
        debug!(
            ?color,
            ?theme_choice,
            no_motion,
            no_proc_sweep,
            flat_cap = flat_config.cap,
            "resolved color/theme/motion/proc-sweep (CLI > env > camembert.toml, no-proc-sweep: CLI > env only)"
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
            flat_config,
            startup_toasts,
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

/// Non-interactive mode: scan to completion, then print the summary on
/// stdout (diagnostics stay on stderr via tracing). `flat_config` (D2/D4)
/// is folded once, after the scan, for the top-files section (D5) — no
/// live accumulator here, this run was never browsed.
fn summary(args: &ScanArgs, scanner: &Scanner, flat_config: &flat::FlatConfig) -> ExitCode {
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

        // D5: the flat-view top files, right beside the top-dirs list,
        // reusing --top/TOP the same way (attack finding 8: one flag, two
        // lists, the interactive view's own cap is independent — see
        // --help). Folded once over the finalized tree (canonical hardlink
        // attribution, same as everything above); `-o -` (dump to stdout)
        // already skips this whole branch, so the dump stream is never at
        // risk (attack finding 7).
        println!();
        println!("Top {} files by real size:", args.top);
        let flat_summary = flat::fold(outcome.tree(), &flat_config.patterns, flat_config.cap, 0);
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
