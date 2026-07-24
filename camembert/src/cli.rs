//! Clap-facing definitions: the `Cli`/subcommand argument surface and the
//! long `--help`/`after_help` text. Kept separate from `main.rs` so the
//! `camembert-mangen` binary (`src/bin/mangen.rs`) can pull in this file
//! with `#[path]` and derive `camembert.1` from it without linking the
//! whole TUI or introducing a `lib` target.
//!
//! One consequence of that split: `ScanArgs::color`/`ScanArgs::theme`
//! cannot use `ui::caps::ColorMode`/`ui::theme::ThemeName` directly (the
//! `ui` module is TUI rendering code with no reason to exist in a man-page
//! generator, and pulling it in would make most of it dead code there).
//! They use the small CLI-only mirrors [`ColorModeArg`]/[`ThemeNameArg`]
//! instead, converted to the real types in `main.rs` — the same pattern
//! [`StatxEngineArg`] already used for [`StatxEngine`].

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use camembert_core::scan::StatxEngine;
use camembert_core::size::parse_size;

/// `<cargo package version> (<git short sha>)`, e.g. `0.1.0 (abc1234)`.
///
/// The commit is captured by `build.rs` at compile time (`unknown` when
/// `.git` or `git` itself isn't available, `-dirty`-suffixed for an unclean
/// worktree).
pub(crate) const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CAMEMBERT_GIT_SHA"),
    ")"
);

/// Disk usage analyzer: what grew, what is freeable, what is stale.
///
/// Without a subcommand, scans PATH (interactive browser on a terminal,
/// summary otherwise). `diff` compares two dumps; `import` converts an
/// ncdu JSON export into a dump.
#[derive(Debug, Parser)]
#[command(version = VERSION, about, after_help = AFTER_HELP)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    #[command(flatten)]
    pub(crate) scan: ScanArgs,

    /// `tracing` filter directive (e.g. `info`, `camembert=debug`) (env: LOG_FILTER)
    ///
    /// Syntax: <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives>
    #[arg(
        long = "log-filter",
        env = "LOG_FILTER",
        default_value = "info",
        global = true
    )]
    pub(crate) log_filter: String,

    /// Write diagnostics to this file instead of the default target
    /// (env: LOG_FILE)
    ///
    /// Default target: stderr, except in the interactive scan mode where
    /// log output would corrupt the full-screen UI and is discarded.
    #[arg(long = "log-file", env = "LOG_FILE", global = true)]
    pub(crate) log_file: Option<PathBuf>,
}

/// Arguments of the default (scan) mode.
#[derive(Debug, Args)]
pub(crate) struct ScanArgs {
    /// Directory to scan (env: SCAN_PATH)
    ///
    /// To scan a directory literally named like a subcommand (`diff`,
    /// `import`), prefix it: `camembert ./diff`.
    #[arg(env = "SCAN_PATH", default_value = ".")]
    pub(crate) path: PathBuf,

    /// Scan worker threads; 0 = auto (env: THREADS)
    ///
    /// The auto policy probes the scan root's backing device once per
    /// scan and adapts: non-rotational storage (SSD/NVMe) uses
    /// `min(cores, 16)` threads; rotational storage (spinning disks,
    /// where parallel readers thrash the seek head) is capped at 2. On
    /// filesystems that report an anonymous device number (btrfs,
    /// notably — no direct sysfs node), it instead resolves the covering
    /// mount's real backing device from `/proc/self/mountinfo` and
    /// probes that. When the medium still can't be determined (network
    /// filesystems, containers with no matching mount, unreadable
    /// sysfs/mountinfo, a `tmpfs`/`overlay` source) it falls back to the
    /// historical `min(2x cores, 8)`. A multi-device btrfs volume is
    /// classified from whichever single member device the mount table
    /// happens to report, which can misjudge a volume mixing SSDs and
    /// HDDs. An explicit value here always wins and skips detection
    /// entirely.
    #[arg(long, env = "THREADS", default_value_t = 0)]
    pub(crate) threads: usize,

    /// Stay on the scan root's filesystem: stop at mount points instead of
    /// descending into them (env: ONE_FILESYSTEM)
    ///
    /// By default (this flag unset) camembert crosses filesystem boundaries
    /// and descends into every filesystem mounted under the scan root,
    /// including RAM-backed `tmpfs` and other disks — their bytes are real
    /// usage of *those* filesystems, not phantom totals. Kernel
    /// pseudo-filesystems (`/proc`, `/sys`, cgroups, …) are always excluded,
    /// regardless of this flag — their sizes are not disk usage. On btrfs,
    /// descending into subvolumes also walks snapshot subvolumes (e.g.
    /// `.snapshots`), which can multiply-count snapshotted data;
    /// `--one-filesystem` avoids that too. Two related caveats this flag
    /// does NOT fully cover: a bind mount whose source is on the same
    /// filesystem is descended as an ordinary directory and its subtree is
    /// double-counted, because its `st_dev` never differs from its
    /// parent's, even with `--one-filesystem`; and the same block device
    /// mounted at two different paths inside the scan is descended twice
    /// under the default crossing behavior. Hardlink deduplication only
    /// catches `nlink > 1` files, so `nlink == 1` files and directories
    /// still double-count in both cases.
    #[arg(long, env = "ONE_FILESYSTEM")]
    pub(crate) one_filesystem: bool,

    /// Stat engine for the scan: auto, sync, io_uring — experimental (env: STATX_ENGINE)
    ///
    /// Per-entry metadata (statx) is fetched either with plain syscalls
    /// (`sync`) or batched through per-worker io_uring rings (`io_uring`,
    /// kernel 5.6+; unavailable under default-seccomp Docker, gVisor, and
    /// the io_uring_disabled sysctl). `auto` uses io_uring only for
    /// low-parallelism scans (2 workers or fewer, the rotational-media
    /// policy) where its batching measurably helps, and plain syscalls
    /// otherwise; it probes io_uring once at scan start and falls back to
    /// sync when it is denied. A forced `io_uring` also falls back rather
    /// than fail the scan. Scan results are identical whichever engine
    /// runs — only speed can differ. The choice is logged at info level
    /// (`statx=io_uring` / `statx=sync`). Experimental: this knob and the
    /// auto heuristic may change once cold-cache data is in.
    #[arg(long, env = "STATX_ENGINE", value_enum, default_value_t = StatxEngineArg::Auto)]
    pub(crate) statx_engine: StatxEngineArg,

    /// Number of entries in the "top directories" and "top files" (D5)
    /// lists, summary mode only (env: TOP)
    ///
    /// One flag governs both lists; the interactive `t` mode's own cap is
    /// the separate `flat_cap` config-file key (default 1000) and is not
    /// affected by this flag.
    #[arg(long, env = "TOP", default_value_t = 20)]
    pub(crate) top: usize,

    /// Disable the interactive UI: scan, then print the summary (env: NO_UI)
    ///
    /// This is also the automatic behavior when stdout is not a terminal
    /// (pipes, redirections).
    #[arg(long = "no-ui", env = "NO_UI")]
    pub(crate) no_ui: bool,

    /// Write a dump of the scan to this file (camembert-dump v1, `.cmbt`)
    /// once the scan completes; `-` writes it to stdout, summary mode
    /// only (env: OUTPUT)
    ///
    /// The dump is JSON Lines in a seekable zstd container, readable with
    /// stock tools: `zstdcat dump.cmbt | jq`. Quitting the interactive
    /// mode mid-scan cancels the scan and skips the dump. With `-` the
    /// summary text is suppressed so stdout carries only the dump stream.
    #[arg(short = 'o', long = "output", env = "OUTPUT")]
    pub(crate) output: Option<PathBuf>,

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
    pub(crate) color: Option<ColorModeArg>,

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
    pub(crate) theme: Option<ThemeNameArg>,

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
    pub(crate) no_motion: bool,

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
    pub(crate) no_proc_sweep: bool,

    /// Disable the freeable-2 selection oracle in the interactive UI (env:
    /// NO_FIEMAP)
    ///
    /// Skips every `FS_IOC_FIEMAP` call: no mark-time reclaim estimate, the
    /// delete confirmation dialog falls back to the phase-1 hardlink-only
    /// advisory, and the ambient exclusive floor is disabled outright — no
    /// background pass, no in-bar bright segment, no `excl ≥ …`/"fully
    /// shared" line on the selection card. For filesystems or containers
    /// where the ioctl is unavailable, undesired, or where the per-mark
    /// `open`+FIEMAP cost on a large selection isn't wanted. Like
    /// NO_PROC_SWEEP, any value at all counts as set, even the empty
    /// string; there is no camembert.toml key for this (see the README's
    /// Reclaim oracle section).
    #[arg(long = "no-fiemap")]
    pub(crate) no_fiemap: bool,

    /// Filter query applied to the scan (env: FILTER)
    ///
    /// Same grammar as the interactive Ctrl-K/`/` palette: whitespace-
    /// separated terms, implicitly ANDed, each optionally negated with a
    /// leading `!` (`*.log >100M !older:1y`). See the README's Filtering
    /// section for the full grammar (substrings, globs, `ext:`, `kind:`,
    /// `is:`, size sugar, `older:`/`newer:`, `dir/` ancestor terms,
    /// double-quoted literals). In summary mode (`--no-ui`) the top
    /// directories/files lists are computed over the match set and a
    /// non-empty query must parse *completely clean* — any unparseable
    /// term exits 2 with every parse error printed, so a typo is never
    /// silently ignored in a script. In interactive mode the query
    /// pre-applies the instant the scan completes (broken terms are inert
    /// there instead, exactly like typing them into the palette).
    #[arg(long, env = "FILTER")]
    pub(crate) filter: Option<String>,
}

/// CLI face of [`StatxEngine`] (experimental knob, see `--statx-engine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub(crate) enum StatxEngineArg {
    /// io_uring for scans with ≤ 2 workers (probed, sync fallback),
    /// plain syscalls otherwise.
    #[default]
    Auto,
    /// Plain statx syscalls (with the fstatat fallback). Always works.
    Sync,
    /// io_uring-batched statx (still falls back if the probe fails).
    #[value(name = "io_uring")]
    IoUring,
}

impl std::fmt::Display for StatxEngineArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Sync => "sync",
            Self::IoUring => "io_uring",
        })
    }
}

impl From<StatxEngineArg> for StatxEngine {
    fn from(arg: StatxEngineArg) -> Self {
        match arg {
            StatxEngineArg::Auto => Self::Auto,
            StatxEngineArg::Sync => Self::Sync,
            StatxEngineArg::IoUring => Self::IoUring,
        }
    }
}

/// CLI face of `ui::caps::ColorMode` (see the module doc comment for why
/// this mirror exists instead of using that type directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum ColorModeArg {
    /// Detect from the environment (`COLORTERM`, `TERM`, `NO_COLOR`).
    Auto,
    /// Ignore `NO_COLOR`; still capped by what the terminal advertises.
    Always,
    /// No color at all (implies ASCII bars, no wheel).
    Never,
}

/// CLI face of `ui::theme::ThemeName` (see the module doc comment for why
/// this mirror exists instead of using that type directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum ThemeNameArg {
    /// Truecolor-first dark palette (the default look).
    TokyoNight,
    /// Tokyo-Night-"day"-style variant: dark text assumptions, tuned for
    /// a light background. Auto-selected by OSC 11 background detection
    /// when nothing else picked a theme.
    Light,
    /// Maximum-contrast palette avoiding mid-greys; usable on either a
    /// dark or a light background.
    HighContrast,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
pub(crate) struct DiffArgs {
    /// The older dump (.cmbt)
    pub(crate) old: PathBuf,

    /// The newer dump (.cmbt)
    pub(crate) new: PathBuf,

    /// Number of directories and entries in each top list (env: TOP)
    #[arg(long, env = "TOP", default_value_t = 20)]
    pub(crate) top: usize,

    /// Machine output: JSON Lines instead of human text (env: JSON_OUTPUT)
    ///
    /// One `{"t":"summary",...}` object, then one `{"t":"dir",...}` per
    /// top directory and one `{"t":"entry",...}` per top entry; see
    /// --help of the diff subcommand for the field list.
    #[arg(long, env = "JSON_OUTPUT")]
    pub(crate) json: bool,

    /// Exit 1 when total disk growth exceeds this size (env: THRESHOLD)
    ///
    /// Size syntax: a decimal number with an optional binary-multiple
    /// unit K/M/G/T/P (1K = 1024 bytes), `iB`/`B` suffix and fractions
    /// allowed: 500M, 2G, 1.5GiB. Turns the diff into a monitoring
    /// probe: 0 = within budget, 1 = grew too much, 2 = error.
    #[arg(long, env = "THRESHOLD", value_parser = parse_size)]
    pub(crate) threshold: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct ImportArgs {
    /// The ncdu JSON export to convert; `-` reads stdin
    ///
    /// Accepts the output of `ncdu -o` (optionally with `-e` extended
    /// info), gzip NOT handled — decompress first (`zcat x.json.gz |
    /// camembert import - -o x.cmbt`).
    pub(crate) input: PathBuf,

    /// Where to write the camembert dump (.cmbt); `-` writes to stdout
    /// (env: OUTPUT)
    #[arg(short = 'o', long = "output", env = "OUTPUT")]
    pub(crate) output: PathBuf,
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

    [queries]              # label = \"query string\"; read-only saved
    big_logs = \"*.log >100M\" # filters (D6), shown in the Ctrl-K/`/`
    stale = \"older:1y\"       # palette when its input is empty.

  Pattern syntax: a basename glob matched against one path component
  (never a full path). Only * (zero or more bytes) and ? (exactly one
  byte) are special; every other character -- including { } [ ] -- is
  literal, not a brace/character class. A trailing / marks a directory
  pattern, which claims its whole matched subtree (D1); without one, the
  pattern matches non-directory entries only.

  Precedence for theme/color/no_motion: the matching CLI flag > its env
  var > this file > the built-in default (tokyo-night/auto/motion
  enabled) — except `theme`, where the OSC 11 query above still gets a
  turn between the config file and the default. flat_cap, [patterns] and
  [queries] are config-file only, no CLI flag or env var (--filter/FILTER
  is a separate, one-shot query -- see Filtering above).

  Parsing is per-key resilient: broken TOML *syntax* falls back to
  every default (unchanged from before flat_cap/[patterns] existed), but
  a bad individual value -- an invalid theme, a non-numeric flat_cap, a
  [patterns]/[queries] entry whose value isn't a string, or either table
  not being a table at all -- is warned about and defaulted on its own,
  never resetting the other keys or the other pattern/query entries. An
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
  Ctrl-K, /        open the filter/command palette (see Filtering below)
  ?                show the keyboard/mouse cheatsheet; ? or Esc closes it
  z                toggle zen mode: table only (no metric cards, disk
                   gauge or donut wheel) — header, table, footer and the
                   basket strip stay
  Esc              contextual: closes the palette first; else an open
                   modal; else leaves a flat/breakdown mode back to the
                   tree; else clears an active filter; only quits when
                   already in tree view with nothing open and no filter
  q, Ctrl-C        quit unconditionally (cancels the scan if still
                   running), regardless of mode or open modal — EXCEPT
                   inside the palette, where only Ctrl-C quits: every
                   other key, q included, is a character being typed

  While any of these floating surfaces (palette, delete confirmation,
  review list, freeable panel, cheatsheet) is open, every key belongs to
  it alone; precedence is palette > confirmation > review list > freeable
  panel > cheatsheet, though in practice only one is ever open at a time.

Filtering (Ctrl-K/`/`, docs/design/query-decisions.md):
  Ctrl-K opens a floating palette over the tree: typed text with no
  leading '>' is a filter query, parsed and applied live (debounced
  ~100ms) to the whole cockpit (tree table, donut, metric cards) as you
  type; a leading '>' switches the same box to fuzzy command search
  (every keyboard shortcut, by name). '/' opens the identical palette,
  pre-scoped to the query side. Grammar: whitespace-separated terms,
  implicitly ANDed, each optionally negated with a leading '!' --
  substrings (smartcase), \"literal quoted\" substrings, *.glob?
  patterns, dir/ ancestor constraints, >SIZE/<SIZE disk-byte sugar,
  older:DUR/newer:DUR mtime age (h/d/w/mo/y), kind:file|dir|symlink,
  ext:EXT, is:hardlink|error|excluded. Broken terms are inert (shown
  inline, span + message) -- the rest of the query still applies; only
  --filter's --no-ui mode is strict (see below). Filtering is post-scan
  only: mid-scan the query side shows \"filter available once the scan
  completes\" (command mode still works). A hardlinked file matches by
  ANY of its names -- a non-canonical link shows as a 0-byte row flagged
  counted-at-its-canonical-path, never silently absent.

  An active filter shows a persistent pill above the basket strip: query
  text, matched entries/bytes, the dir-inode residual (\"+N GiB in M
  directory inode(s) not counted\") whenever nonzero, and \"Esc clears\".
  The tree table/donut/cards compose over the match set (a directory row
  survives only when its filtered subtree still matches; the viewed
  directory itself always renders, even at zero matches); t/b do the
  same. Directory marks are refused while a filter is active (file marks
  still work) -- a filtered directory shows only its matches, so marking
  it would delete everything inside it, matched or not.

  While the palette is open it owns the keyboard: only Esc (close),
  Enter (commit), the arrows/Home/End/Backspace/Delete, and Ctrl-C (quit)
  are interpreted specially -- every other key, q included, is text.
  Up/Down recall query history (persisted to
  $XDG_STATE_HOME/camembert/history, or ~/.local/state/camembert/history,
  one query per line, bounded to 200, written atomically); on an empty
  query box they instead browse camembert.toml's read-only [queries]
  table. See --filter below for the same grammar on the command line.

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

  Confidence verdict: the panel and the delete-confirmation dialog each
  open with one line grading the figure below it — \"confidence:
  measured\" (nothing measurable is missing), \"partial\" (a named part
  is missing, the majority was read), \"fragmentary\" (more than half
  the inputs were unreadable, or the figure may overstate) or \"no
  figure\" (the feature is off, the pass hasn't landed, /proc was
  unreadable — never a fallback number in its place). The reason names
  the one or two dominant limiters; every detailed caveat keeps its own
  line underneath, and the graded word is plain text so the level reads
  without color. The dialog's verdict grades the reclaim estimate; the
  open-file advisory carries its own coverage caveat separately.

  What phase 1 covers and doesn't: scoped to the scan root's own
  filesystem only (the same one the disk gauge describes) — a btrfs
  layout split across several subvolume-mounted `st_dev`s shares one
  pool underneath, so the count under-reports there; files held open on
  a *different* filesystem the scan crossed into (the default; restrict
  with --one-filesystem) still show up in the panel, labeled by device,
  but are never added to the gauge.
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
