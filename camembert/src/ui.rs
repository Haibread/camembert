//! Interactive TUI: browse the tree **while the scan runs** (D5), in the
//! "dashboard cockpit" layout (docs/design/tui-design.md): header with
//! the `▞ camembert` signature, metric cards, disk gauge, table + donut
//! wheel split, selection card, footer.
//!
//! The render loop is wait-free: every frame loads the current
//! [`ViewSnapshot`] from the [`ViewBus`] (arc-swap) and re-sorts only when
//! the generation or the sort changed; it never blocks on the scan.
//! Navigation goes the other way through the capacity-1 latest-wins nav
//! cell. Once the scan finishes the owner thread exits and this module
//! serves navigation itself from the frozen arena
//! ([`camembert_core::view::build_snapshot`] on the [`ScanOutcome`]).
//!
//! Rendering is capability-gated ([`caps::Caps`], detected at startup):
//! colors walk the truecolor → 256 → 16 → mono ladder, glyphs the
//! sextant → half-block → ASCII ladder. All drawing helpers here only
//! *place* content; the pure geometry/color logic lives in the
//! unit-tested [`caps`], [`theme`], [`wheel`] and [`fmt`] submodules.
//!
//! Diagnostics: `tracing` only — stdout/stderr belong to the terminal UI
//! while it runs (redirect stderr to a file to capture logs).
//!
//! Mouse support (design slice 3) is additive to the keyboard map: rows,
//! wheel slices, the breadcrumb and the errors card are all clickable,
//! hit-tested against a [`state::FrameGeometry`] recomputed every frame
//! from the actual layout — see [`draw`] and [`handle_mouse`].
//!
//! Design slice 4 adds the deletion basket strip (persistent above the
//! footer while anything is marked), the `v` review list and `?`
//! cheatsheet (floating modals, precedence confirm > review > cheatsheet
//! — see [`handle_key`]), and top-right [`toast::ToastQueue`]
//! notifications for events the footer's [`Flash`] is a poor fit for
//! (see the module doc on [`toast`] for the split). Both new modals'
//! keyboard/mouse maps are documented in [`keymap`], the single table the
//! `?` cheatsheet renders from.

mod anim;
pub mod caps;
mod filterview;
mod flatview;
mod fmt;
mod freeable_panel;
mod history;
mod keymap;
mod osc11;
mod palette;
mod state;
pub mod theme;
mod toast;
mod wheel;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Paragraph, Row as TableRow, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};
use tracing::{debug, error, info, warn};

use camembert_core::delete;
use camembert_core::dump::{self, DumpMeta};
use camembert_core::flat::{self, FlatConfig};
use camembert_core::freeable::{self, Ledger};
use camembert_core::query::{self, ApplyOptions, FilterResult, HardlinkIndex, Query};
use camembert_core::scan::{LiveScan, ScanOutcome, Scanner};
use camembert_core::size::HumanSize;
use camembert_core::tree::{DirId, NodeFlags, NodeId, Tree};
use camembert_core::view::{self, RowState};

use caps::{Caps, GlyphLevel};
use fmt::DiskSpace;
use palette::{CommandAction, PaletteMode};
use state::{
    ConfirmState, FrameGeometry, MarkRefusal, ReviewState, SortKey, TableGeometry, UiState,
    ViewMode, WheelGeometry, show_hardlink_note, show_updating_note,
};
use theme::{Theme, ThemeName};
use toast::ToastQueue;

/// Flash shown when a flat/breakdown action needs the frozen post-scan
/// arena (Enter-jump, marking): both need a real path to resolve a
/// containing directory or build a [`state::MarkedEntry`], and a path
/// requires walking the arena's parent chain — not shareable with the UI
/// thread mid-scan (D3; see [`flatview`]'s module doc for why that's a
/// real API boundary and not a shortcut). The name alone (shown in the
/// table) is available live; only these two actions stay gated.
const FLAT_ROW_DETAILS_LOCKED: &str = "row details available once the scan completes";

/// Flash for Enter on a breakdown row (D3 phase 1: group drill-down is
/// wave 3's query language, not this feature).
const BREAKDOWN_DRILLDOWN_LOCKED: &str = "group drill-down comes with the query language";

/// Flash for a sort key the active mode has no meaningful column for (D3
/// — e.g. `m`/`e` in a flat/breakdown mode, or `c` in flat mode).
const SORT_NOT_APPLICABLE: &str = "not available in this view";

/// Frame budget: poll timeout of the render loop (~30 fps, D5) while
/// something needs a timely redraw without new input — a running scan,
/// an in-flight bar/donut animation, or a toast/flash winding down
/// (design slice 5).
const FRAME: Duration = Duration::from_millis(33);

/// Poll timeout the rest of the time: the scan is done, nothing is
/// animating, and no transient note is showing, so nothing on screen
/// changes without the user doing something. Long enough that it never
/// wakes the loop on its own (keeping a quiescent UI's CPU cost at
/// effectively zero between keypresses), short enough to bound the wait
/// defensively.
const IDLE_POLL: Duration = Duration::from_secs(3600);

/// Width, in terminal cells, of the header-line mini-donut the wheel
/// panel collapses to below [`MIN_WHEEL_TERMINAL_WIDTH`] (design
/// slice 5).
const MINI_DONUT_WIDTH: u16 = 6;

/// Two clicks on the same cell within this window count as a
/// double-click (navigate into the row) — independent of the
/// already-selected-row shortcut, which navigates on a single click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Rows moved per mouse-wheel notch over the table.
const SCROLL_STEP: usize = 3;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// ASCII spinner used when the glyph ladder bottoms out.
const SPINNER_ASCII: [char; 4] = ['|', '/', '-', '\\'];

/// Signature glyph opening the header.
const SIGNATURE: &str = "▞ camembert";

/// Width of the proportion-bar column in the table.
const BAR_WIDTH: usize = 12;

/// Below this terminal width the side wheel panel has no room: the table
/// takes the full width and a compact mini-donut rides the header line
/// instead ([`draw_mini_donut`], design slice 5). Zen mode (`z`) and the
/// ASCII glyph rung hide the wheel outright regardless of width — see
/// [`wheel_layout`].
const MIN_WHEEL_TERMINAL_WIDTH: u16 = 100;

/// Footer hint while mark/delete keys are inactive during the scan.
const DELETION_LOCKED: &str = "deletion available once the scan completes";

/// Flash for a directory-mark attempt under an active filter (D4): the row
/// shows only matching descendants, so marking it would delete everything
/// inside it, matched or not — the exact trap query-attack-a names.
const DIR_MARK_FILTER_LOCKED: &str =
    "directory marks are disabled while a filter is active — clear the filter first";

/// Immutable per-session render context: capabilities, palette, disk
/// space of the scanned filesystem.
struct RenderCtx {
    caps: Caps,
    theme: Theme,
    /// `None` when statvfs failed (gauge shows "unavailable").
    disk: Option<DiskSpace>,
    /// Whether micro-animations are enabled (design slice 5):
    /// `--no-motion`/`NO_MOTION` set this to `false`, in which case bars
    /// and the donut always render at their target value.
    animate: bool,
    /// `--no-proc-sweep`/`NO_PROC_SWEEP` (freeable phase 1, D7): disables
    /// both the scan-end `/proc` sweep and the pre-deletion open-file
    /// index refresh — for paranoid environments and containers with a
    /// masked `/proc`.
    no_proc_sweep: bool,
    /// Flat-view config (D2/D4: presets + `[patterns]` + `flat_cap`) —
    /// kept around post-scan to recompute the authoritative
    /// [`flat::fold`] on a render-time epoch mismatch (see
    /// [`ensure_flat_summary_fresh`]).
    flat_config: FlatConfig,
}

impl RenderCtx {
    fn ascii(&self) -> bool {
        self.caps.glyphs == GlyphLevel::Ascii
    }
}

/// Transient footer message (mark refusals, deletion summaries).
struct Flash {
    message: Option<(String, Instant)>,
}

impl Flash {
    const TTL: Duration = Duration::from_secs(4);

    fn new() -> Self {
        Self { message: None }
    }

    fn set(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now() + Self::TTL));
    }

    /// The current message, dropping it once expired.
    fn current(&mut self) -> Option<&str> {
        if let Some((_, until)) = &self.message
            && Instant::now() > *until
        {
            self.message = None;
        }
        self.message.as_ref().map(|(text, _)| text.as_str())
    }

    /// Whether a message is set, without checking expiry — cheap enough
    /// to consult every frame when deciding the render loop's poll
    /// deadline (design slice 5): a soon-to-expire flash still needs a
    /// timely redraw to actually disappear on schedule.
    fn is_set(&self) -> bool {
        self.message.is_some()
    }
}

/// Where the rows come from.
enum Phase {
    /// Owner thread alive: snapshots and navigation over the bus.
    Scanning(LiveScan),
    /// Scan over: this thread (reads) and the off-thread filter fold
    /// (reads, D5) share the frozen arena through a `RwLock` rather than
    /// a bare `Box` — the *only* writer is a confirmed deletion, which
    /// briefly blocks on the write lock exactly like it would block on
    /// any other exclusive access, while an in-flight background filter
    /// fold holds the read side for its duration (tens of ms even at
    /// scale). Every other reader (navigation, the hardlink index build,
    /// drawing) takes the read lock too. See `spawn_filter_fold`.
    Done(Arc<RwLock<ScanOutcome>>),
    /// Transient marker while moving between the two.
    Transitioning,
}

/// Acquire the read side of a [`Phase::Done`] lock, recovering from
/// poisoning rather than panicking (a panicked reader/writer elsewhere in
/// the session must not turn every subsequent frame into a crash — the
/// arena itself is still exactly as valid as it was the instant before).
fn read_outcome(lock: &RwLock<ScanOutcome>) -> std::sync::RwLockReadGuard<'_, ScanOutcome> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Acquire the write side (deletion only — see [`Phase::Done`]'s doc).
fn write_outcome(lock: &RwLock<ScanOutcome>) -> std::sync::RwLockWriteGuard<'_, ScanOutcome> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run the interactive UI over a live scan of `path`. Blocks until the
/// user quits; quitting mid-scan cancels the scan (workers stop, partial
/// results are dropped). When `output` is set, a dump is written once the
/// scan completes — never on a mid-scan cancel. `animate` is `false` for
/// `--no-motion`/`NO_MOTION` (design slice 5): bars and the donut then
/// always render at their target value, no easing. `theme_choice` is
/// whatever `--theme`/`THEME`/the config file already decided (design
/// slice 6); `None` means none of them did, in which case an OSC 11
/// background query — bounded, and skipped outright on a non-tty or
/// `TERM=dumb` — auto-selects `light` when the terminal answers with a
/// light background, before the alternate screen opens. `no_proc_sweep` is
/// `--no-proc-sweep`/`NO_PROC_SWEEP` (freeable phase 1, D7): disables both
/// the scan-end `/proc` sweep and the pre-deletion open-file index
/// refresh. `flat_config` (D2/D4) is what `main` already handed the
/// scanner via `Scanner::with_flat` — kept here too so post-scan
/// deletions can recompute the authoritative [`flat::fold`] (the scanner
/// itself only needed it to seed the live accumulator). `startup_toasts`
/// (D4) surfaces config-time warnings collected before the UI existed —
/// today just the combined `[patterns]` warning count ("N invalid
/// patterns ignored — see log"). `saved_queries` (D6) is
/// `camembert.toml`'s read-only `[queries]` table (label, query string),
/// offered in the Ctrl-K/`/` palette when its input is empty.
/// `filter_text` (D7) pre-applies `--filter`/`FILTER` the instant the scan
/// completes, exactly as if the user had typed it into the palette and
/// pressed Enter — empty means no pre-applied filter.
#[allow(clippy::too_many_arguments)]
pub fn run(
    scanner: Scanner,
    path: &Path,
    output: Option<PathBuf>,
    caps: Caps,
    animate: bool,
    theme_choice: Option<ThemeName>,
    no_proc_sweep: bool,
    flat_config: FlatConfig,
    startup_toasts: Vec<String>,
    saved_queries: Vec<(String, String)>,
    filter_text: Option<String>,
) -> io::Result<()> {
    info!(
        ?caps,
        animate,
        ?theme_choice,
        no_proc_sweep,
        "terminal capabilities detected"
    );
    let theme_name = resolve_theme_name(theme_choice);
    let disk = disk_space(path);
    let live = scanner.scan_live(path);
    // ratatui::init enters the alternate screen, enables raw mode, and
    // installs a panic hook that restores the terminal first.
    let mut terminal = ratatui::init();
    enable_mouse_capture();
    let ctx = RenderCtx {
        caps,
        theme: Theme::new(theme_name, caps.color),
        disk,
        animate,
        no_proc_sweep,
        flat_config,
    };
    let result = event_loop(
        &mut terminal,
        live,
        output,
        &ctx,
        startup_toasts,
        saved_queries,
        filter_text,
    );
    disable_mouse_capture();
    ratatui::restore();
    result
}

/// Theme precedence's last step (design slice 6, design §Color and
/// capabilities point 5): an explicit choice (CLI/env/config, resolved
/// by `main`'s `config` module before this ever runs) always wins;
/// otherwise an OSC 11 query decides between the default dark theme and
/// `light`. Runs before `ratatui::init` touches the terminal — there is
/// no alternate screen to protect yet, and the query itself puts stdin
/// into its own narrow, bounded raw-mode window and restores the
/// original settings before returning (see
/// `osc11::query_terminal_background`), so it can never hang, echo
/// escape garbage to the screen, or swallow more than a sliver of early
/// user input.
fn resolve_theme_name(explicit: Option<ThemeName>) -> ThemeName {
    if let Some(name) = explicit {
        debug!(?name, "theme: explicit (CLI, THEME, or camembert.toml)");
        return name;
    }
    let term = std::env::var("TERM").ok();
    if !osc11::should_query(
        term.as_deref(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    ) {
        debug!("theme: OSC 11 query skipped (not a tty, or TERM dumb/unset); defaulting dark");
        return ThemeName::default();
    }
    match osc11::query_terminal_background() {
        Some((r, g, b)) if osc11::is_light(r, g, b) => {
            debug!(r, g, b, "theme: OSC 11 reported a light background");
            ThemeName::Light
        }
        Some((r, g, b)) => {
            debug!(r, g, b, "theme: OSC 11 reported a dark background");
            ThemeName::default()
        }
        None => {
            debug!("theme: OSC 11 query got no answer in time; assuming dark");
            ThemeName::default()
        }
    }
}

/// Turn on crossterm mouse reporting and extend the panic hook
/// `ratatui::init` just installed so a mid-session panic disables it
/// again before the terminal is restored — otherwise the outer terminal
/// is left capturing the mouse after camembert exits. A failure to enable
/// is logged and never fatal: the UI degrades to keyboard-only.
fn enable_mouse_capture() {
    if let Err(err) = crossterm::execute!(io::stdout(), EnableMouseCapture) {
        warn!(%err, "failed to enable mouse capture: mouse input stays inactive");
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_mouse_capture();
        previous(info);
    }));
}

/// Inverse of [`enable_mouse_capture`]; safe to call even if enabling
/// never succeeded (or was never attempted) — worst case, an inert
/// escape sequence.
fn disable_mouse_capture() {
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
}

/// statvfs on the scan root, for the disk gauge. A failure is logged and
/// degrades the gauge to "unavailable" — never fatal.
fn disk_space(path: &Path) -> Option<DiskSpace> {
    match rustix::fs::statvfs(path) {
        Ok(vfs) => {
            let capacity = vfs.f_blocks.saturating_mul(vfs.f_frsize);
            let free = vfs.f_bfree.saturating_mul(vfs.f_frsize);
            Some(DiskSpace {
                capacity,
                used: capacity.saturating_sub(free),
            })
        }
        Err(err) => {
            warn!(%err, path = %path.display(), "statvfs failed: disk gauge unavailable");
            None
        }
    }
}

/// Result of [`finish_scan`]'s dump attempt, driving the "dump written"
/// toast at the call site that stays around to show it (quitting mid-scan
/// exits right after and never looks at this).
enum DumpOutcome {
    /// No `--output` was given.
    NotRequested,
    Written(PathBuf),
    /// Cancelled mid-scan (--output skipped) or the write itself failed;
    /// either way already logged, nothing more to say in the UI.
    Unavailable,
}

/// Finalize hardlink attribution and, when requested, write the dump.
/// Dump failures are logged, not fatal — the browsing session goes on.
fn finish_scan(outcome: &mut ScanOutcome, output: Option<PathBuf>) -> DumpOutcome {
    outcome.finalize_hardlinks();
    let Some(path) = output else {
        return DumpOutcome::NotRequested;
    };
    if outcome.cancelled {
        warn!(path = %path.display(), "scan cancelled mid-run: dump not written");
        return DumpOutcome::Unavailable;
    }
    let meta = DumpMeta {
        timestamp: SystemTime::now(),
    };
    match dump::write_dump_to_path(outcome, &path, &meta) {
        Ok(()) => {
            info!(path = %path.display(), "dump written");
            DumpOutcome::Written(path)
        }
        Err(err) => {
            error!(%err, path = %path.display(), "dump write failed");
            DumpOutcome::Unavailable
        }
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    live: LiveScan,
    output: Option<PathBuf>,
    ctx: &RenderCtx,
    startup_toasts: Vec<String>,
    saved_queries: Vec<(String, String)>,
    filter_text: Option<String>,
) -> io::Result<()> {
    let bus = Arc::clone(live.bus());
    let mut output = output;
    let mut phase = Phase::Scanning(live);
    let mut ui = UiState::new(bus.load());
    let mut table_state = TableState::default();
    let started = Instant::now();
    // Local generations continue past the last live one so the sort cache
    // invalidates on post-scan navigation.
    let mut local_generation: u64 = 0;
    let mut flash = Flash::new();
    let mut toasts = ToastQueue::new();
    // D4: config-time warnings collected before the UI existed (today just
    // the combined invalid-pattern count) — one toast each, same as any
    // other startup notice.
    for text in startup_toasts {
        toasts.push(text);
    }
    // Last left-click's time and cell, for double-click detection —
    // independent of the click-already-selected-row shortcut.
    let mut last_click: Option<(Instant, u16, u16)> = None;
    // Bar/donut animation state (design slice 5) — see the `anim` module
    // doc. `ctx.animate` is `false` for `--no-motion`/`NO_MOTION`.
    let mut motion = anim::Motion::new(ctx.animate);
    // Freeable phase 1 (D4): `Some` from scan end until the off-thread
    // sweep's result lands, polled non-blockingly below (step 2.5) — never
    // set at all under `--no-proc-sweep`/`NO_PROC_SWEEP`.
    let mut sweep_rx: Option<Receiver<Ledger>> = None;
    // D6: the palette's clock/IO-owning runtime (history file, the
    // debounce timer, the off-thread filter fold's channel) — kept out of
    // `PaletteState`/`UiState` the same way `Flash`/`sweep_rx` are kept out
    // of the terminal-free state modules.
    let mut palette_rt = PaletteRuntime::new(saved_queries);
    // D7: `--filter`/`FILTER` pre-applies the instant the scan completes,
    // exactly like a palette query the user typed and committed.
    let mut pending_filter_text = filter_text.filter(|text| !text.trim().is_empty());

    loop {
        // 1. Input (drain everything pending; block at most one frame
        //    while something needs a timely redraw of its own accord —
        //    otherwise idle: a quiescent UI costs nothing between
        //    keypresses, design slice 5).
        let mut deadline =
            if needs_frequent_polling(&phase, &flash, &toasts, &motion, &sweep_rx, &palette_rt) {
                FRAME
            } else {
                IDLE_POLL
            };
        while event::poll(deadline)? {
            deadline = Duration::ZERO;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match handle_key(
                        key.code,
                        key.modifiers,
                        &mut ui,
                        &mut phase,
                        &mut local_generation,
                        &mut flash,
                        &mut toasts,
                        ctx.no_proc_sweep,
                        &mut palette_rt,
                        &ctx.flat_config,
                    ) {
                        Action::Quit => {
                            if let Phase::Scanning(live) = phase {
                                info!("quit during scan: cancelling");
                                live.cancel();
                                match live.join() {
                                    Ok(mut outcome) => {
                                        debug!(cancelled = outcome.cancelled, "scan wound down");
                                        // Rarely, the scan finished before the
                                        // cancel landed: honor --output then.
                                        // The process exits right after, so
                                        // there is no toast to show for it.
                                        finish_scan(&mut outcome, output.take());
                                    }
                                    Err(err) => debug!(%err, "scan failed while quitting"),
                                }
                            }
                            return Ok(());
                        }
                        Action::Continue => {}
                    }
                }
                Event::Mouse(mouse) => {
                    handle_mouse(mouse, &mut ui, &phase, &mut last_click, &mut flash);
                }
                _ => {}
            }
        }

        // 2. Scan finished? Take over the arena.
        if matches!(&phase, Phase::Scanning(live) if live.is_finished()) {
            let Phase::Scanning(live) = std::mem::replace(&mut phase, Phase::Transitioning) else {
                unreachable!("checked above");
            };
            let mut outcome = live.join().map_err(io::Error::other)?;
            info!(
                entries = outcome.entries,
                cancelled = outcome.cancelled,
                "scan finished; UI now serves the frozen arena"
            );
            // Canonical hardlink attribution + dump, before the frozen
            // arena starts serving views (D3: corrected at scan end).
            let dump_outcome = finish_scan(&mut outcome, output.take());
            // Two toasts (design slice 4): the dump, when one was
            // written, and the scan itself finishing — the browsing
            // session keeps going, so both are worth a transient note
            // instead of only a log line.
            if let DumpOutcome::Written(path) = dump_outcome {
                toasts.push(format!("dump written: {}", path.display()));
            }
            toasts.push(format!(
                "scan finished in {}",
                fmt::humanize_duration(outcome.elapsed)
            ));
            // Freeable phase 1 (D4): one sweep off the UI thread, scoped to
            // the scan root's own filesystem — the same `st_dev`
            // camembert-core already recorded on the root directory for
            // mount-boundary detection (D2: "the same filesystem the
            // statvfs disk gauge describes"). Reusing that scanned value
            // is cheaper and more honest than a fresh `statat` here, which
            // would race a filesystem that changed underneath since the
            // scan started.
            if !ctx.no_proc_sweep {
                let root_dev = outcome.dir(outcome.root()).dev;
                sweep_rx = spawn_freeable_sweep(root_dev);
            }
            local_generation = ui.snapshot().generation;
            phase = Phase::Done(Arc::new(RwLock::new(outcome)));
            // Re-view the current dir so states/totals show final values,
            // resolving any nav request the owner no longer serves.
            let dir = ui.pending_nav().unwrap_or(ui.snapshot().dir);
            serve_local(&phase, dir, &mut local_generation, &mut ui);
            // D7: `--filter`/`FILTER` pre-applies now, exactly like a
            // palette query the user typed and committed — the interactive
            // UI is lenient (broken terms are inert, per the module docs),
            // unlike `--no-ui`'s strict parse, so this never blocks the
            // session over a typo; it just toasts the problem count.
            if let Some(text) = pending_filter_text.take() {
                let parsed = query::parse(&text);
                if !parsed.errors.is_empty() {
                    for err in &parsed.errors {
                        warn!(message = %err.message, "--filter: parse issue (term ignored)");
                    }
                    toasts.push(format!(
                        "--filter: {} term(s) ignored — see log",
                        parsed.errors.len()
                    ));
                }
                request_filter_fold(
                    &phase,
                    &ui,
                    &mut palette_rt,
                    &ctx.flat_config,
                    parsed.query,
                    text,
                );
            }
        }

        // 2.5. Freeable sweep result landed? (D4/D5 — polled
        // non-blockingly; `needs_frequent_polling` above keeps the loop at
        // FRAME cadence while `sweep_rx` is still pending, so this lands
        // within a frame or two of the sweep actually finishing.)
        if let Some(rx) = &sweep_rx {
            match rx.try_recv() {
                Ok(ledger) => {
                    let freeable_bytes = ledger.root_fs_freeable_bytes();
                    let capacity = ctx.disk.map_or(0, |disk| disk.capacity);
                    if freeable_panel::should_toast(freeable_bytes, capacity) {
                        toasts.push(format!(
                            "{} freeable by closing files — press f",
                            HumanSize(freeable_bytes)
                        ));
                    }
                    ui.set_freeable_ledger(ledger);
                    sweep_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    debug!("freeable sweep thread ended without a result");
                    sweep_rx = None;
                }
            }
        }

        // 2.6. Filter fold (D5): debounced trigger, then a non-blocking
        // poll of whatever is currently in flight — same shape as the
        // freeable sweep just above, applied to the query engine instead.
        maybe_spawn_filter_fold(&phase, &mut ui, &mut palette_rt, &ctx.flat_config);
        poll_filter_fold(&mut ui, &mut palette_rt);

        // 3. Snapshot for this frame (wait-free).
        match &phase {
            Phase::Scanning(_) => {
                ui.apply_snapshot(bus.load());
                // D2: the live accumulator's provisional flat-view summary
                // rides the same arc-swap cadence as the tree snapshot —
                // `None` when the scan didn't enable `with_flat` at all.
                if let Some(flat) = bus.load_flat() {
                    ui.set_flat_summary(flat);
                }
            }
            Phase::Done(_) => {
                if let Some(dir) = ui.pending_nav() {
                    serve_local(&phase, dir, &mut local_generation, &mut ui);
                }
            }
            Phase::Transitioning => unreachable!("resolved in step 2"),
        }

        // 3.5. Flat/breakdown freshness (D2/D3, attack finding 1):
        // render-time epoch check, not "on first t/b" — a delete performed
        // from *within* a flat/breakdown mode must never leave a stale,
        // already-deleted row on screen past this very frame.
        ensure_flat_summary_fresh(&phase, &ctx.flat_config, &mut ui);

        // 4. Render.
        table_state.select((ui.row_count() > 0).then_some(ui.cursor()));
        let spinner = spinner_frame(ctx, started.elapsed());
        let flash_text = flash.current().map(str::to_owned);
        // `is_empty` skips the prune-and-collect work on the (overwhelmingly
        // common) frame where nothing has ever been pushed.
        let toast_texts: Vec<String> = if toasts.is_empty() {
            Vec::new()
        } else {
            toasts
                .active()
                .iter()
                .map(|toast| toast.message.clone())
                .collect()
        };
        let mut geometry = FrameGeometry::default();
        terminal.draw(|frame| {
            geometry = draw(
                frame,
                &ui,
                &phase,
                &mut table_state,
                spinner,
                flash_text.as_deref(),
                &toast_texts,
                &mut motion,
                ctx,
                palette_rt.fold_rx.is_some(),
                &palette_rt.saved_queries,
            );
        })?;
        // Recomputed every frame (design slice 3): mouse events hit-test
        // against exactly what is on screen right now.
        if let Some(total_rows) = geometry.freeable_rows {
            // Same feedback idiom as `set_geometry` itself: the freeable
            // panel's true row count is only known once actually drawn, so
            // the scroll cursor is reined in here rather than at every
            // keypress (see `UiState::clamp_freeable_cursor`).
            ui.clamp_freeable_cursor(total_rows);
        }
        ui.set_geometry(geometry);
    }
}

/// Spawn the scan-end `/proc` sweep (D4) off the UI thread, scoped to
/// `root_dev`. The sweep is plain data in, plain [`Ledger`] out (~25ms
/// measured cost, see the `freeable` module doc) — a bare
/// [`thread::Builder`] + a one-shot channel is the simplest fit, rather
/// than routing it through the scan's own snapshot/bus machinery (which
/// exists for the very different job of many incremental updates from a
/// long-lived scan owner). `try_recv` in the render loop's own poll keeps
/// this from ever blocking a frame. Returns `None` (logged, never fatal)
/// if the thread could not be spawned at all — the session simply has no
/// freeable data this run, same as `--no-proc-sweep`.
fn spawn_freeable_sweep(root_dev: u64) -> Option<Receiver<Ledger>> {
    let (tx, rx) = mpsc::channel();
    match thread::Builder::new()
        .name("freeable-sweep".to_owned())
        .spawn(move || {
            let ledger = freeable::sweep(root_dev);
            // The receiver may already be gone (process exiting); a failed
            // send just means nobody is listening anymore.
            let _ = tx.send(ledger);
        }) {
        Ok(_handle) => Some(rx),
        Err(err) => {
            warn!(%err, "failed to spawn the freeable sweep thread; no freeable data this session");
            None
        }
    }
}

/// Whether the render loop needs to keep polling at [`FRAME`] cadence
/// even without new input: a running scan (progress arrives off the
/// input stream), an in-flight bar/donut animation, a toast/flash that
/// still needs to expire on schedule, a freeable sweep whose result
/// hasn't landed yet (D4 — `sweep_rx` is `Some` from scan end until
/// `try_recv` succeeds), or the filter palette waiting out its debounce
/// window / a fold already in flight (D5). `false` means nothing on
/// screen changes until the user does something, so the loop idles at
/// [`IDLE_POLL`] instead (design slice 5).
fn needs_frequent_polling(
    phase: &Phase,
    flash: &Flash,
    toasts: &ToastQueue,
    motion: &anim::Motion,
    sweep_rx: &Option<Receiver<Ledger>>,
    palette_rt: &PaletteRuntime,
) -> bool {
    matches!(phase, Phase::Scanning(_))
        || motion.is_active()
        || flash.is_set()
        || !toasts.is_empty()
        || sweep_rx.is_some()
        || palette_rt.last_edit.is_some()
        || palette_rt.fold_rx.is_some()
}

// ---------------------------------------------------------------------------
// Filter fold (D5): off-UI-thread, debounced, epoch-guarded
// ---------------------------------------------------------------------------

/// Debounce window (D5, D6: "~100 ms"): a query fold is requested only
/// once this long has passed since the palette buffer last changed in
/// query mode, so a fast typist never triggers one fold per keystroke.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(100);

/// One completed background fold, carried back with the query text it was
/// computed for (the text becomes the pill's label and the history
/// entry — the [`FilterResult`] itself only carries a fingerprint, not the
/// original text).
struct FilterFoldOutcome {
    query_text: String,
    result: FilterResult,
}

/// The palette's clock/IO-owning runtime: everything [`palette::PaletteState`]
/// and [`state::UiState`] deliberately stay free of (see their module
/// docs) — the debounce timer, the in-flight fold's channel and its
/// (fingerprint, epoch) identity, the lazily-built/cached
/// [`HardlinkIndex`] (D3: built on first filter use, rebuilt only when the
/// deletion epoch moves), and the history/saved-queries lists themselves.
/// One instance lives for the whole interactive session, exactly like
/// `sweep_rx`/`Flash`/`ToastQueue`.
struct PaletteRuntime {
    /// Set on every palette-buffer edit in query mode; cleared once a
    /// fold has been requested for the resulting text. `None` means
    /// nothing is waiting on the debounce.
    last_edit: Option<Instant>,
    /// (query fingerprint, deletion epoch) of the fold currently
    /// requested/in-flight, if any.
    in_flight: Option<(u64, u64)>,
    fold_rx: Option<Receiver<FilterFoldOutcome>>,
    hardlink_cache: Option<(u64, Arc<HardlinkIndex>)>,
    history: Vec<String>,
    history_path: Option<PathBuf>,
    saved_queries: Vec<(String, String)>,
}

impl PaletteRuntime {
    fn new(saved_queries: Vec<(String, String)>) -> Self {
        let history_path = history::history_path();
        let history = history_path
            .as_deref()
            .map(history::load)
            .unwrap_or_default();
        Self {
            last_edit: None,
            in_flight: None,
            fold_rx: None,
            hardlink_cache: None,
            history,
            history_path,
            saved_queries,
        }
    }

    /// Record that the query committed (Enter, or the palette closing with
    /// a nonempty query) so Up/Down can recall it later; best-effort
    /// persisted to the history file (never fatal, see `history`'s module
    /// doc).
    fn record_history(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_owned());
        }
        if let Some(path) = &self.history_path {
            history::append(path, text);
        }
    }
}

/// "Now" in unix seconds for [`ApplyOptions::now_unix`] — read once per
/// fold request, never inside the fold itself (D5: reproducible results).
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Worker threads for the candidate fold ([`ApplyOptions::threads`]):
/// the machine's parallelism, same idea as the scan's own auto thread
/// count — [`query::apply`] produces an identical result for any count.
fn default_fold_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Off-UI-thread filtered fold (D5): the freeable sweep's spawn+channel
/// idiom, applied to the query engine. The spawned thread holds a
/// *read* lock on the frozen arena for the fold's duration; the sole
/// writer (a confirmed deletion) briefly waits on the same lock instead of
/// racing it — see [`Phase::Done`]'s doc.
fn spawn_filter_fold(
    outcome: Arc<RwLock<ScanOutcome>>,
    hardlinks: Arc<HardlinkIndex>,
    query: Query,
    query_text: String,
    patterns: flat::PatternSet,
    opts: ApplyOptions,
) -> Receiver<FilterFoldOutcome> {
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("filter-fold".to_owned())
        .spawn(move || {
            let guard = match outcome.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let result = query::apply(guard.tree(), &query, &patterns, &hardlinks, &opts);
            drop(guard);
            // The receiver may already be gone (process exiting, or the
            // session moved on) — a failed send just means nobody is
            // listening anymore.
            let _ = tx.send(FilterFoldOutcome { query_text, result });
        });
    if let Err(err) = spawned {
        warn!(%err, "failed to spawn the filter fold thread; filter not applied");
    }
    rx
}

/// Request a fold right now, no debounce (used once the debounce window
/// has actually elapsed, and by `--filter`/`FILTER`'s scan-end pre-apply,
/// which has no keystrokes to debounce in the first place). Builds/rebuilds
/// the cached [`HardlinkIndex`] first when the deletion epoch has moved
/// (D3) — a bounded, infrequent rebuild, never per-keystroke.
fn request_filter_fold(
    phase: &Phase,
    ui: &UiState,
    rt: &mut PaletteRuntime,
    flat_config: &FlatConfig,
    query: Query,
    query_text: String,
) {
    let Phase::Done(lock) = phase else { return };
    let fingerprint = query.fingerprint();
    let epoch = ui.flat_epoch();
    let hardlinks = {
        let guard = match lock.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &rt.hardlink_cache {
            Some((cached_epoch, index)) if *cached_epoch == epoch => Arc::clone(index),
            _ => {
                let index = Arc::new(HardlinkIndex::build(&guard, epoch));
                rt.hardlink_cache = Some((epoch, Arc::clone(&index)));
                index
            }
        }
    };
    let opts = ApplyOptions {
        cap: flat_config.cap,
        epoch,
        now_unix: unix_now(),
        threads: default_fold_threads(),
    };
    let rx = spawn_filter_fold(
        Arc::clone(lock),
        hardlinks,
        query,
        query_text,
        flat_config.patterns.clone(),
        opts,
    );
    rt.fold_rx = Some(rx);
    rt.in_flight = Some((fingerprint, epoch));
    rt.last_edit = None;
}

/// The debounce check (D5/D6, ~100 ms after the last edit): while the
/// palette is open in query mode, post-scan, request a fold once the
/// buffer has been quiet long enough — unless the exact (fingerprint,
/// epoch) is already in flight or already the applied result, in which
/// case there is nothing new to compute.
fn maybe_spawn_filter_fold(
    phase: &Phase,
    ui: &mut UiState,
    rt: &mut PaletteRuntime,
    flat_config: &FlatConfig,
) {
    if !matches!(phase, Phase::Done(_)) {
        return; // D2: post-scan only
    }
    if rt.fold_rx.is_some() {
        return; // one in flight; let it land before requesting another
    }
    let Some(palette) = ui.palette() else {
        return;
    };
    if palette.mode() != PaletteMode::Query {
        return;
    }
    let Some(last_edit) = rt.last_edit else {
        return;
    };
    if last_edit.elapsed() < FILTER_DEBOUNCE {
        return;
    }
    let parsed = palette.parsed();
    let fingerprint = parsed.query.fingerprint();
    let epoch = ui.flat_epoch();
    if rt.in_flight == Some((fingerprint, epoch)) {
        rt.last_edit = None;
        return;
    }
    if let Some(active) = ui.active_filter()
        && active.result.query_hash == fingerprint
        && active.result.epoch == epoch
    {
        rt.last_edit = None; // already showing exactly this
        return;
    }
    let text = palette.text();
    request_filter_fold(phase, ui, rt, flat_config, parsed.query, text);
}

/// Non-blocking poll of whatever fold is currently in flight (D5): a
/// result is only adopted when it is still the one currently wanted
/// (`rt.in_flight` still names its exact (fingerprint, epoch)) *and* the
/// deletion epoch has not moved since it was requested — stale results
/// (superseded by a newer keystroke, or overtaken by a delete) are
/// dropped silently and never rendered.
fn poll_filter_fold(ui: &mut UiState, rt: &mut PaletteRuntime) {
    let Some(rx) = &rt.fold_rx else {
        return;
    };
    match rx.try_recv() {
        Ok(outcome) => {
            rt.fold_rx = None;
            let still_wanted =
                rt.in_flight == Some((outcome.result.query_hash, outcome.result.epoch));
            let epoch_current = ui.flat_epoch() == outcome.result.epoch;
            if still_wanted && epoch_current {
                ui.set_active_filter(outcome.query_text, Arc::new(outcome.result));
            } else {
                debug!("dropped a stale filter fold result");
            }
            rt.in_flight = None;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            rt.fold_rx = None;
            rt.in_flight = None;
        }
    }
}

fn spinner_frame(ctx: &RenderCtx, elapsed: Duration) -> char {
    let tick = (elapsed.as_millis() / 80) as usize;
    if ctx.ascii() {
        SPINNER_ASCII[tick % SPINNER_ASCII.len()]
    } else {
        SPINNER[tick % SPINNER.len()]
    }
}

enum Action {
    Continue,
    Quit,
}

/// Modal precedence: the Ctrl-K/`/` palette (D6) outranks everything,
/// then confirm beats review beats the freeable panel beats the
/// cheatsheet (design slice 4's ladder, D5 extends it) — only one open at
/// a time, keys route to the open one only. Each modal branch below
/// `return`s unconditionally, so the normal-mode match at the bottom is
/// only ever reached with none of them open — which is also what keeps
/// that invariant true: opening a modal from normal mode can never happen
/// while a higher-precedence one is up, and the palette can only ever be
/// opened from normal mode (so it can never coexist with the other four
/// either). `no_proc_sweep` is `--no-proc-sweep`/`NO_PROC_SWEEP` (D7): `D`
/// skips the pre-deletion open-file refresh outright when set.
// Every parameter is an independent per-keypress input (the key itself,
// the UI/phase/generation/flash/toast state it can mutate, and two
// runtime flags/config): same shape as `draw`'s own too-many-arguments
// allowance.
#[allow(clippy::too_many_arguments)]
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    ui: &mut UiState,
    phase: &mut Phase,
    generation: &mut u64,
    flash: &mut Flash,
    toasts: &mut ToastQueue,
    no_proc_sweep: bool,
    palette_rt: &mut PaletteRuntime,
    flat_config: &FlatConfig,
) -> Action {
    // The palette (D6) is the topmost rung: while open, it owns the
    // keyboard except Esc/Enter/arrows/Home/End/Backspace/Delete/Ctrl-C —
    // every other key, `q` included, is a character (attack finding 2).
    if ui.palette_open() {
        return handle_palette_key(
            code,
            modifiers,
            ui,
            phase,
            flash,
            no_proc_sweep,
            palette_rt,
            flat_config,
        );
    }
    // The confirmation modal captures every key: `y` confirms, anything
    // else cancels (Ctrl-C keeps quitting — safety hatch).
    if ui.confirm().is_some() {
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if code == KeyCode::Char('y') {
            execute_deletion(ui, phase, generation, toasts);
        } else {
            ui.cancel_confirm();
        }
        return Action::Continue;
    }
    if ui.review().is_some() {
        match code {
            KeyCode::Down | KeyCode::Char('j') => ui.review_move_down(),
            KeyCode::Up | KeyCode::Char('k') => ui.review_move_up(),
            KeyCode::Char(' ') => ui.unmark_at_review_cursor(),
            // `D` is natural from inside the list too: close it and open
            // the same confirm modal `D` opens from the main view.
            KeyCode::Char('D') => {
                ui.close_review();
                open_delete_confirm(ui, phase, flash, no_proc_sweep);
            }
            KeyCode::Char('v') | KeyCode::Esc => ui.close_review(),
            _ => {}
        }
        return Action::Continue;
    }
    if ui.freeable_open() {
        match code {
            KeyCode::Down | KeyCode::Char('j') => ui.freeable_move_down(),
            KeyCode::Up | KeyCode::Char('k') => ui.freeable_move_up(),
            KeyCode::Char('f') | KeyCode::Esc => ui.close_freeable_panel(),
            _ => {}
        }
        return Action::Continue;
    }
    if ui.cheatsheet_open() {
        if matches!(code, KeyCode::Char('?') | KeyCode::Esc) {
            ui.close_cheatsheet();
        }
        return Action::Continue;
    }
    match code {
        KeyCode::Char('q') => return Action::Quit,
        // Contextual Esc ladder (D3, extended by D6/attack-a finding 12):
        // every modal already returned above, so getting here means none
        // is open — leave a flat/breakdown mode first, else clear an
        // active filter, else quit exactly like `q`. `q` itself always
        // quits regardless of mode or filter (D3: "`q` always quits").
        KeyCode::Esc => {
            if ui.mode() != ViewMode::Tree {
                ui.leave_mode();
            } else if ui.active_filter().is_some() {
                ui.clear_filter();
            } else {
                return Action::Quit;
            }
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Action::Quit,
        // Ctrl-K / `/` (D6): open the palette, pre-filled with the active
        // filter's text when there is one so editing continues naturally
        // instead of starting from an empty box behind its own effect.
        KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
            let prefill = ui.active_filter().map(|f| f.query_text.clone());
            ui.open_palette(prefill.as_deref());
        }
        KeyCode::Char('/') => {
            let prefill = ui.active_filter().map(|f| f.query_text.clone());
            ui.open_palette(prefill.as_deref());
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => activate_selected(ui, phase, flash),
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
            // Ascend is a tree-mode concept only: a flat/breakdown row
            // list has no descend stack of its own to climb.
            if ui.mode() == ViewMode::Tree {
                try_ascend(ui, phase);
            }
        }
        KeyCode::Char(' ') => try_toggle_mark(ui, phase, flash),
        KeyCode::Char('D') => open_delete_confirm(ui, phase, flash, no_proc_sweep),
        KeyCode::Char('v') => try_open_review(ui, flash),
        KeyCode::Char('f') => open_freeable_panel(ui, phase),
        // The sort keys are mode-aware (D3: a group total has no mtime or
        // error count) and need the flash queue to say so when refused —
        // context the stateless `keymap::SIMPLE` table doesn't carry (see
        // its module doc), so they are hand-written here instead of
        // dispatched through it.
        KeyCode::Char('d') => try_sort(ui, flash, SortKey::Disk),
        KeyCode::Char('a') => try_sort(ui, flash, SortKey::Apparent),
        KeyCode::Char('n') => try_sort(ui, flash, SortKey::Name),
        KeyCode::Char('m') => try_sort(ui, flash, SortKey::Mtime),
        KeyCode::Char('c') => try_sort(ui, flash, SortKey::Items),
        KeyCode::Char('e') => try_sort(ui, flash, SortKey::Errors),
        // Every other key (movement, `p`, `u`, `?`, `t`, `b`, `z`) is
        // stateless enough to live in the keymap dispatch table
        // (`ui::keymap`) — the single source the `?` cheatsheet also
        // renders from.
        _ => {
            keymap::dispatch_simple(code, ui);
        }
    }
    Action::Continue
}

/// Route one keypress to the open palette (D6). Only Esc, Enter, the
/// arrows/Home/End/Backspace/Delete and Ctrl-C are interpreted; every
/// other key is a character inserted at the cursor — `q` included (attack
/// finding 2: `q`-always-quits is suspended the instant the palette has
/// focus, and resumes the instant it closes). Mouse stays inert except
/// click-outside-closes, handled in `handle_mouse`.
#[allow(clippy::too_many_arguments)]
fn handle_palette_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    ui: &mut UiState,
    phase: &mut Phase,
    flash: &mut Flash,
    no_proc_sweep: bool,
    rt: &mut PaletteRuntime,
    flat_config: &FlatConfig,
) -> Action {
    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit; // the one safety hatch that survives inside the palette
    }
    match code {
        // Esc closes the palette only — it must never also clear the
        // filter (attack finding 12's off-by-one): whatever was last
        // applied while typing stays active.
        KeyCode::Esc => ui.close_palette(),
        KeyCode::Enter => return palette_enter(ui, phase, flash, no_proc_sweep, rt, flat_config),
        KeyCode::Left => {
            if let Some(p) = ui.palette_mut() {
                p.move_left();
            }
        }
        KeyCode::Right => {
            if let Some(p) = ui.palette_mut() {
                p.move_right();
            }
        }
        KeyCode::Home => {
            if let Some(p) = ui.palette_mut() {
                p.move_home();
            }
        }
        KeyCode::End => {
            if let Some(p) = ui.palette_mut() {
                p.move_end();
            }
        }
        KeyCode::Backspace => {
            if let Some(p) = ui.palette_mut() {
                p.backspace();
            }
            rt.last_edit = Some(Instant::now());
        }
        KeyCode::Delete => {
            if let Some(p) = ui.palette_mut() {
                p.delete_forward();
            }
            rt.last_edit = Some(Instant::now());
        }
        KeyCode::Up => palette_move_selection(ui, rt, true),
        KeyCode::Down => palette_move_selection(ui, rt, false),
        KeyCode::Char(ch) => {
            if let Some(p) = ui.palette_mut() {
                p.insert_char(ch);
            }
            rt.last_edit = Some(Instant::now());
        }
        _ => {}
    }
    Action::Continue
}

/// Up/Down inside the palette: command-mode selection, saved-query
/// selection (query mode, empty buffer, D6), or history navigation (query
/// mode, nonempty buffer) — three different lists sharing one key, none
/// of them live at once.
fn palette_move_selection(ui: &mut UiState, rt: &PaletteRuntime, up: bool) {
    let Some(mode) = ui.palette().map(palette::PaletteState::mode) else {
        return;
    };
    match mode {
        PaletteMode::Command => {
            let content = ui
                .palette()
                .map(palette::PaletteState::content)
                .unwrap_or_default();
            let commands = palette::all_commands();
            let len = palette::filter_commands(&content, &commands).len();
            if let Some(p) = ui.palette_mut() {
                if up {
                    p.move_selection_up();
                } else {
                    p.move_selection_down(len);
                }
            }
        }
        PaletteMode::Query => {
            let empty = ui.palette().map(|p| p.text().is_empty()).unwrap_or(true);
            if empty {
                let len = rt.saved_queries.len();
                if let Some(p) = ui.palette_mut() {
                    if up {
                        p.move_selection_up();
                    } else {
                        p.move_selection_down(len);
                    }
                }
            } else if let Some(p) = ui.palette_mut() {
                if up {
                    p.history_up(&rt.history);
                } else {
                    p.history_down(&rt.history);
                }
            }
        }
    }
}

/// Enter inside the palette: commit. Command mode runs the
/// fuzzy-selected command; query mode applies the typed text (or, on an
/// empty buffer, the selected saved query, D6) immediately — bypassing the
/// debounce, since an explicit Enter is exactly the "the user is done
/// typing" signal the debounce exists to infer from silence — and closes
/// the palette either way, leaving whatever filter is now active in
/// place.
fn palette_enter(
    ui: &mut UiState,
    phase: &mut Phase,
    flash: &mut Flash,
    no_proc_sweep: bool,
    rt: &mut PaletteRuntime,
    flat_config: &FlatConfig,
) -> Action {
    let Some(mode) = ui.palette().map(palette::PaletteState::mode) else {
        return Action::Continue;
    };
    match mode {
        PaletteMode::Command => {
            let content = ui
                .palette()
                .map(palette::PaletteState::content)
                .unwrap_or_default();
            let commands = palette::all_commands();
            let filtered = palette::filter_commands(&content, &commands);
            let selected = ui
                .palette()
                .map(palette::PaletteState::list_selected)
                .unwrap_or(0);
            let action = filtered.get(selected).map(|&i| commands[i].action);
            ui.close_palette();
            if let Some(action) = action {
                return execute_command(action, ui, phase, flash, no_proc_sweep);
            }
        }
        PaletteMode::Query => {
            let text = ui
                .palette()
                .map(palette::PaletteState::text)
                .unwrap_or_default();
            let final_text = if text.trim().is_empty() {
                let selected = ui
                    .palette()
                    .map(palette::PaletteState::list_selected)
                    .unwrap_or(0);
                rt.saved_queries.get(selected).map(|(_, q)| q.clone())
            } else {
                Some(text)
            };
            ui.close_palette();
            if let Some(text) = final_text
                && !text.trim().is_empty()
            {
                let parsed = query::parse(&text);
                rt.record_history(&text);
                request_filter_fold(phase, ui, rt, flat_config, parsed.query, text);
            }
        }
    }
    Action::Continue
}

/// Run one palette command (D6): [`CommandAction::Simple`] dispatches
/// exactly like `keymap::dispatch_simple`; the rest reuse the same
/// hand-written helpers the equivalent keypress already calls, so a
/// command can never do something its keyboard equivalent wouldn't.
fn execute_command(
    action: CommandAction,
    ui: &mut UiState,
    phase: &mut Phase,
    flash: &mut Flash,
    no_proc_sweep: bool,
) -> Action {
    match action {
        CommandAction::Simple(apply) => apply(ui),
        CommandAction::ReviewMarks => try_open_review(ui, flash),
        CommandAction::DeleteMarked => open_delete_confirm(ui, phase, flash, no_proc_sweep),
        CommandAction::FreeablePanel => open_freeable_panel(ui, phase),
        CommandAction::ClearFilter => ui.clear_filter(),
        CommandAction::Quit => return Action::Quit,
    }
    Action::Continue
}

/// Start descending into the directory under the cursor — shared by the
/// keyboard (`Enter`/`l`/`Right`) and every mouse action that opens a row
/// (double-click, click-on-already-selected, donut slice).
fn try_descend(ui: &mut UiState, phase: &Phase) {
    if let Some(dir) = ui.descend() {
        request_nav(phase, dir);
    }
}

/// Start going up to the parent — shared by the keyboard
/// (`Backspace`/`h`/`Left`) and breadcrumb clicks.
fn try_ascend(ui: &mut UiState, phase: &Phase) {
    if let Some(dir) = ui.ascend() {
        request_nav(phase, dir);
    }
}

/// Activate the row under the cursor: descend in tree mode, jump to the
/// containing directory on a flat row, flash the phase-1 no-op on a
/// breakdown row (D3). Shared by the keyboard (`Enter`/`l`/`Right`) and
/// every mouse action that opens a row (double-click,
/// click-on-already-selected, donut slice) — the same idiom `try_descend`
/// already was for tree mode alone.
fn activate_selected(ui: &mut UiState, phase: &Phase, flash: &mut Flash) {
    match ui.mode() {
        ViewMode::Tree => try_descend(ui, phase),
        ViewMode::FlatTop => try_jump_flat_row(ui, phase, flash),
        ViewMode::Breakdown => flash.set(BREAKDOWN_DRILLDOWN_LOCKED),
    }
}

/// Enter on a flat top-files row (D3): jump to its containing directory in
/// tree view, cursor on the file itself. Only possible post-scan —
/// resolving a containing directory needs a real path, and the live
/// accumulator's `TopFile` (denormalized name aside, see `flatview`'s
/// module doc) has no path to give one from, so mid-scan this flashes the
/// same "available once the scan completes" note marking already uses.
fn try_jump_flat_row(ui: &mut UiState, phase: &Phase, flash: &mut Flash) {
    let Phase::Done(lock) = phase else {
        flash.set(FLAT_ROW_DETAILS_LOCKED);
        return;
    };
    let guard = read_outcome(lock);
    let outcome = &*guard;
    let Some(summary) = ui.flat_summary() else {
        return;
    };
    let rows = flatview::flat_rows(summary, Some(outcome));
    let order = flatview::sort_flat_rows(&rows, ui.sort());
    let Some(&index) = order.get(ui.cursor()) else {
        return; // empty view: silent no-op
    };
    let node = rows[index].node;
    let tree = outcome.tree();
    let Some(dir) = tree.dir_of(tree.node(node).parent()) else {
        // Every live file's parent is a scanned directory with a DirId;
        // this would only miss on a data-model bug, never a user action.
        warn!(
            ?node,
            "flat row's parent has no directory metadata: cannot jump"
        );
        return;
    };
    let ancestors = ancestor_chain(tree, dir);
    if let Some(target) = ui.jump_to_directory(dir, ancestors, node) {
        request_nav(phase, target);
        ui.leave_mode();
    }
}

/// Root-first chain of ancestor directories *above* `dir` (excluding
/// `dir` itself) — what [`UiState::jump_to_directory`] needs to rebuild
/// the breadcrumb stack for a directory reached directly (not via
/// `descend`/`ascend`), same shape as [`UiState::stack_dirs`].
fn ancestor_chain(tree: &Tree, dir: DirId) -> Vec<DirId> {
    let mut chain = Vec::new();
    let mut current = dir;
    while let Some(parent) = tree.dir(current).parent {
        chain.push(parent);
        current = parent;
    }
    chain.reverse();
    chain
}

/// Sort keypress (`d`/`a`/`n`/`m`/`c`/`e`, D3): refused with a flash when
/// the active mode has no meaningful column for the key (a group total
/// has no mtime; a single top file has no subtree item count) — hand
/// -written rather than in `keymap::SIMPLE` because the refusal needs the
/// flash queue, context that table doesn't carry (see its module doc).
fn try_sort(ui: &mut UiState, flash: &mut Flash, key: SortKey) {
    let supported = match ui.mode() {
        ViewMode::Tree => true,
        ViewMode::FlatTop => flatview::flat_supports_sort(key),
        ViewMode::Breakdown => flatview::breakdown_supports_sort(key),
    };
    if !supported {
        flash.set(SORT_NOT_APPLICABLE);
        return;
    }
    ui.press_sort(key);
}

/// Route a mouse event against the last frame's [`FrameGeometry`] (mouse
/// support, design slice 3). Inert while any modal is open — confirm,
/// review, the freeable panel (D5) or cheatsheet (design slice 4) — they
/// only listen to the keyboard; a click through to a hidden row
/// underneath would be surprising. The palette (D6) is almost the same,
/// with one exception: any click closes it (there is no inner click
/// target to route to — no click-to-position-the-cursor, no
/// click-a-list-row — so every click is definitionally "outside").
fn handle_mouse(
    mouse: MouseEvent,
    ui: &mut UiState,
    phase: &Phase,
    last_click: &mut Option<(Instant, u16, u16)>,
    flash: &mut Flash,
) {
    if ui.palette_open() {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            ui.close_palette();
        }
        return;
    }
    if ui.confirm().is_some() || ui.review().is_some() || ui.freeable_open() || ui.cheatsheet_open()
    {
        return;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            handle_click(mouse.column, mouse.row, ui, phase, last_click, flash);
        }
        MouseEventKind::Moved => handle_hover(mouse.column, mouse.row, ui),
        MouseEventKind::ScrollDown if over_table(mouse.column, mouse.row, ui) => {
            for _ in 0..SCROLL_STEP {
                ui.move_down();
            }
        }
        MouseEventKind::ScrollUp if over_table(mouse.column, mouse.row, ui) => {
            for _ in 0..SCROLL_STEP {
                ui.move_up();
            }
        }
        _ => {}
    }
}

fn over_table(col: u16, row: u16, ui: &UiState) -> bool {
    ui.geometry()
        .table
        .as_ref()
        .is_some_and(|table| table.hit_test(col, row).is_some())
}

/// Left click: errors card toggles sort-by-errors, a breadcrumb segment
/// jumps to that ancestor, a donut slice navigates straight into that
/// child, and a table row selects it — or, matching the keyboard's
/// descend key, navigates into it when the click double-clicks the same
/// cell or lands on the row already under the cursor.
fn handle_click(
    col: u16,
    row: u16,
    ui: &mut UiState,
    phase: &Phase,
    last_click: &mut Option<(Instant, u16, u16)>,
    flash: &mut Flash,
) {
    if ui.geometry().gauge_freeable_hit(col, row) {
        open_freeable_panel(ui, phase);
        *last_click = None;
        return;
    }
    if ui.geometry().errors_card_hit(col, row) {
        try_sort(ui, flash, SortKey::Errors);
        *last_click = None;
        return;
    }
    if let Some(dir) = ui.geometry().breadcrumb_hit(col, row) {
        try_ascend_to(ui, phase, dir);
        *last_click = None;
        return;
    }
    let wheel_target = ui
        .geometry()
        .wheel
        .as_ref()
        .and_then(|w| w.hit_test(col, row));
    if let Some(position) = wheel_target {
        // A slice already summarizes an entire child: click always
        // activates, there is no separate "select" step.
        ui.select_at(position);
        activate_selected(ui, phase, flash);
        *last_click = None;
        return;
    }
    let table_target = ui
        .geometry()
        .table
        .as_ref()
        .and_then(|table| table.hit_test(col, row));
    let Some(position) = table_target else {
        *last_click = None;
        return;
    };
    let already_selected = ui.row_count() > 0 && ui.cursor() == position;
    let double_click = matches!(*last_click, Some((at, c, r)) if c == col && r == row && at.elapsed() < DOUBLE_CLICK);
    ui.select_at(position);
    if already_selected || double_click {
        activate_selected(ui, phase, flash);
        *last_click = None;
    } else {
        *last_click = Some((Instant::now(), col, row));
    }
}

/// Breadcrumb click: jump straight to the ancestor `dir`, in one request.
fn try_ascend_to(ui: &mut UiState, phase: &Phase, dir: DirId) {
    if let Some(target) = ui.jump_to_ancestor(dir) {
        request_nav(phase, target);
    }
}

/// Mouse moved over the table without clicking: the selection card
/// follows the hovered row until the mouse leaves the table or a
/// keyboard action reclaims it.
fn handle_hover(col: u16, row: u16, ui: &mut UiState) {
    match ui
        .geometry()
        .table
        .as_ref()
        .and_then(|table| table.hit_test(col, row))
    {
        Some(position) if position < ui.row_count() => ui.set_hover(position),
        _ => ui.clear_hover(),
    }
}

/// `Space`: mark/unmark the row under the cursor. Inactive during the
/// scan (HANDOFF §5: deletion only works on the frozen post-scan arena).
/// Mode-aware (D3): flat rows mark real nodes into the same shared basket;
/// breakdown rows aren't markable at all (a group isn't a single node —
/// group-level marking is a deliberate fast-follow, D6).
fn try_toggle_mark(ui: &mut UiState, phase: &Phase, flash: &mut Flash) {
    if matches!(phase, Phase::Scanning(_)) {
        flash.set(DELETION_LOCKED);
        return;
    }
    match ui.mode() {
        ViewMode::Tree => match ui.toggle_mark() {
            Ok(()) => {}
            Err(MarkRefusal::ScanRunning) => flash.set(DELETION_LOCKED),
            Err(MarkRefusal::MountPoint) => {
                flash.set("mount points cannot be marked for deletion");
            }
            Err(MarkRefusal::FilterActive) => flash.set(DIR_MARK_FILTER_LOCKED),
        },
        ViewMode::FlatTop => try_toggle_mark_flat(ui, phase, flash),
        ViewMode::Breakdown => flash.set("marking is not available in the breakdown view"),
    }
}

/// `Space` on a flat top-files row (D3): resolve the row's real
/// `NodeId`/path/disk size from the frozen arena and mark/unmark it in the
/// same shared basket tree-mode marking uses. Only possible post-scan —
/// same reason as [`try_jump_flat_row`] (the live accumulator's
/// `TopFile` has no path to resolve).
fn try_toggle_mark_flat(ui: &mut UiState, phase: &Phase, flash: &mut Flash) {
    let Phase::Done(lock) = phase else {
        flash.set(DELETION_LOCKED);
        return;
    };
    let guard = read_outcome(lock);
    let outcome = &*guard;
    let Some(summary) = ui.flat_summary() else {
        return;
    };
    let rows = flatview::flat_rows(summary, Some(outcome));
    let order = flatview::sort_flat_rows(&rows, ui.sort());
    let Some(&index) = order.get(ui.cursor()) else {
        return; // empty view: silent no-op, matching tree mode
    };
    let row = &rows[index];
    let path = row
        .path
        .clone()
        .unwrap_or_else(|| outcome.tree().path_of_node(row.node));
    match ui.toggle_mark_flat(row.node, path, row.disk) {
        Ok(()) => {}
        Err(MarkRefusal::ScanRunning) => flash.set(DELETION_LOCKED),
        Err(MarkRefusal::MountPoint) => {
            debug!("unreachable: flat rows are always regular files, never mount points");
        }
        Err(MarkRefusal::FilterActive) => {
            debug!("unreachable: flat rows are always regular files, never refused by a filter");
        }
    }
}

/// `v`: open the review list over the marked entries. Refused (flashed,
/// like `D`'s own "nothing marked") when the basket is empty — there is
/// nothing to review, and an empty modal would just be confusing.
fn try_open_review(ui: &mut UiState, flash: &mut Flash) {
    if !ui.open_review() {
        flash.set("nothing marked — Space marks the row under the cursor");
    }
}

/// `D`: open the confirmation modal over the marked entries, computing the
/// hardlink warning from the frozen arena and, unless `no_proc_sweep` (D7),
/// the D6 pre-deletion open-file advisory.
fn open_delete_confirm(ui: &mut UiState, phase: &Phase, flash: &mut Flash, no_proc_sweep: bool) {
    let Phase::Done(lock) = phase else {
        flash.set(DELETION_LOCKED);
        return;
    };
    let guard = read_outcome(lock);
    let outcome = &*guard;
    if ui.marked_summary().is_none() {
        flash.set("nothing marked — Space marks the row under the cursor");
        return;
    }
    let nodes: Vec<NodeId> = ui.marks().iter().map(|mark| mark.node).collect();
    let hardlinks = delete::hardlink_files_in(outcome, &nodes);
    let open_warning = if no_proc_sweep {
        None
    } else {
        pre_deletion_open_warning(ui)
    };
    ui.open_confirm(hardlinks, open_warning);
}

/// D6: refresh the open-file index (unfiltered sweep, same ~25ms cost as
/// the scan-end one) and match it against the marked selection two ways:
///
/// - **marked files**: a fresh `symlink_metadata` supplies each marked
///   file's own `(dev, ino)`, looked up directly against the index. Tree
///   nodes don't carry `(dev, ino)` past scan time (D8 keeps `tree.rs`
///   untouched, and `Node` is a packed 32 bytes with no room for it), so
///   this mirrors the exact "fresh look, without following a symlink"
///   guard [`camembert_core::delete`] already takes before touching disk.
/// - **marked directories** (D6 amendment): a directory has no single
///   inode whose openness would mean anything about its *contents*, so
///   instead every indexed open file's evidence path
///   ([`freeable::OpenFileIndex::iter`]) is checked for path-prefix
///   containment under the marked directory — the same
///   [`freeable_panel::is_path_prefix`] rule the panel's ancestor grouping
///   already established. This is the primary real-world scenario a
///   files-only check would miss entirely: marking a data directory (say
///   a database's) whose individual files are what's actually held open,
///   which is exactly the false reassurance D6 forbids.
///
/// Holders from both channels are deduplicated by pid in
/// [`freeable_panel::build_open_warning`]. Cost stays bounded by
/// process/fd counts (~25ms), not tree size: the containment check is a
/// linear scan of the index (a few thousand short paths at most), never a
/// syscall-per-descendant walk of an arbitrarily large marked directory.
/// Synchronous: a modal open blocking for one sweep's worth of time
/// (~25ms, well under the ~50ms a UI action can eat before feeling laggy)
/// is a fair trade against threading a second off-thread machine through
/// the UI for what is otherwise a one-shot, on-demand check.
fn pre_deletion_open_warning(ui: &UiState) -> Option<freeable_panel::OpenWarning> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let index = freeable::open_file_index();

    let marked_file_lookups: Vec<Option<Vec<freeable::Holder>>> = ui
        .marks()
        .iter()
        .filter(|mark| !mark.is_dir)
        .map(|mark| {
            std::fs::symlink_metadata(&mark.path)
                .ok()
                .and_then(|meta| index.holders(meta.dev(), meta.ino()).map(|s| s.to_vec()))
        })
        .collect();

    let marked_dirs: Vec<Vec<u8>> = ui
        .marks()
        .iter()
        .filter(|mark| mark.is_dir)
        .map(|mark| mark.path.as_os_str().as_bytes().to_vec())
        .collect();
    let contained_holders: Vec<Vec<freeable::Holder>> = if marked_dirs.is_empty() {
        Vec::new()
    } else {
        index
            .iter()
            .filter(|&(evidence, _, _, _)| {
                marked_dirs.iter().any(|dir| {
                    // Strictly *under* the directory, not the directory's
                    // own inode (that would mean someone's cwd is there,
                    // not a file inside it) — `is_path_prefix` allows an
                    // exact-length match, so that case is excluded here.
                    freeable_panel::is_path_prefix(evidence, dir) && evidence.len() > dir.len()
                })
            })
            .map(|(_, _, _, holders)| holders.to_vec())
            .collect()
    };

    freeable_panel::build_open_warning(&marked_file_lookups, &contained_holders, index.coverage())
}

/// `f` or a gauge-suffix click: open the freeable panel (D5), building and
/// caching the display grouping (D5: longest-prefix match against the
/// frozen tree's live directory paths) the first time it's needed for the
/// current ledger. Always succeeds, even with no ledger yet — the panel
/// then shows an explanatory empty state rather than refusing to open.
fn open_freeable_panel(ui: &mut UiState, phase: &Phase) {
    if !ui.freeable_groups_built()
        && let Some(ledger) = ui.freeable_ledger()
    {
        // Cloned out to end the borrow before `set_freeable_groups` needs
        // `&mut ui` — the ledger's root-fs entries are typically few
        // enough (deleted-open files, not the whole tree) for this to be
        // cheap.
        let entries = ledger.root_fs_entries().to_vec();
        let ancestors = match phase {
            Phase::Done(lock) => live_dir_paths(&read_outcome(lock)),
            _ => Vec::new(),
        };
        let groups = freeable_panel::group_by_ancestor(&entries, &ancestors);
        ui.set_freeable_groups(groups);
    }
    ui.open_freeable_panel();
}

/// Every still-existing directory's full path, as raw bytes — the
/// candidate ancestors [`freeable_panel::group_by_ancestor`] longest-prefix
/// matches evidence paths against (D5). An in-memory walk of the frozen
/// arena (no syscalls), the same kind of one-time synchronous cost
/// `delete::hardlink_files_in` already pays for the confirm modal's
/// hardlink warning.
fn live_dir_paths(outcome: &ScanOutcome) -> Vec<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let tree = outcome.tree();
    tree.dir_ids()
        .filter(|&dir| !tree.is_removed(tree.dir(dir).node))
        .map(|dir| tree.path_of(dir).as_os_str().as_bytes().to_vec())
        .collect()
}

/// Modal confirmed (`y`): delete the marked entries from disk (all guards
/// in [`camembert_core::delete`]), update the tree's accounting, and
/// re-view from the nearest surviving directory. The result is a toast
/// (design slice 4), not a footer flash — "deletion done" is exactly the
/// kind of announcement-of-something-that-happened the toast mechanism
/// exists for, see the `toast` module doc for the split.
fn execute_deletion(
    ui: &mut UiState,
    phase: &mut Phase,
    generation: &mut u64,
    toasts: &mut ToastQueue,
) {
    let Phase::Done(lock) = phase else {
        // The modal only opens post-scan, but never delete on a stale
        // assumption.
        ui.cancel_confirm();
        return;
    };
    let Some(marks) = ui.take_confirmed_marks() else {
        return;
    };
    let nodes: Vec<NodeId> = marks.iter().map(|mark| mark.node).collect();
    info!(count = nodes.len(), "deletion confirmed");
    // The viewed directory may sit inside a deleted subtree: climb to the
    // nearest surviving ancestor before rebuilding the view.
    let mut dir = ui.snapshot().dir;
    {
        // The write lock (D5: the sole writer against `Phase::Done`'s
        // shared arena — see its doc) is scoped to this block and dropped
        // before `serve_local` below re-acquires the *read* side; an
        // in-flight background filter fold simply waited its turn on the
        // same lock, never raced it.
        let mut guard = write_outcome(lock);
        let outcome = &mut *guard;
        let report = delete::delete_nodes(outcome, &nodes);
        if report.deleted > 0 {
            // D2/D3, attack finding 1: advance the epoch so the very next
            // render-time check (`ensure_flat_summary_fresh`) recomputes
            // the flat/breakdown summary before drawing, and so the very
            // next filter-fold poll (D5) discards any in-flight result
            // computed against the arena as it stood before this delete —
            // regardless of which mode/surface this deletion came from.
            ui.bump_flat_epoch();
        }
        if report.failed > 0 || report.skipped > 0 {
            toasts.push(format!(
                "deleted {} ({} freed), failed {}, skipped {} — see log",
                report.deleted,
                HumanSize(report.freed.real),
                report.failed,
                report.skipped
            ));
        } else {
            toasts.push(format!(
                "deleted {} entries, {} freed",
                report.deleted,
                HumanSize(report.freed.real)
            ));
        }
        let tree = outcome.tree();
        while tree.is_removed(tree.dir(dir).node) {
            dir = tree
                .dir(dir)
                .parent
                .expect("the scan root is never removable");
        }
    }
    serve_local(phase, dir, generation, ui);
}

/// Route a navigation request: over the bus while the owner lives, served
/// locally next frame once the scan is done.
fn request_nav(phase: &Phase, dir: DirId) {
    if let Phase::Scanning(live) = phase {
        live.bus().request(dir);
    }
    // Phase::Done: the frame loop sees pending_nav and serves it.
}

/// Post-scan navigation: build the requested view straight off the frozen
/// arena (same row shape as live snapshots, root_complete stats).
fn serve_local(phase: &Phase, dir: DirId, generation: &mut u64, ui: &mut UiState) {
    let Phase::Done(lock) = phase else {
        return;
    };
    let guard = read_outcome(lock);
    let outcome = &*guard;
    *generation += 1;
    let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
    let snapshot = view::build_snapshot(
        outcome.tree(),
        dir,
        *generation,
        stats,
        outcome.hardlink_inodes,
        false,
    );
    ui.apply_snapshot(Arc::new(snapshot));
}

/// Recompute the flat/breakdown summary on a render-time epoch mismatch
/// (D2/D3, attack finding 1) — called once per frame, right before
/// drawing, regardless of which mode is active: cheap to check (two field
/// reads), and checking unconditionally is what makes the very next frame
/// after a delete honest even though the delete itself may have happened
/// while flat/breakdown mode was already open (the deleted row must never
/// render as still occupying space).
///
/// Mid-scan this is a no-op (the accumulator's provisional summary is
/// already fresh every frame via `bus.load_flat()`, see `event_loop` step
/// 3, and marks/deletes don't exist yet to bump the epoch). Post-scan, a
/// stale or absent summary — no cache yet, the cached one is still the
/// scan-end provisional hand-off, or its `epoch` disagrees with
/// [`UiState::flat_epoch`] — triggers one authoritative
/// [`flat::fold`] over the frozen arena.
fn ensure_flat_summary_fresh(phase: &Phase, flat_config: &FlatConfig, ui: &mut UiState) {
    let Phase::Done(lock) = phase else {
        return;
    };
    let epoch = ui.flat_epoch();
    let stale = ui
        .flat_summary()
        .is_none_or(|summary| summary.provisional || summary.epoch != epoch);
    if !stale {
        return;
    }
    let guard = read_outcome(lock);
    let outcome = &*guard;
    let summary = flat::fold(
        outcome.tree(),
        &flat_config.patterns,
        flat_config.cap,
        epoch,
    );
    ui.set_flat_summary(Arc::new(summary));
}

/// Draws one frame and returns the hit-testing geometry mouse events are
/// matched against next: recomputed every draw so it always describes
/// exactly what is on screen (design slice 3).
// Every parameter is an independent per-frame input (frame buffer,
// immutable UI state, table scroll state, spinner phase, two transient
// overlay contents, animation state, render context) with no natural
// subgroup to bundle without inventing an arbitrary struct just to
// satisfy the lint — same call already made for
// `ScanOutcome::from_tree` in camembert-core.
#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut Frame<'_>,
    ui: &UiState,
    phase: &Phase,
    table_state: &mut TableState,
    spinner: char,
    flash: Option<&str>,
    toasts: &[String],
    motion: &mut anim::Motion,
    ctx: &RenderCtx,
    filter_fold_in_flight: bool,
    saved_queries: &[(String, String)],
) -> FrameGeometry {
    // Once per frame: a navigation/sort since the last frame starts a
    // fresh animation window (design slice 5) — see the `anim` module.
    motion.observe(ui.view_change_seq());

    let snapshot = ui.snapshot();
    // The basket strip (design slice 4) only takes a row while something
    // is marked — `Length(0)` otherwise, so browsing without ever marking
    // anything never sees the layout shift, same idea as the selection
    // card below. Zen mode (`z`, design slice 5) collapses the cards row
    // and disk gauge the same way: table + footer + basket strip only.
    let basket_height = if ui.marked_summary().is_some() { 1 } else { 0 };
    // D6: the filter pill takes a row the same way the basket strip does
    // — `Length(0)` while no filter is active, so browsing without ever
    // filtering never sees the layout shift. Sits just above the basket
    // strip (both above the footer); with both active at once the two
    // stack, each keeping its own line.
    let pill_height = if ui.active_filter().is_some() { 1 } else { 0 };
    let (cards_height, gauge_height) = cards_and_gauge_heights(ui.zen());
    let [
        header_area,
        cards_area,
        gauge_area,
        main_area,
        pill_area,
        basket_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(cards_height),
        Constraint::Length(gauge_height),
        Constraint::Min(3),
        Constraint::Length(pill_height),
        Constraint::Length(basket_height),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    // Held for the whole frame (brief, synchronous) — every draw helper
    // below that needs the frozen arena borrows through this one guard
    // rather than each acquiring its own.
    let outcome_guard = match phase {
        Phase::Done(lock) => Some(read_outcome(lock)),
        _ => None,
    };
    let outcome: Option<&ScanOutcome> = outcome_guard.as_deref();

    let breadcrumb = draw_header(frame, header_area, ui, spinner, ctx);
    let errors_card = if ui.zen() {
        None
    } else {
        draw_metric_cards(frame, cards_area, ui, ctx)
    };
    let gauge_freeable = if ui.zen() {
        None
    } else {
        draw_disk_gauge(frame, gauge_area, ui, ctx)
    };

    // Main split: table (with selection card) left, wheel right — see
    // `wheel_layout` for the responsive-collapse/zen-mode rules (design
    // slice 5). The selection card only makes sense over tree rows (mtime/
    // items/share of *this row's parent*); flat/breakdown rows don't carry
    // that shape, so it is hidden in those modes rather than showing
    // something misleading.
    let layout = wheel_layout(frame.area().width, ctx.ascii(), ui.zen());
    let (left_area, wheel_area) = if layout == WheelLayout::Full {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main_area);
        (left, Some(right))
    } else {
        (main_area, None)
    };
    let show_selection_card = !ui.zen() && ui.mode() == ViewMode::Tree && left_area.height >= 9;
    let (table_area, card_area) = if show_selection_card {
        let [table, card] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(left_area);
        (table, Some(card))
    } else {
        (left_area, None)
    };

    let bar_progress = motion.bar_progress();
    // Table and wheel are always built together from the same rows/ranks
    // (D3): tree, flat top files, or the pattern breakdown — so the
    // identity colors and the slice->row click mapping can never disagree
    // between the two, the same guarantee tree mode already relied on.
    let (table, wheel_source) = match ui.mode() {
        ViewMode::Tree => {
            let disks: Vec<u64> = snapshot.rows.iter().map(|row| row.disk).collect();
            let ranks = theme::assign_identity(&disks, theme::IDENTITY_LEN);
            let table = draw_table(
                frame,
                table_area,
                ui,
                outcome,
                table_state,
                spinner,
                &ranks,
                bar_progress,
                ctx,
            );
            // D4 composition: under an active filter, the donut's slices
            // and total follow the same filtered per-directory totals the
            // table rows now show, instead of the raw unfiltered subtree.
            let slice_rows: Vec<(u64, Option<usize>)> = ui
                .rows_indexed()
                .map(|(index, row)| {
                    let disk = match (ui.active_filter(), row.dir) {
                        (Some(filter), Some(dir)) => filter.result.dir_total(dir).disk,
                        _ => row.disk,
                    };
                    (disk, ranks.get(index).copied().flatten())
                })
                .collect();
            let total = ui
                .active_filter()
                .map(|filter| filter.result.dir_total(snapshot.dir).disk)
                .unwrap_or(snapshot.totals.disk);
            let wheel_source = WheelSource {
                slice_rows,
                total,
                caption: snapshot.path.display().to_string(),
            };
            (table, wheel_source)
        }
        ViewMode::FlatTop => {
            let scan_disk_total = ui
                .active_filter()
                .map(|filter| filter.result.matched_disk)
                .unwrap_or(snapshot.stats.disk_bytes);
            draw_flat_table(
                frame,
                table_area,
                ui,
                outcome,
                table_state,
                scan_disk_total,
                bar_progress,
                ctx,
            )
        }
        ViewMode::Breakdown => {
            draw_breakdown_table(frame, table_area, ui, table_state, bar_progress, ctx)
        }
    };
    if let Some(card_area) = card_area {
        draw_selection_card(frame, card_area, ui, ctx);
    }
    if layout == WheelLayout::Mini {
        draw_mini_donut(frame, header_area, &wheel_source, motion, ctx);
    }
    let wheel =
        wheel_area.and_then(|wheel_area| draw_wheel(frame, wheel_area, &wheel_source, motion, ctx));

    draw_filter_pill(frame, pill_area, ui, filter_fold_in_flight, ctx);
    draw_basket_strip(frame, basket_area, ui, ctx);
    draw_footer(frame, footer_area, ui, flash, ctx);

    // Toasts must not obstruct the confirm modal (design slice 4) or the
    // palette (D6): they sit top-right of the main content, well clear of
    // either's centered dialog, but are skipped outright whenever one is
    // open — simpler than reasoning about overlap and correct for every
    // terminal size, not just the common ones.
    if ui.confirm().is_none() && !ui.palette_open() {
        draw_toasts(frame, main_area, toasts, ctx);
    }

    // Modal precedence: the palette (D6) outranks everything (D5 extends
    // design slice 4's ladder for the rest: confirm > review > freeable
    // panel > cheatsheet).
    let mut freeable_rows = None;
    if let Some(palette) = ui.palette() {
        draw_palette_modal(frame, palette, ui, phase, saved_queries, ctx);
    } else if let Some(confirm) = ui.confirm() {
        draw_confirm_modal(frame, ui, confirm, ctx);
    } else if let Some(review) = ui.review() {
        draw_review_modal(frame, ui, review, ctx);
    } else if ui.freeable_open() {
        freeable_rows = Some(draw_freeable_modal(frame, ui, ctx));
    } else if ui.cheatsheet_open() {
        draw_cheatsheet_modal(frame, ctx);
    }

    FrameGeometry {
        table: Some(table),
        breadcrumb_row: header_area.y,
        breadcrumb,
        errors_card,
        wheel,
        gauge_freeable,
        freeable_rows,
    }
}

/// Which of the three main-panel layouts applies this frame (design
/// slice 5): the full side donut, the header mini-donut, or neither. A
/// pure function of exactly what drives the decision, so the threshold
/// and the zen/ASCII precedence are unit-tested without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelLayout {
    Full,
    Mini,
    Hidden,
}

/// Zen mode and the ASCII glyph rung both hide the wheel outright
/// (there is nothing to click into color-blind ASCII, and zen mode is
/// "table only" by definition) regardless of width; otherwise the full
/// side panel needs [`MIN_WHEEL_TERMINAL_WIDTH`] columns and the
/// header-line mini-donut takes over below it.
fn wheel_layout(terminal_width: u16, ascii: bool, zen: bool) -> WheelLayout {
    if ascii || zen {
        WheelLayout::Hidden
    } else if terminal_width >= MIN_WHEEL_TERMINAL_WIDTH {
        WheelLayout::Full
    } else {
        WheelLayout::Mini
    }
}

/// Row heights of the metric-cards line and the disk-gauge line —
/// collapsed to zero in zen mode (design slice 5's `z`: table + footer +
/// basket strip only, see [`draw`]).
fn cards_and_gauge_heights(zen: bool) -> (u16, u16) {
    if zen { (0, 0) } else { (3, 1) }
}

/// Header line: signature glyph, clickable breadcrumb path, scan status
/// with spinner. Returns each path component's screen column range paired
/// with the ancestor directory clicking it jumps to (`None` for the
/// current directory's own trailing segment, and for any segment before
/// the first descend — there is nothing above the scan root to jump to).
fn draw_header(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    spinner: char,
    ctx: &RenderCtx,
) -> Vec<(u16, u16, Option<DirId>)> {
    let snapshot = ui.snapshot();
    let theme = &ctx.theme;
    let signature = if ctx.ascii() { "camembert" } else { SIGNATURE };
    let status: Span<'_> = if snapshot.stats.root_complete {
        Span::from("done").fg(theme.color(theme::GOOD))
    } else {
        Span::from(format!("{spinner} scanning")).fg(theme.color(theme::ACCENT))
    };
    let path = snapshot.path.display().to_string();
    let mut spans = vec![
        Span::from(" "),
        Span::from(signature).fg(theme.color(theme::ACCENT)).bold(),
        Span::from("  "),
        Span::from(path.clone()).bold(),
        Span::from("  "),
        status,
    ];
    // D3 mode badge: which flat/breakdown mode is active, and whether its
    // summary is still the live provisional one — same style as the
    // hardlink footer note (italic, accent color).
    if let Some(text) = mode_badge_text(ui.mode(), ui.flat_summary()) {
        spans.push(Span::from("  ·  "));
        spans.push(Span::from(text).fg(theme.color(theme::ACCENT)).italic());
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    // Path starts right after " " + signature + "  " on the header line.
    let path_col = area.x + (1 + signature.chars().count() + 2) as u16;
    let stack: Vec<DirId> = ui.stack_dirs().collect();
    let segments = fmt::path_segments(&path);
    let total = segments.len();
    // Components before the first descend all belong to the scan root's
    // own (possibly multi-component) path — clicking any of them jumps to
    // the same place, the root.
    let root_prefix = total.saturating_sub(stack.len());
    segments
        .into_iter()
        .enumerate()
        .map(|(i, (start, end))| {
            let target = if i + 1 == total {
                None // the current directory's own segment
            } else if i < root_prefix {
                stack.first().copied()
            } else {
                stack.get(i - root_prefix + 1).copied()
            };
            (path_col + start as u16, path_col + end as u16, target)
        })
        .collect()
}

/// The D3 mode badge text for the header line, or `None` in tree mode
/// (nothing to badge). Mirrors the hardlink footer note's own
/// "provisional totals... corrected at scan end" framing: the summary is
/// still the live accumulator's, not the authoritative post-scan fold.
/// The truncation line ("top N shown") is a separate footer note (see
/// [`draw_footer`]) — distinct information from provenance, worth its own
/// line rather than one badge carrying both.
fn mode_badge_text(mode: ViewMode, summary: Option<&flat::FlatSummary>) -> Option<String> {
    let label = match mode {
        ViewMode::Tree => return None,
        ViewMode::FlatTop => "flat top files",
        ViewMode::Breakdown => "pattern breakdown",
    };
    let mut text = label.to_owned();
    if summary.is_some_and(|s| s.provisional) {
        text.push_str(" — provisional, updates live during the scan");
    }
    Some(text)
}

/// Metric cards row: total real · entries · errors · hardlinks, one
/// rounded-border card each with its own accent color. The errors card is
/// clickable (toggles sort-by-errors); its screen rect is returned for
/// that hit-test.
fn draw_metric_cards(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    ctx: &RenderCtx,
) -> Option<(u16, u16, u16, u16)> {
    let snapshot = ui.snapshot();
    let theme = &ctx.theme;
    let stats = &snapshot.stats;
    let error_entry = if stats.errors > 0 {
        theme::ERROR
    } else {
        theme::MUTED
    };
    // D4 composition: "total"/"entries" become the match set's own
    // totals under an active filter — errors and the hardlink-inode count
    // stay whole-scan concepts the filter has no opinion about.
    let (total_label, total_bytes, entries) = match ui.active_filter() {
        Some(filter) => (
            "matched",
            filter.result.matched_disk,
            filter.result.matched_entries,
        ),
        None => ("total", stats.disk_bytes, stats.entries),
    };
    let cards: [(&str, String, theme::Slot); 4] = [
        (
            total_label,
            HumanSize(total_bytes).to_string(),
            theme::ACCENT,
        ),
        ("entries", entries.to_string(), theme::INFO),
        ("errors", stats.errors.to_string(), error_entry),
        (
            "hardlinks",
            snapshot.hardlink_inodes.to_string(),
            theme::MAUVE,
        ),
    ];
    let areas = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(area);
    let mut errors_card = None;
    for ((label, value, accent), card_area) in cards.into_iter().zip(areas.iter()) {
        if label == "errors" {
            errors_card = Some((card_area.x, card_area.y, card_area.width, card_area.height));
        }
        let block = card_block(ctx)
            .border_style(Style::new().fg(theme.color(theme::MUTED)))
            .title(Span::from(format!(" {label} ")).fg(theme.color(accent)));
        let text = Paragraph::new(Line::from(Span::from(value).fg(theme.color(accent)).bold()))
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(text, *card_area);
    }
    errors_card
}

/// Rounded borders where the glyph ladder allows, plain ASCII otherwise.
fn card_block(ctx: &RenderCtx) -> Block<'static> {
    if ctx.ascii() {
        Block::bordered().border_type(BorderType::Plain)
    } else {
        Block::bordered().border_type(BorderType::Rounded)
    }
}

/// Disk gauge line: statvfs capacity of the scanned filesystem — how much
/// is occupied, and how much of the occupied space this scan accounts
/// for. Coverage is clamped to 100% (mid-scan hardlink attribution and
/// concurrent writes can transiently overshoot). When the freeable ledger
/// has root-fs freeable bytes (D5), a clickable " · X.X GiB freeable"
/// suffix appears and this returns the gauge's screen rect for that
/// hit-test; `None` when there's nothing to click through to (no ledger
/// yet, a degraded/zero sweep, `--no-proc-sweep`, or a future dump-loaded
/// session with no ledger at all, D7).
fn draw_disk_gauge(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    ctx: &RenderCtx,
) -> Option<(u16, u16, u16, u16)> {
    let theme = &ctx.theme;
    let snapshot = ui.snapshot();
    let Some(disk) = ctx.disk else {
        frame.render_widget(
            Paragraph::new(Line::from(
                Span::from(" disk stats unavailable").fg(theme.color(theme::MUTED)),
            )),
            area,
        );
        return None;
    };
    let used = disk.used_fraction();
    let coverage = disk.coverage_fraction(snapshot.stats.disk_bytes);
    let freeable_bytes = ui
        .freeable_ledger()
        .map_or(0, Ledger::root_fs_freeable_bytes);
    let mut text = format!(
        " {} · {:.0}% used · this scan covers {:.0}% of used",
        HumanSize(disk.capacity),
        used * 100.0,
        coverage * 100.0,
    );
    if freeable_bytes > 0 {
        text.push_str(&format!(" · {} freeable", HumanSize(freeable_bytes)));
    }
    text.push(' ');
    let label = " disk ";
    let bar_width = area
        .width
        .saturating_sub(label.chars().count() as u16)
        .saturating_sub(text.chars().count() as u16) as usize;
    let (covered_ch, used_ch, free_ch) = if ctx.ascii() {
        ('#', '=', '.')
    } else {
        ('█', '█', '░')
    };
    let used_cells = (used * bar_width as f64).round() as usize;
    let covered_cells = (used * coverage * bar_width as f64).round() as usize;
    let covered_cells = covered_cells.min(used_cells);
    let mut spans = vec![
        Span::from(label).fg(theme.color(theme::MUTED)),
        Span::from(covered_ch.to_string().repeat(covered_cells)).fg(theme.color(theme::ACCENT)),
        Span::from(used_ch.to_string().repeat(used_cells - covered_cells))
            .fg(theme.color(theme::MUTED)),
        Span::from(free_ch.to_string().repeat(bar_width - used_cells))
            .fg(theme.color(theme::MUTED))
            .dim(),
    ];
    spans.push(Span::from(text).fg(theme.color(theme::MUTED)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    (freeable_bytes > 0).then_some((area.x, area.y, area.width, area.height))
}

// Same call as `draw`'s: each parameter is an independent per-frame
// input, none of them a natural subgroup.
#[allow(clippy::too_many_arguments)]
fn draw_table(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    outcome: Option<&ScanOutcome>,
    table_state: &mut TableState,
    spinner: char,
    ranks: &[Option<usize>],
    bar_progress: f64,
    ctx: &RenderCtx,
) -> TableGeometry {
    let theme = &ctx.theme;
    let snapshot = ui.snapshot();
    let sort = ui.sort();
    let arrow = |key: SortKey| -> &'static str {
        if sort.key != key {
            ""
        } else if sort.descending {
            if ctx.ascii() { "v" } else { "▼" }
        } else if ctx.ascii() {
            "^"
        } else {
            "▲"
        }
    };
    let mut header_cells = vec![
        Cell::from(" "),
        Cell::from(" "),
        Cell::from(format!("real{}", arrow(SortKey::Disk))),
    ];
    let mut widths = vec![
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(10),
    ];
    if ui.show_apparent {
        header_cells.push(Cell::from(format!("apparent{}", arrow(SortKey::Apparent))));
        widths.push(Constraint::Length(10));
    }
    header_cells.extend([
        Cell::from("%"),
        Cell::from(""),
        Cell::from(format!("items{}", arrow(SortKey::Items))),
        Cell::from(format!("name{}", arrow(SortKey::Name))),
    ]);
    widths.extend([
        Constraint::Length(6),
        Constraint::Length(BAR_WIDTH as u16),
        Constraint::Length(9),
        Constraint::Min(10),
    ]);

    // D4 composition: under an active filter, a directory row's own
    // subtree total is swapped for its *filtered* one (the row set itself
    // is already filtered — see `UiState::ensure_sorted` — this is only
    // the displayed numbers); `parent_disk` (the % column's denominator)
    // follows the same swap so the percentages stay internally
    // consistent with what they're a percentage *of*.
    let active_filter = ui.active_filter();
    let parent_disk = active_filter
        .map(|filter| filter.result.dir_total(snapshot.dir).disk)
        .unwrap_or(snapshot.totals.disk);
    let muted = theme.color(theme::MUTED);
    let coral = theme.color(theme::ERROR);
    let rows = ui.rows_indexed().map(|(index, row)| {
        let (disk, apparent, items) = match (active_filter, row.dir) {
            (Some(filter), Some(dir)) => {
                let totals = filter.result.dir_total(dir);
                (totals.disk, totals.apparent, totals.entries)
            }
            _ => (row.disk, row.apparent, row.items),
        };
        let marked = ui.is_marked(row.node);
        let mark = if marked {
            Span::from("*").fg(coral).bold()
        } else {
            Span::raw(" ")
        };
        let marker = match row.state {
            RowState::Scanning => Span::from(spinner.to_string()).fg(theme.color(theme::ACCENT)),
            RowState::Error => Span::from("!").fg(coral).bold(),
            RowState::Complete | RowState::File if row.errors > 0 => Span::from("!").fg(coral),
            RowState::Complete | RowState::File => Span::raw(" "),
        };
        let frac = if parent_disk > 0 {
            disk as f64 / parent_disk as f64
        } else {
            0.0
        };
        let pct = if parent_disk > 0 {
            format!("{:5.1}", 100.0 * frac)
        } else {
            format!("{:>5}", "-")
        };
        // Identity color: bar color == name color == wheel slice color.
        let identity = ranks
            .get(index)
            .copied()
            .flatten()
            .map(|rank| theme.identity(rank));
        // Eased bar fill (design slice 5): the percentage text above
        // shows the real value immediately, only the bar itself grows in
        // — `bar_progress` is a uniform 0->1 reveal shared by every row
        // in the view, restarted on the next navigation/sort.
        let bar = Span::from(wheel::proportion_bar(
            frac * bar_progress,
            BAR_WIDTH,
            ctx.ascii(),
        ))
        .fg(identity.unwrap_or(muted));
        let mut name_text = String::from_utf8_lossy(&row.name).into_owned();
        if row.is_dir {
            name_text.push('/');
        }
        // D3/attack finding 1: a matched hardlink-extra row is present at
        // 0 bytes (never silently absent) — flagged so the 0 makes sense
        // instead of reading as "empty"; the pill spells out where the
        // bytes are actually counted.
        let is_extra = active_filter.is_some()
            && !row.is_dir
            && outcome.is_some_and(|outcome| {
                outcome
                    .node(row.node)
                    .flags()
                    .contains(NodeFlags::HARDLINK_EXTRA)
            });
        if is_extra {
            name_text.push_str(" \u{26d3}");
        }
        let name = if row.is_dir {
            Span::from(name_text).bold()
        } else {
            Span::from(name_text)
        };
        // Marked rows tint coral; otherwise the identity color (non-top-N
        // rows keep the default foreground).
        let name = if marked {
            name.fg(coral)
        } else if let Some(color) = identity {
            name.fg(color)
        } else {
            name
        };
        let mut cells = vec![
            Cell::from(mark),
            Cell::from(marker),
            Cell::from(format!("{:>9}", HumanSize(disk).to_string())),
        ];
        if ui.show_apparent {
            cells.push(Cell::from(format!(
                "{:>9}",
                HumanSize(apparent).to_string()
            )));
        }
        cells.extend([
            Cell::from(pct),
            Cell::from(bar),
            Cell::from(format!("{:>8}", items)),
            Cell::from(name),
        ]);
        TableRow::new(cells)
    });
    let table = Table::new(rows, widths)
        .header(
            TableRow::new(header_cells).style(
                Style::new()
                    .fg(theme.color(theme::MUTED))
                    .add_modifier(Modifier::UNDERLINED),
            ),
        )
        .row_highlight_style(theme.selection_style());
    frame.render_stateful_widget(table, area, table_state);
    // Body rows sit below the one-line header; ratatui scrolls
    // `table_state`'s offset during the render above to keep the cursor
    // visible, so reading it back here always matches what was just
    // drawn.
    TableGeometry {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(1),
        offset: table_state.offset(),
    }
}

/// `t` mode: the flat top-files table (D3). Columns: mark, `⛓` hardlink
/// badge, real size, optional apparent size, % of the whole scan, bar,
/// name/path. Mid-scan (no frozen arena yet, see `flatview`'s module doc)
/// the last column shows the basename alone — real data, not a
/// placeholder — widening to the full path (abbreviated like the
/// breadcrumb) once the scan completes. Rows are real arena nodes:
/// marking and Enter-jump act on them exactly like a tree row (see
/// `try_toggle_mark_flat`/`try_jump_flat_row`), though both stay gated to
/// post-scan since they need a real path. Returns the same
/// [`TableGeometry`] shape as [`draw_table`] (mouse hit-testing is
/// mode-agnostic) plus the [`WheelSource`] built from the same rows/order,
/// so the donut can never disagree with what's on screen.
#[allow(clippy::too_many_arguments)]
fn draw_flat_table(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    outcome: Option<&ScanOutcome>,
    table_state: &mut TableState,
    scan_disk_total: u64,
    bar_progress: f64,
    ctx: &RenderCtx,
) -> (TableGeometry, WheelSource) {
    let theme = &ctx.theme;
    let sort = ui.sort();
    let arrow = |key: SortKey| -> &'static str {
        if sort.key != key {
            ""
        } else if sort.descending {
            if ctx.ascii() { "v" } else { "▼" }
        } else if ctx.ascii() {
            "^"
        } else {
            "▲"
        }
    };

    // D4 composition: under an active filter, `t` shows the filtered top
    // files ([`FilterResult::top_files`]) instead of the whole-scan
    // summary — never a silent unfiltered list under a filtered header.
    let flat_rows = match ui.active_filter() {
        Some(filter) => flatview::filtered_flat_rows(&filter.result, outcome),
        None => ui
            .flat_summary()
            .map(|summary| flatview::flat_rows(summary, outcome))
            .unwrap_or_default(),
    };
    let order = flatview::sort_flat_rows(&flat_rows, sort);
    let disks: Vec<u64> = flat_rows.iter().map(|row| row.disk).collect();
    let ranks = theme::assign_identity(&disks, theme::IDENTITY_LEN);

    let header_cells = vec![
        Cell::from(" "),
        Cell::from("⛓"),
        Cell::from(format!("real{}", arrow(SortKey::Disk))),
        Cell::from(format!("apparent{}", arrow(SortKey::Apparent))),
        Cell::from("%"),
        Cell::from(""),
        Cell::from(format!("name/path{}", arrow(SortKey::Name))),
    ];
    let widths = [
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(BAR_WIDTH as u16),
        Constraint::Min(10),
    ];
    // Every fixed-width column above, so the path column's actual share
    // (`Constraint::Min(10)`) can be computed here for abbreviation (D3:
    // "path, abbreviated like the breadcrumb does") — same idiom
    // `draw_wheel`'s caption already uses for the viewed directory's path.
    const FIXED_COLUMNS_WIDTH: u16 = 1 + 1 + 10 + 10 + 6 + BAR_WIDTH as u16;
    let path_width = area.width.saturating_sub(FIXED_COLUMNS_WIDTH) as usize;

    let muted = theme.color(theme::MUTED);
    let coral = theme.color(theme::ERROR);
    let table_rows = order.iter().map(|&index| {
        let row = &flat_rows[index];
        let marked = ui.is_marked(row.node);
        let mark = if marked {
            Span::from("*").fg(coral).bold()
        } else {
            Span::raw(" ")
        };
        let badge = if row.hardlink {
            Span::from("⛓").fg(theme.color(theme::MAUVE))
        } else {
            Span::raw(" ")
        };
        let frac = if scan_disk_total > 0 {
            row.disk as f64 / scan_disk_total as f64
        } else {
            0.0
        };
        let pct = if scan_disk_total > 0 {
            format!("{:5.1}", 100.0 * frac)
        } else {
            format!("{:>5}", "-")
        };
        let identity = ranks
            .get(index)
            .copied()
            .flatten()
            .map(|rank| theme.identity(rank));
        let bar = Span::from(wheel::proportion_bar(
            frac * bar_progress,
            BAR_WIDTH,
            ctx.ascii(),
        ))
        .fg(identity.unwrap_or(muted));
        // Post-scan: the full path, abbreviated like the breadcrumb.
        // Mid-scan (no frozen arena yet): the basename alone — real data
        // straight off `TopFile.name`, not a placeholder (D3/flatview's
        // module doc).
        let path_text = match &row.path {
            Some(p) => fmt::abbreviate_path(&p.display().to_string(), path_width),
            None => fmt::abbreviate_path(&row.name, path_width),
        };
        let path = Span::from(path_text);
        let path = if marked {
            path.fg(coral)
        } else if let Some(color) = identity {
            path.fg(color)
        } else {
            path
        };
        let apparent = row
            .apparent
            .map(|a| HumanSize(a).to_string())
            .unwrap_or_else(|| "-".to_owned());
        TableRow::new(vec![
            Cell::from(mark),
            Cell::from(badge),
            Cell::from(format!("{:>9}", HumanSize(row.disk).to_string())),
            Cell::from(format!("{apparent:>9}")),
            Cell::from(pct),
            Cell::from(bar),
            Cell::from(path),
        ])
    });
    let table = Table::new(table_rows, widths)
        .header(
            TableRow::new(header_cells).style(
                Style::new()
                    .fg(theme.color(theme::MUTED))
                    .add_modifier(Modifier::UNDERLINED),
            ),
        )
        .row_highlight_style(theme.selection_style());
    frame.render_stateful_widget(table, area, table_state);
    let geometry = TableGeometry {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(1),
        offset: table_state.offset(),
    };
    let slice_rows: Vec<(u64, Option<usize>)> = order
        .iter()
        .map(|&index| (flat_rows[index].disk, ranks.get(index).copied().flatten()))
        .collect();
    let wheel_source = WheelSource {
        slice_rows,
        total: scan_disk_total,
        caption: "flat top files".to_owned(),
    };
    (geometry, wheel_source)
}

/// `b` mode: the pattern-breakdown table (D3/D1). Columns: label, kind
/// (dir-pattern/file-pattern/blank for the uncategorized row), total,
/// entry count, % of the breakdown's own total. The trailing uncategorized
/// row ([`flatview::breakdown_rows`]) is always shown but never given an
/// identity rank or a wheel slice of its own — D1's disjoint-partition
/// invariant means the wheel's automatic "unaccounted" remainder already
/// equals it exactly, so excluding it from `slice_rows` here is what
/// produces the correct gray "uncategorized" wedge (attack finding 2's
/// fix) instead of a second, redundant colored one.
fn draw_breakdown_table(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    table_state: &mut TableState,
    bar_progress: f64,
    ctx: &RenderCtx,
) -> (TableGeometry, WheelSource) {
    let theme = &ctx.theme;
    let sort = ui.sort();
    let arrow = |key: SortKey| -> &'static str {
        if sort.key != key {
            ""
        } else if sort.descending {
            if ctx.ascii() { "v" } else { "▼" }
        } else if ctx.ascii() {
            "^"
        } else {
            "▲"
        }
    };

    // D4 composition / attack finding 6: under an active filter, `b`
    // shows groups computed *over the match set*
    // ([`FilterResult::groups`]/`rest`) — never the silent unfiltered
    // groups under a filtered header the attack forbids.
    let rows = match ui.active_filter() {
        Some(filter) => flatview::filtered_breakdown_rows(&filter.result),
        None => ui
            .flat_summary()
            .map(flatview::breakdown_rows)
            .unwrap_or_default(),
    };
    let order = flatview::sort_breakdown_rows(&rows, sort);
    let total_disk = match ui.active_filter() {
        Some(filter) => flatview::filtered_breakdown_total_disk(&filter.result),
        None => ui
            .flat_summary()
            .map(flatview::breakdown_total_disk)
            .unwrap_or(0),
    };
    // Every row except the trailing uncategorized one gets a rank (never
    // that one — see the function doc); `rows.len() - 1` is always the
    // uncategorized row's position (`flatview::breakdown_rows` appends it
    // last, unconditionally).
    let group_disks: Vec<u64> = rows
        .iter()
        .take(rows.len().saturating_sub(1))
        .map(|row| row.disk)
        .collect();
    let mut ranks = theme::assign_identity(&group_disks, theme::IDENTITY_LEN);
    ranks.push(None);

    let header_cells = vec![
        Cell::from(format!("label{}", arrow(SortKey::Name))),
        Cell::from("kind"),
        Cell::from(format!("real{}", arrow(SortKey::Disk))),
        Cell::from(format!("apparent{}", arrow(SortKey::Apparent))),
        Cell::from(format!("entries{}", arrow(SortKey::Items))),
        Cell::from("%"),
        Cell::from(""),
    ];
    let widths = [
        Constraint::Min(16),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(6),
        Constraint::Length(BAR_WIDTH as u16),
    ];

    let muted = theme.color(theme::MUTED);
    let table_rows = order.iter().map(|&index| {
        let row = &rows[index];
        let kind = match row.kind {
            Some(flat::PatternKind::Dir) => "dir/",
            Some(flat::PatternKind::File) => "file",
            None => "",
        };
        let pct = flatview::breakdown_percent(row, total_disk);
        let pct_text = if total_disk > 0 {
            format!("{pct:5.1}")
        } else {
            format!("{:>5}", "-")
        };
        let identity = ranks
            .get(index)
            .copied()
            .flatten()
            .map(|rank| theme.identity(rank));
        let bar = Span::from(wheel::proportion_bar(
            (pct / 100.0) * bar_progress,
            BAR_WIDTH,
            ctx.ascii(),
        ))
        .fg(identity.unwrap_or(muted));
        let label = Span::from(row.label.clone());
        let label = if let Some(color) = identity {
            label.fg(color)
        } else {
            label
        };
        TableRow::new(vec![
            Cell::from(label),
            Cell::from(kind),
            Cell::from(format!("{:>9}", HumanSize(row.disk).to_string())),
            Cell::from(format!("{:>9}", HumanSize(row.apparent).to_string())),
            Cell::from(format!("{:>8}", row.entries)),
            Cell::from(pct_text),
            Cell::from(bar),
        ])
    });
    let table = Table::new(table_rows, widths)
        .header(
            TableRow::new(header_cells).style(
                Style::new()
                    .fg(theme.color(theme::MUTED))
                    .add_modifier(Modifier::UNDERLINED),
            ),
        )
        .row_highlight_style(theme.selection_style());
    frame.render_stateful_widget(table, area, table_state);
    let geometry = TableGeometry {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(1),
        offset: table_state.offset(),
    };
    // The uncategorized row is excluded from the donut's own rows (see the
    // function doc): its share reaches the wheel only as the automatic
    // "unaccounted" remainder `build_slices` computes from `total -
    // sum(slice_rows)`, which — thanks to D1's disjoint partition — is
    // exactly `summary.rest`, never an overlap artifact.
    let slice_rows: Vec<(u64, Option<usize>)> = order
        .iter()
        .filter(|&&index| rows[index].kind.is_some())
        .map(|&index| (rows[index].disk, ranks.get(index).copied().flatten()))
        .collect();
    let wheel_source = WheelSource {
        slice_rows,
        total: total_disk,
        caption: "pattern breakdown".to_owned(),
    };
    (geometry, wheel_source)
}

/// Selection card under the table: humanized mtime, item count, share of
/// the parent, error count for the row under the cursor — or the
/// mouse-hovered row while the pointer sits over the table.
fn draw_selection_card(frame: &mut Frame<'_>, area: Rect, ui: &UiState, ctx: &RenderCtx) {
    let theme = &ctx.theme;
    // Accent border while the mouse is driving the card (a transient
    // preview), muted for the keyboard cursor's steady-state selection.
    let border = if ui.hover().is_some() {
        theme::ACCENT
    } else {
        theme::MUTED
    };
    let block = card_block(ctx).border_style(Style::new().fg(theme.color(border)));
    // The mouse-hovered row while present, else the keyboard cursor —
    // both drive this card (design slice 3).
    let Some(row) = ui.card_row() else {
        frame.render_widget(
            Paragraph::new(Line::from(
                Span::from("nothing selected").fg(theme.color(theme::MUTED)),
            ))
            .block(block),
            area,
        );
        return;
    };
    let name = String::from_utf8_lossy(&row.name).into_owned();
    let suffix = if row.is_dir { "/" } else { "" };
    let parent_disk = ui.snapshot().totals.disk;
    let share = if parent_disk > 0 {
        format!(
            "{:.1}% of parent",
            100.0 * row.disk as f64 / parent_disk as f64
        )
    } else {
        "-% of parent".to_owned()
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let sep = Span::from(" · ").fg(theme.color(theme::MUTED));
    let mut line2 = vec![
        Span::from(format!("modified {}", fmt::humanize_age(now, row.mtime))),
        sep.clone(),
        Span::from(format!("{} items", row.items)),
    ];
    if row.errors > 0 {
        line2.push(sep.clone());
        line2.push(Span::from(format!("{} errors", row.errors)).fg(theme.color(theme::ERROR)));
    }
    let lines = vec![
        Line::from(vec![
            Span::from(format!("{:>9}", HumanSize(row.disk).to_string())).bold(),
            sep.clone(),
            Span::from(share),
        ]),
        Line::from(line2),
    ];
    let block = block.title(Span::from(format!(" {name}{suffix} ")).bold());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The donut's data source for the current frame (D3): the same
/// `(disk, identity rank)` rows and total the table used, in display
/// order — so `slice_rows`' position doubles as the cursor position a
/// click on that slice should land on, whichever mode built it. `caption`
/// is the un-abbreviated text to show under the donut (a directory path
/// in tree mode, a short mode label in flat/breakdown mode — see
/// [`draw_wheel`], which abbreviates it to fit).
#[derive(Debug, Clone, Default)]
struct WheelSource {
    slice_rows: Vec<(u64, Option<usize>)>,
    total: u64,
    caption: String,
}

/// The donut camembert: `source`'s rows as slices, colored with the same
/// identity colors as the table they came from. Small/unranked slices
/// merge into a gray rest slice (in breakdown mode this is exactly D1's
/// disjoint "everything matched by no group", since the rest bucket is
/// never itself one of `source`'s rows — see
/// [`draw_breakdown_table`]); under the wheel: `source.caption`
/// (abbreviated) and its total.
fn draw_wheel(
    frame: &mut Frame<'_>,
    area: Rect,
    source: &WheelSource,
    motion: &mut anim::Motion,
    ctx: &RenderCtx,
) -> Option<WheelGeometry> {
    let theme = &ctx.theme;
    let block = card_block(ctx).border_style(Style::new().fg(theme.color(theme::MUTED)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height < 4 {
        return None;
    }
    // Reserve the bottom two lines for caption + total.
    let [donut_area, caption_area] =
        Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).areas(inner);

    let (target_fracs, slice_ranks) = wheel::build_slices(&source.slice_rows, source.total);
    // Donut morph (design slice 5): eases from whatever was last drawn
    // into `target_fracs` on a navigation/sort — during a live scan the
    // donut already grows continuously and `motion` never triggers (see
    // `UiState::view_change_seq`), so this never fights that growth.
    let fracs = motion.donut_fracs(&target_fracs);
    let mut geometry = None;
    if !fracs.is_empty() && donut_area.width >= 2 && donut_area.height >= 2 {
        let colors: Vec<Color> = slice_ranks
            .iter()
            .map(|rank| rank.map_or(theme.color(theme::MUTED), |r| theme.identity(r)))
            .collect();
        let cells = match ctx.caps.glyphs {
            GlyphLevel::Sextant => wheel::compose_sextants(&wheel::rasterize(
                &fracs,
                2 * donut_area.width as usize,
                3 * donut_area.height as usize,
                wheel::SEXTANT_ASPECT,
            )),
            GlyphLevel::HalfBlock | GlyphLevel::Ascii => {
                wheel::compose_half_blocks(&wheel::rasterize(
                    &fracs,
                    donut_area.width as usize,
                    2 * donut_area.height as usize,
                    wheel::HALF_BLOCK_ASPECT,
                ))
            }
        };
        blit_wheel(frame, donut_area, &cells, &colors);

        let targets = wheel::build_slice_targets(&source.slice_rows, source.total);
        let (width, height) = (donut_area.width as usize, donut_area.height as usize);
        let mut cell_slices = vec![None; width * height];
        for (row, line) in cells.iter().enumerate().take(height) {
            for (col, cell) in line.iter().enumerate().take(width) {
                cell_slices[row * width + col] = cell.fg;
            }
        }
        geometry = Some(WheelGeometry {
            x: donut_area.x,
            y: donut_area.y,
            width,
            height,
            cells: cell_slices,
            targets,
        });
    }

    let caption = vec![
        Line::from(
            Span::from(fmt::abbreviate_path(
                &source.caption,
                inner.width.saturating_sub(2) as usize,
            ))
            .fg(theme.color(theme::MUTED)),
        )
        .alignment(Alignment::Center),
        Line::from(Span::from(HumanSize(source.total).to_string()).bold())
            .alignment(Alignment::Center),
    ];
    frame.render_widget(Paragraph::new(caption), caption_area);
    geometry
}

/// Responsive collapse (design slice 5, [`wheel_layout`]): below
/// [`MIN_WHEEL_TERMINAL_WIDTH`] the side donut panel has no room, so a
/// compact version rides the right end of the header line instead — the
/// same slice data (identity colors, motion-eased fractions) as the full
/// wheel, just a handful of cells instead of a whole panel.
///
/// Decorative only, by design: unlike the full wheel's slices, these
/// cells are never added to [`FrameGeometry`] and are not click targets.
/// Hit-testing a shape this small reliably (sub-cell precision, shared
/// with the header's breadcrumb/status text) is not worth the
/// complexity for a feature whose whole point is staying out of the
/// way on a narrow terminal — clicking is still available once the
/// terminal is wide enough for the real panel, or via the keyboard.
fn draw_mini_donut(
    frame: &mut Frame<'_>,
    header_area: Rect,
    source: &WheelSource,
    motion: &mut anim::Motion,
    ctx: &RenderCtx,
) {
    if header_area.width < MINI_DONUT_WIDTH + 2 {
        return; // no room even for the compact form
    }
    let area = Rect {
        x: header_area.x + header_area.width - MINI_DONUT_WIDTH,
        y: header_area.y,
        width: MINI_DONUT_WIDTH,
        height: 1,
    };
    let theme = &ctx.theme;
    let (target_fracs, slice_ranks) = wheel::build_slices(&source.slice_rows, source.total);
    let fracs = motion.donut_fracs(&target_fracs);
    if fracs.is_empty() {
        return;
    }
    let colors: Vec<Color> = slice_ranks
        .iter()
        .map(|rank| rank.map_or(theme.color(theme::MUTED), |r| theme.identity(r)))
        .collect();
    let cells = match ctx.caps.glyphs {
        GlyphLevel::Sextant => wheel::compose_sextants(&wheel::rasterize(
            &fracs,
            2 * area.width as usize,
            3 * area.height as usize,
            wheel::SEXTANT_ASPECT,
        )),
        GlyphLevel::HalfBlock | GlyphLevel::Ascii => wheel::compose_half_blocks(&wheel::rasterize(
            &fracs,
            area.width as usize,
            2 * area.height as usize,
            wheel::HALF_BLOCK_ASPECT,
        )),
    };
    blit_wheel(frame, area, &cells, &colors);
}

/// Copy a composed wheel-cell grid into the frame buffer, mapping slice
/// indices to colors. All coordinates are bounded by `area`, which the
/// caller guarantees lies within the frame.
fn blit_wheel(
    frame: &mut Frame<'_>,
    area: Rect,
    cells: &[Vec<wheel::WheelCell>],
    colors: &[Color],
) {
    let buffer = frame.buffer_mut();
    for (row, line) in cells.iter().enumerate().take(area.height as usize) {
        for (col, cell) in line.iter().enumerate().take(area.width as usize) {
            if cell.fg.is_none() && cell.bg.is_none() {
                continue;
            }
            let position = (area.x + col as u16, area.y + row as u16);
            let Some(buf_cell) = buffer.cell_mut(position) else {
                continue;
            };
            buf_cell.set_char(cell.ch);
            let color_of = |slice: u16| colors.get(slice as usize).copied().unwrap_or(Color::Reset);
            let mut style = Style::new();
            if let Some(fg) = cell.fg {
                style = style.fg(color_of(fg));
            }
            if let Some(bg) = cell.bg {
                style = style.bg(color_of(bg));
            }
            buf_cell.set_style(style);
        }
    }
}

fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    flash: Option<&str>,
    ctx: &RenderCtx,
) {
    let theme = &ctx.theme;
    let snapshot = ui.snapshot();
    let mut notes: Vec<Span<'_>> = Vec::new();
    let push_note = |notes: &mut Vec<Span<'_>>, note: Span<'static>| {
        if !notes.is_empty() {
            notes.push(Span::raw(" · "));
        }
        notes.push(note);
    };
    if let Some(text) = flash {
        push_note(
            &mut notes,
            Span::from(format!(" {text}"))
                .fg(theme.color(theme::ACCENT))
                .bold(),
        );
    }
    // Marked-entry count/size lives in the basket strip now (design
    // slice 4, drawn just above the footer) — showing it here too would
    // be the same fact twice on adjacent lines.
    if show_hardlink_note(snapshot) {
        push_note(
            &mut notes,
            Span::from("provisional totals (hardlinks) — corrected at scan end")
                .fg(theme.color(theme::ACCENT))
                .italic(),
        );
    }
    if show_updating_note(snapshot) {
        push_note(&mut notes, "updating…".italic().dim());
    }
    // D3 / attack finding 5: a silent cap is exactly the dishonesty this
    // tool exists to avoid — name the cap and where to change it.
    if ui.mode() == ViewMode::FlatTop
        && let Some(summary) = ui.flat_summary()
        && summary.truncated
    {
        push_note(
            &mut notes,
            Span::from(format!(
                "top {} shown — flat_cap in camembert.toml",
                summary.top_files.len()
            ))
            .fg(theme.color(theme::ACCENT))
            .italic(),
        );
    }
    let hints = match ui.mode() {
        ViewMode::Tree => {
            " ↑↓/jk move · ⏎/l/→ open · ⌫/h/← up · g/G ends · d/a/n/m/c/e sort · p apparent · \
             Space mark · u unmark · v review · D delete · t/b flat/breakdown · ? help · q quit"
        }
        ViewMode::FlatTop => {
            " ↑↓/jk move · ⏎/l/→ jump to directory · g/G ends · d/a/n sort · p apparent · \
             Space mark · u unmark · v review · D delete · t back to tree · ? help · q quit"
        }
        ViewMode::Breakdown => {
            " ↑↓/jk move · g/G ends · d/a/n/c sort · p apparent · b back to tree · ? help · q quit"
        }
    };
    let footer =
        Paragraph::new(vec![Line::from(hints.dim()), Line::from(notes)]).alignment(Alignment::Left);
    frame.render_widget(footer, area);
}

/// Persistent one-line deletion basket, above the footer, while at least
/// one entry is marked (design slice 4). `draw` reserves zero height for
/// `area` otherwise, so this simply has nothing to render into — no
/// separate visibility check needed beyond the one `marked_summary`
/// already does.
fn draw_basket_strip(frame: &mut Frame<'_>, area: Rect, ui: &UiState, ctx: &RenderCtx) {
    let Some((count, disk)) = ui.marked_summary() else {
        return;
    };
    let theme = &ctx.theme;
    let glyph = if ctx.ascii() { "[x]" } else { "⌫" };
    let noun = if count == 1 { "item" } else { "items" };
    let text = format!(
        " {glyph} basket: {count} {noun}, {} — v to review, D to delete",
        HumanSize(disk)
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::from(text).fg(theme.color(theme::ERROR)))),
        area,
    );
}

/// Persistent one-line filter pill (D6), above the basket strip while a
/// filter is active. `draw` reserves zero height otherwise, same idiom as
/// [`draw_basket_strip`]. Content: query text, matched entries/bytes, the
/// dir-inode residual explanation when nonzero, a hardlink-extra count
/// when nonzero, and "Esc clears" (attack finding 12: true only from tree
/// view with nothing else open — the Esc ladder in `handle_key` is what
/// actually enforces that scope, this is just the label matching it). A
/// spinner replaces the static glyph while a background fold is in flight
/// (D5), so a debounced keystroke that hasn't landed yet is visibly
/// "still computing" rather than silently stale-looking.
fn draw_filter_pill(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    fold_in_flight: bool,
    ctx: &RenderCtx,
) {
    let Some(filter) = ui.active_filter() else {
        return;
    };
    let theme = &ctx.theme;
    let mut text = filterview::pill_text(&filter.query_text, &filter.result, ui.show_apparent);
    if fold_in_flight {
        let spinner = if ctx.ascii() { '*' } else { '⠋' };
        text = format!("{spinner} {text}");
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::from(text).fg(theme.color(theme::ACCENT)))),
        area,
    );
}

/// Top-right transient notification stack (design slice 4): whatever is
/// still active in the [`toast::ToastQueue`] this frame, one per row,
/// growing down from `area`'s top-right corner (the caller passes the
/// main table/wheel split so toasts never sit over the header or metric
/// cards). Never called while the confirm modal is open — see `draw`.
fn draw_toasts(frame: &mut Frame<'_>, area: Rect, toasts: &[String], ctx: &RenderCtx) {
    let theme = &ctx.theme;
    for (i, message) in toasts.iter().enumerate() {
        let y = area.y + i as u16;
        if y >= area.y + area.height {
            break; // ran out of room: older toasts below the fold wait
        }
        let text = format!(" {message} ");
        let width = (text.chars().count() as u16).min(area.width);
        let rect = Rect {
            x: area.x + area.width - width,
            y,
            width,
            height: 1,
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(Line::from(
                Span::from(text).fg(theme.color(theme::ACCENT)).bold(),
            ))
            .alignment(Alignment::Right),
            rect,
        );
    }
}

/// The Ctrl-K/`/` palette (D6): topmost modal, floating like the others.
/// The input line renders with a block cursor; below it, one of three
/// bodies depending on mode/phase/buffer: mid-scan query mode shows the
/// "available once the scan completes" note (command mode still works,
/// D2); an empty query-mode buffer lists saved queries
/// (`camembert.toml`'s `[queries]`, D6); a nonempty one shows its live
/// parse errors inline (span text + message, attack finding: broken terms
/// are inert, never blocking further typing); command mode lists the
/// fuzzy-matched commands with the current selection picked out.
fn draw_palette_modal(
    frame: &mut Frame<'_>,
    palette: &palette::PaletteState,
    ui: &UiState,
    phase: &Phase,
    saved_queries: &[(String, String)],
    ctx: &RenderCtx,
) {
    let theme = &ctx.theme;
    let area = frame.area();
    let width = area
        .width
        .saturating_sub(4)
        .clamp(30, 90)
        .min(area.width.saturating_sub(2));

    let mode = palette.mode();
    let sigil = match mode {
        PaletteMode::Command => ">",
        PaletteMode::Query => "/",
    };
    let text = palette.text();
    let chars: Vec<char> = text.chars().collect();
    let cursor = palette.cursor();
    // No dedicated background slot in the theme palette — a reverse-video
    // cell is a portable, always-visible cursor block on every capability
    // rung without needing one.
    let cursor_style = Style::new().add_modifier(Modifier::REVERSED);
    let mut input_spans = vec![
        Span::from(format!("{sigil} "))
            .fg(theme.color(theme::ACCENT))
            .bold(),
    ];
    for (i, ch) in chars.iter().enumerate() {
        let span = Span::from(ch.to_string());
        input_spans.push(if i == cursor {
            span.style(cursor_style)
        } else {
            span
        });
    }
    if cursor == chars.len() {
        input_spans.push(Span::from(" ").style(cursor_style));
    }
    let mut lines: Vec<Line<'_>> = vec![Line::from(input_spans), Line::default()];

    match mode {
        PaletteMode::Command => {
            let commands = palette::all_commands();
            let filtered = palette::filter_commands(&palette.content(), &commands);
            if filtered.is_empty() {
                lines.push(Line::from(Span::from("no matching commands").dim()));
            }
            let selected = palette.list_selected();
            for (i, &index) in filtered.iter().enumerate().take(14) {
                let cmd = commands[index];
                let text = format!("{:<34} {}", cmd.label, cmd.hint);
                lines.push(if i == selected {
                    Line::from(Span::from(text).fg(theme.color(theme::ACCENT)).bold())
                } else {
                    Line::from(Span::from(text).dim())
                });
            }
            lines.push(Line::default());
            lines.push(Line::from("↑↓ select · Enter run · Esc close".dim()));
        }
        PaletteMode::Query => {
            if matches!(phase, Phase::Scanning(_)) {
                lines.push(Line::from(
                    Span::from(
                        "filter available once the scan completes — command mode (>) still works",
                    )
                    .fg(theme.color(theme::ACCENT))
                    .italic(),
                ));
            } else if text.is_empty() {
                if saved_queries.is_empty() {
                    lines.push(Line::from(
                        "type a query, or > for commands (Up/Down recalls history)".dim(),
                    ));
                } else {
                    lines.push(Line::from(Span::from("saved queries").bold()));
                    let selected = palette.list_selected();
                    for (i, (label, query)) in saved_queries.iter().enumerate().take(14) {
                        let text = format!("{label:<20} {query}");
                        lines.push(if i == selected {
                            Line::from(Span::from(text).fg(theme.color(theme::ACCENT)).bold())
                        } else {
                            Line::from(Span::from(text).dim())
                        });
                    }
                }
            } else {
                let parsed = palette.parsed();
                if parsed.query.is_empty() && !parsed.errors.is_empty() {
                    lines.push(Line::from(
                        Span::from("every term failed to parse — nothing to apply yet").dim(),
                    ));
                } else if !ui.active_filter().is_some_and(|f| {
                    f.result.query_hash == parsed.query.fingerprint()
                        && f.result.epoch == ui.flat_epoch()
                }) {
                    lines.push(Line::from("applying…".dim()));
                }
                for err in &parsed.errors {
                    let snippet = text.get(err.span.start..err.span.end).unwrap_or("");
                    lines.push(Line::from(vec![
                        Span::from("  "),
                        Span::from(format!("{snippet:?}"))
                            .fg(theme.color(theme::ERROR))
                            .bold(),
                        Span::from(format!(" — {}", err.message))
                            .fg(theme.color(theme::ERROR))
                            .italic(),
                    ]));
                }
            }
            lines.push(Line::default());
            lines.push(Line::from(
                "Enter applies · Up/Down history · Esc close · > for commands".dim(),
            ));
        }
    }

    let title = match mode {
        PaletteMode::Command => " commands ",
        PaletteMode::Query => " filter ",
    };
    render_floating_modal(frame, ctx, area, width, lines, title, theme::ACCENT);
}

/// Centered confirmation modal: count, cumulative size, the first few
/// paths, the hardlink warning when applicable. `y` confirms, anything
/// else cancels — rendering only; the key routing lives in `handle_key`.
fn draw_confirm_modal(
    frame: &mut Frame<'_>,
    ui: &UiState,
    confirm: &ConfirmState,
    ctx: &RenderCtx,
) {
    /// Paths listed in full before the "… and N more" ellipsis.
    const MAX_PATHS: usize = 8;

    let theme = &ctx.theme;
    let Some((count, disk)) = ui.marked_summary() else {
        return; // unreachable: the modal only opens with marks
    };
    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::from(format!(
            "Delete {count} entries — {} on disk?",
            HumanSize(disk)
        ))),
        Line::default(),
    ];
    for mark in ui.marks().iter().take(MAX_PATHS) {
        let suffix = if mark.is_dir { "/" } else { "" };
        lines.push(Line::from(
            Span::from(format!("  {}{suffix}", mark.path.display())).dim(),
        ));
    }
    if count > MAX_PATHS {
        lines.push(Line::from(
            Span::from(format!("  … and {} more", count - MAX_PATHS)).dim(),
        ));
    }
    if confirm.hardlink_files > 0 {
        lines.push(Line::default());
        lines.push(Line::from(
            Span::from(format!(
                "{} hardlinked file(s) in the selection: space is only",
                confirm.hardlink_files
            ))
            .fg(theme.color(theme::ACCENT)),
        ));
        lines.push(Line::from(
            Span::from("freed once every link to an inode is deleted")
                .fg(theme.color(theme::ACCENT)),
        ));
    }
    // D6: advisory only — never blocks `y` — so it just adds a line, same
    // as the hardlink note above.
    if let Some(warning) = &confirm.open_warning {
        lines.push(Line::default());
        lines.push(Line::from(
            Span::from(freeable_panel::warning_text(warning)).fg(theme.color(theme::ACCENT)),
        ));
    }
    lines.push(Line::default());
    lines.push(Line::from(
        "press y to confirm — any other key cancels".bold(),
    ));

    let area = frame.area();
    let width = (lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4)
        .min(area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 2).min(area.height);
    let modal = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, modal);
    let dialog = Paragraph::new(lines).block(
        card_block(ctx)
            .title(" delete marked entries ")
            .border_style(Style::new().fg(theme.color(theme::ERROR))),
    );
    frame.render_widget(dialog, modal);
}

/// Centered, scrollable review-list modal (`v`): every marked entry with
/// its path and size, the cursor row picked out, `Space` unmarks it. Only
/// ever drawn when [`ConfirmState`] is not open (see `draw`'s
/// precedence), so it never has to reason about that overlap.
fn draw_review_modal(frame: &mut Frame<'_>, ui: &UiState, review: &ReviewState, ctx: &RenderCtx) {
    let theme = &ctx.theme;
    let marks = ui.marks();
    let area = frame.area();
    let width = area
        .width
        .saturating_sub(4)
        .clamp(20, 76)
        .min(area.width.saturating_sub(2));
    // Reserve: title line, blank, scroll-position note, blank, hint line.
    const CHROME_LINES: u16 = 5;
    let visible_rows = area.height.saturating_sub(2 + CHROME_LINES).max(1) as usize;
    let offset = if marks.len() <= visible_rows {
        0
    } else {
        review
            .cursor
            .saturating_sub(visible_rows - 1)
            .min(marks.len() - visible_rows)
    };

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::from(format!("{} marked entries", marks.len())).bold()),
        Line::default(),
    ];
    for (i, mark) in marks.iter().enumerate().skip(offset).take(visible_rows) {
        let suffix = if mark.is_dir { "/" } else { "" };
        let text = format!(
            "{:>9}  {}{suffix}",
            HumanSize(mark.disk).to_string(),
            mark.path.display()
        );
        lines.push(if i == review.cursor {
            Line::from(Span::from(text).fg(theme.color(theme::ERROR)).bold())
        } else {
            Line::from(Span::from(text))
        });
    }
    lines.push(Line::default());
    if marks.len() > visible_rows {
        lines.push(Line::from(
            Span::from(format!(
                "row {} of {} — scroll with ↑↓/jk",
                review.cursor + 1,
                marks.len()
            ))
            .dim(),
        ));
    }
    lines.push(Line::from("Space unmark · D delete · v/Esc close".dim()));

    let height = (lines.len() as u16 + 2).min(area.height);
    let modal = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, modal);
    let dialog = Paragraph::new(lines).block(
        card_block(ctx)
            .title(" review marked entries ")
            .border_style(Style::new().fg(theme.color(theme::ACCENT))),
    );
    frame.render_widget(dialog, modal);
}

/// Centered, scrollable freeable panel (`f`, D5): deleted-but-open files on
/// the scan root's filesystem, grouped display-only under their deepest
/// still-existing ancestor directory (D5 grouping, [`freeable_panel`]),
/// then — when present — the cross-filesystem section (D2), the
/// RAM-backed line (D3), and the partial-coverage caveat (D6/D7). Returns
/// the total content-row count actually laid out this frame, fed back into
/// [`UiState::clamp_freeable_cursor`] right after the frame is drawn (see
/// `event_loop`) so the scroll position can never run past what was
/// rendered — the same feedback idiom [`FrameGeometry`] itself uses for
/// mouse hit-testing. Only ever drawn when neither the confirm nor the
/// review modal is open (D5's precedence, see `draw`).
fn draw_freeable_modal(frame: &mut Frame<'_>, ui: &UiState, ctx: &RenderCtx) -> usize {
    let theme = &ctx.theme;
    let area = frame.area();
    let width = area
        .width
        .saturating_sub(4)
        .clamp(24, 90)
        .min(area.width.saturating_sub(2));
    // Reserve: title line, blank, scroll-position note, blank, hint line.
    const CHROME_LINES: u16 = 5;
    let visible_rows = area.height.saturating_sub(2 + CHROME_LINES).max(1) as usize;

    let Some(ledger) = ui.freeable_ledger() else {
        let hint = if ctx.no_proc_sweep {
            "disabled (--no-proc-sweep/NO_PROC_SWEEP)"
        } else {
            "no data yet — the sweep runs once the scan completes"
        };
        let lines = vec![
            Line::from(Span::from("freeable files").bold()),
            Line::default(),
            Line::from(Span::from(hint).dim()),
            Line::default(),
            Line::from("f/Esc closes".dim()),
        ];
        render_floating_modal(frame, ctx, area, width, lines, " freeable ", theme::INFO);
        return 0;
    };

    let mut content: Vec<Line<'_>> = Vec::new();
    for group in ui.freeable_groups() {
        let heading = match &group.ancestor {
            Some(path) => format!("under {}", String::from_utf8_lossy(path)),
            None => "(outside the scan / unknown)".to_owned(),
        };
        content.push(Line::from(
            Span::from(heading).bold().fg(theme.color(theme::INFO)),
        ));
        for &index in &group.entries {
            let Some(entry) = ledger.root_fs_entries().get(index) else {
                continue;
            };
            let holders: Vec<String> = entry
                .holders
                .iter()
                .map(|h| freeable_panel::format_holder(h.pid, &h.comm))
                .collect();
            content.push(Line::from(format!(
                "  {:>9}  {}  [{}]",
                HumanSize(entry.bytes).to_string(),
                entry.evidence_lossy(),
                holders.join(", ")
            )));
        }
    }
    let other_devices = ledger.other_device_groups();
    if !other_devices.is_empty() {
        content.push(Line::from(
            Span::from("other filesystems (excluded from the gauge, D2)")
                .bold()
                .fg(theme.color(theme::MUTED)),
        ));
        for group in other_devices {
            content.push(Line::from(format!("  device {}", group.dev)));
            for entry in &group.entries {
                content.push(Line::from(format!(
                    "    {:>9}  {}",
                    HumanSize(entry.bytes).to_string(),
                    entry.evidence_lossy()
                )));
            }
        }
    }
    if ledger.ram_backed_count() > 0 {
        content.push(Line::from(
            Span::from(format!(
                "{} RAM-backed (memfd/shm) — {}, not disk space",
                ledger.ram_backed_count(),
                HumanSize(ledger.ram_backed_bytes())
            ))
            .italic()
            .dim(),
        ));
    }
    if ledger.coverage().is_partial() {
        content.push(Line::from(
            Span::from(format!(
                "{} of {} processes readable — run as root for the full view",
                ledger.coverage().readable,
                ledger.coverage().seen
            ))
            .fg(theme.color(theme::ACCENT)),
        ));
    }
    if content.is_empty() {
        content.push(Line::from("nothing freeable found".dim()));
    }

    let total = content.len();
    let offset = if total <= visible_rows {
        0
    } else {
        ui.freeable_cursor()
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(total - visible_rows)
    };

    let mut lines: Vec<Line<'_>> = vec![Line::from(
        Span::from(format!(
            "{} freeable on the root filesystem",
            HumanSize(ledger.root_fs_freeable_bytes())
        ))
        .bold(),
    )];
    lines.push(Line::default());
    lines.extend(content.into_iter().skip(offset).take(visible_rows));
    lines.push(Line::default());
    if total > visible_rows {
        lines.push(Line::from(
            Span::from(format!(
                "row {} of {} — scroll with ↑↓/jk",
                ui.freeable_cursor() + 1,
                total
            ))
            .dim(),
        ));
    }
    lines.push(Line::from("f/Esc closes".dim()));

    render_floating_modal(frame, ctx, area, width, lines, " freeable ", theme::INFO);
    total
}

/// Shared floating-modal chrome (centered `Clear` + bordered `Paragraph`)
/// for the freeable panel's two shapes (empty state / populated content).
fn render_floating_modal(
    frame: &mut Frame<'_>,
    ctx: &RenderCtx,
    area: Rect,
    width: u16,
    lines: Vec<Line<'_>>,
    title: &'static str,
    border: theme::Slot,
) {
    let height = (lines.len() as u16 + 2).min(area.height);
    let modal = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, modal);
    let dialog = Paragraph::new(lines).block(
        card_block(ctx)
            .title(title)
            .border_style(Style::new().fg(ctx.theme.color(border))),
    );
    frame.render_widget(dialog, modal);
}

/// Centered `?` cheatsheet: every keyboard shortcut and mouse action,
/// read straight from [`keymap`] — the same tables `handle_key` dispatch
/// and the mouse handlers implement, so this can't drift from what the
/// keys actually do (see the `keymap` module doc).
fn draw_cheatsheet_modal(frame: &mut Frame<'_>, ctx: &RenderCtx) {
    let theme = &ctx.theme;
    let key_line = |keys: &str, action: &str| -> Line<'static> {
        Line::from(vec![
            Span::from(format!("  {keys:<24}")).fg(theme.color(theme::ACCENT)),
            Span::from(action.to_owned()),
        ])
    };
    let heading = |text: &'static str| -> Line<'static> {
        Line::from(Span::from(text).bold().fg(theme.color(theme::INFO)))
    };

    let mut lines: Vec<Line<'_>> = vec![heading("Keyboard")];
    for key in keymap::SIMPLE {
        lines.push(key_line(key.keys, key.action));
    }
    for key in keymap::EXTRA {
        lines.push(key_line(key.keys, key.action));
    }
    lines.push(Line::default());
    lines.push(heading("Mouse"));
    for key in keymap::MOUSE {
        lines.push(key_line(key.keys, key.action));
    }
    lines.push(Line::default());
    lines.push(Line::from("? or Esc closes".italic().dim()));

    let area = frame.area();
    let width = (lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4)
        .min(area.width.saturating_sub(2));
    // Cheatsheet content is fixed-size (not scrollable, unlike the review
    // list): on a too-short terminal it simply clips to what fits rather
    // than panicking or growing past the frame.
    let height = (lines.len() as u16 + 2).min(area.height);
    lines.truncate(height.saturating_sub(2) as usize);
    let modal = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, modal);
    let dialog = Paragraph::new(lines).block(
        card_block(ctx)
            .title(" keys & mouse ")
            .border_style(Style::new().fg(theme.color(theme::INFO))),
    );
    frame.render_widget(dialog, modal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use camembert_core::view::{DirTotals, Row, ScanStats, ViewSnapshot};
    use caps::ColorLevel;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_snapshot() -> Arc<ViewSnapshot> {
        let row = |name: &[u8], disk: u64, is_dir: bool| Row {
            name: name.into(),
            node: NodeId::from_raw(0),
            dir: None,
            is_dir,
            apparent: disk,
            disk,
            items: 3,
            errors: u64::from(name == b"locked"),
            state: if is_dir {
                RowState::Scanning
            } else {
                RowState::File
            },
            mtime: 1_000_000,
        };
        Arc::new(ViewSnapshot {
            generation: 1,
            dir: DirId::from_raw(0),
            parent: None,
            path: PathBuf::from("/scan/root"),
            rows: vec![
                row(b"big", 6000, true),
                row(b"mid", 3000, false),
                row(b"locked", 500, true),
                row(b"tiny", 1, false),
            ],
            totals: DirTotals {
                apparent: 10_000,
                disk: 10_000,
                items: 12,
                errors: 1,
            },
            stats: ScanStats {
                entries: 12,
                dirs: 3,
                errors: 1,
                disk_bytes: 10_000,
                elapsed: Duration::from_millis(500),
                root_complete: false,
            },
            hardlink_inodes: 2,
            degraded: false,
        })
    }

    /// A scan-complete snapshot with distinct node ids per row (unlike
    /// [`sample_snapshot`], which shares node 0 across every row and is
    /// therefore unsuitable for tests that mark individual entries) —
    /// backs the basket/review/cheatsheet render tests below.
    fn markable_snapshot() -> Arc<ViewSnapshot> {
        let row = |node: u32, name: &[u8], disk: u64| Row {
            name: name.into(),
            node: NodeId::from_raw(node),
            dir: None,
            is_dir: false,
            apparent: disk,
            disk,
            items: 1,
            errors: 0,
            state: RowState::File,
            mtime: 0,
        };
        Arc::new(ViewSnapshot {
            generation: 1,
            dir: DirId::from_raw(0),
            parent: None,
            path: PathBuf::from("/scan/root"),
            rows: vec![row(1, b"big", 6000), row(2, b"mid", 3000)],
            totals: DirTotals {
                apparent: 9000,
                disk: 9000,
                items: 2,
                errors: 0,
            },
            stats: ScanStats {
                entries: 2,
                dirs: 0,
                errors: 0,
                disk_bytes: 9000,
                elapsed: Duration::from_millis(500),
                root_complete: true,
            },
            hardlink_inodes: 0,
            degraded: false,
        })
    }

    fn ctx(glyphs: GlyphLevel, color: ColorLevel) -> RenderCtx {
        RenderCtx {
            caps: Caps { color, glyphs },
            theme: Theme::new(ThemeName::TokyoNight, color),
            disk: Some(DiskSpace {
                capacity: 100_000,
                used: 40_000,
            }),
            animate: true,
            no_proc_sweep: false,
            flat_config: FlatConfig::default(),
        }
    }

    /// A disabled `Motion` for tests that do not care about animation:
    /// bars/donut always render at their exact target value, matching
    /// what every pre-slice-5 assertion here already expected.
    fn no_motion() -> anim::Motion {
        anim::Motion::new(false)
    }

    /// D6: the palette modal (query mode empty/typed/erroring, command
    /// mode, mid-scan, with saved queries and an active filter) renders
    /// without panicking at every terminal size — the palette is
    /// entirely new drawing code (the cursor span, the parse-error
    /// lines, the command/saved-query lists) that no other render test
    /// exercises.
    #[test]
    fn palette_modal_never_panics_across_sizes_and_states() {
        let sizes = [(120, 35), (80, 24), (40, 10), (10, 5), (3, 2), (1, 1)];
        let ctx = ctx(GlyphLevel::Sextant, ColorLevel::Truecolor);
        let saved_queries = [("big_logs".to_owned(), "*.log >100M".to_owned())];

        for (width, height) in sizes {
            for (prefill, phase, saved) in [
                (None, &Phase::Transitioning, &[][..]),
                (Some("*.log"), &Phase::Transitioning, &[][..]),
                (Some("a;b"), &Phase::Transitioning, &[][..]), // parse error
                (Some(">flat"), &Phase::Transitioning, &[][..]), // command mode
                (None, &Phase::Transitioning, &saved_queries[..]), // saved queries
            ] {
                let mut ui = UiState::new(sample_snapshot());
                ui.open_palette(prefill);
                let mut table_state = TableState::default();
                let mut motion = no_motion();
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        draw(
                            frame,
                            &ui,
                            phase,
                            &mut table_state,
                            '⠋',
                            None,
                            &[],
                            &mut motion,
                            &ctx,
                            true, // fold in flight: exercises the spinner path too
                            saved,
                        );
                    })
                    .unwrap();
            }
        }
    }

    /// The cockpit renders without panicking at every size and capability
    /// rung — including degenerate terminals ("no panics at tiny sizes").
    #[test]
    fn draw_never_panics_across_sizes_and_caps() {
        let sizes = [
            (120, 35),
            (100, 30),
            (80, 24),
            (40, 10),
            (10, 5),
            (3, 2),
            (1, 1),
        ];
        let rungs = [
            (GlyphLevel::Sextant, ColorLevel::Truecolor),
            (GlyphLevel::HalfBlock, ColorLevel::Ansi256),
            (GlyphLevel::HalfBlock, ColorLevel::Ansi16),
            (GlyphLevel::Ascii, ColorLevel::Mono),
        ];
        for (width, height) in sizes {
            for (glyphs, color) in rungs {
                let ctx = ctx(glyphs, color);
                let ui = UiState::new(sample_snapshot());
                let mut table_state = TableState::default();
                let mut motion = no_motion();
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        draw(
                            frame,
                            &ui,
                            &Phase::Transitioning,
                            &mut table_state,
                            '⠋',
                            Some("note"),
                            &[],
                            &mut motion,
                            &ctx,
                            false,
                            &[],
                        );
                    })
                    .unwrap();
            }
        }
    }

    /// D3: the flat top-files and breakdown tables (and their donut)
    /// render without panicking at every size/capability rung, both with
    /// a populated summary and with none at all yet (mode entered before
    /// the first summary arrives) — mirrors
    /// `draw_never_panics_across_sizes_and_caps` for the two new modes.
    #[test]
    fn flat_and_breakdown_modes_never_panic_across_sizes_and_caps() {
        let sizes = [
            (120, 35),
            (100, 30),
            (80, 24),
            (40, 10),
            (10, 5),
            (3, 2),
            (1, 1),
        ];
        let rungs = [
            (GlyphLevel::Sextant, ColorLevel::Truecolor),
            (GlyphLevel::Ascii, ColorLevel::Mono),
        ];
        for populated in [false, true] {
            for mode_key in [KeyCode::Char('t'), KeyCode::Char('b')] {
                for (width, height) in sizes {
                    for (glyphs, color) in rungs {
                        let ctx = ctx(glyphs, color);
                        let mut ui = UiState::new(sample_snapshot());
                        keymap::dispatch_simple(mode_key, &mut ui);
                        if populated {
                            ui.set_flat_summary(flat_summary_test_fixture());
                        }
                        let mut table_state = TableState::default();
                        let mut motion = no_motion();
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal
                            .draw(|frame| {
                                draw(
                                    frame,
                                    &ui,
                                    &Phase::Transitioning,
                                    &mut table_state,
                                    '⠋',
                                    None,
                                    &[],
                                    &mut motion,
                                    &ctx,
                                    false,
                                    &[],
                                );
                            })
                            .unwrap();
                    }
                }
            }
        }
    }

    fn flat_summary_test_fixture() -> Arc<flat::FlatSummary> {
        use camembert_core::flat::{GroupTotal, PatternKind, RestTotal, TopFile};
        Arc::new(flat::FlatSummary {
            groups: vec![GroupTotal {
                label: "node_modules".to_owned(),
                kind: PatternKind::Dir,
                apparent: 100,
                disk: 100,
                entries: 2,
            }],
            rest: RestTotal {
                apparent: 50,
                disk: 50,
                entries: 1,
            },
            top_files: vec![
                TopFile {
                    node: NodeId::from_raw(1),
                    name: "file1".into(),
                    disk: 900,
                    hardlink: false,
                },
                TopFile {
                    node: NodeId::from_raw(2),
                    name: "file2".into(),
                    disk: 500,
                    hardlink: true,
                },
            ],
            truncated: true,
            provisional: false,
            epoch: 0,
        })
    }

    /// Wide terminal: the full side wheel panel renders half-block
    /// pixels well below the header. Narrow terminal (design slice 5):
    /// the side panel disappears — no half-block pixels below the header
    /// row — and a compact mini-donut renders in the header row instead.
    #[test]
    fn wheel_collapses_to_a_header_mini_donut_below_the_width_threshold() {
        let draw_at = |width: u16| -> ratatui::buffer::Buffer {
            let ctx = ctx(GlyphLevel::HalfBlock, ColorLevel::Truecolor);
            let ui = UiState::new(sample_snapshot());
            let mut table_state = TableState::default();
            let mut motion = no_motion();
            let mut terminal = Terminal::new(TestBackend::new(width, 35)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &ui,
                        &Phase::Transitioning,
                        &mut table_state,
                        '⠋',
                        None,
                        &[],
                        &mut motion,
                        &ctx,
                        false,
                        &[],
                    );
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let has_half_block_in_row = |buffer: &ratatui::buffer::Buffer, width: u16, row: u16| {
            let content = buffer.content();
            (0..width).any(|col| {
                let idx = row as usize * width as usize + col as usize;
                matches!(content.get(idx).map(|c| c.symbol()), Some("▀") | Some("▄"))
            })
        };

        let wide = draw_at(120);
        assert!(
            (5..30).any(|row| has_half_block_in_row(&wide, 120, row)),
            "full donut panel renders below the header on a wide terminal"
        );

        let narrow = draw_at(80);
        assert!(
            !(5..30).any(|row| has_half_block_in_row(&narrow, 80, row)),
            "no side panel below 100 columns"
        );
        assert!(
            has_half_block_in_row(&narrow, 80, 0),
            "mini-donut renders in the header row instead"
        );
    }

    /// [`wheel_layout`]'s threshold and precedence, pure and terminal-free
    /// (design slice 5): the width boundary matches
    /// [`MIN_WHEEL_TERMINAL_WIDTH`] exactly, and ASCII/zen both override
    /// width entirely.
    #[test]
    fn wheel_layout_threshold_and_precedence() {
        assert_eq!(
            wheel_layout(MIN_WHEEL_TERMINAL_WIDTH, false, false),
            WheelLayout::Full,
            "exactly at the threshold: full panel"
        );
        assert_eq!(
            wheel_layout(MIN_WHEEL_TERMINAL_WIDTH - 1, false, false),
            WheelLayout::Mini,
            "one column short: mini donut"
        );
        assert_eq!(
            wheel_layout(0, false, false),
            WheelLayout::Mini,
            "degenerate width still gets a (clipped) mini donut, not a panic"
        );
        assert_eq!(
            wheel_layout(200, true, false),
            WheelLayout::Hidden,
            "ASCII: no wheel at any width, full or mini"
        );
        assert_eq!(
            wheel_layout(200, false, true),
            WheelLayout::Hidden,
            "zen mode: no wheel at any width, even one wide enough for Full"
        );
        assert_eq!(
            wheel_layout(50, true, true),
            WheelLayout::Hidden,
            "ascii and zen both hidden: still just Hidden, not a panic/conflict"
        );
    }

    /// [`cards_and_gauge_heights`] collapses both rows to zero in zen
    /// mode and keeps their normal heights otherwise (design slice 5).
    #[test]
    fn cards_and_gauge_heights_collapse_in_zen_mode() {
        assert_eq!(cards_and_gauge_heights(false), (3, 1));
        assert_eq!(cards_and_gauge_heights(true), (0, 0));
    }

    /// `z` zen mode (design slice 5): metric cards, disk gauge and the
    /// donut wheel all disappear — header, table and footer remain. The
    /// errors-card hit-test area is `None` too, consistent with nothing
    /// being drawn there (mouse hit-testing must match what's on screen).
    #[test]
    fn zen_mode_hides_cards_gauge_and_wheel() {
        // `markable_snapshot` (unlike `sample_snapshot`) has no hardlinks
        // and a completed scan, so the footer's own "provisional totals
        // (hardlinks)" note never fires and can't be confused with the
        // "total" metric card below.
        let ctx = ctx(GlyphLevel::HalfBlock, ColorLevel::Truecolor);
        let mut ui = UiState::new(markable_snapshot());
        let mut table_state = TableState::default();
        let mut motion = no_motion();
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();

        let mut geometry = FrameGeometry::default();
        terminal
            .draw(|frame| {
                geometry = draw(
                    frame,
                    &ui,
                    &Phase::Transitioning,
                    &mut table_state,
                    '⠋',
                    None,
                    &[],
                    &mut motion,
                    &ctx,
                    false,
                    &[],
                );
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        assert!(content.contains("total"), "normal view: cards present");
        assert!(content.contains("disk"), "normal view: gauge present");
        assert!(content.contains('▀'), "normal view: wheel present");

        ui.toggle_zen();
        terminal
            .draw(|frame| {
                geometry = draw(
                    frame,
                    &ui,
                    &Phase::Transitioning,
                    &mut table_state,
                    '⠋',
                    None,
                    &[],
                    &mut motion,
                    &ctx,
                    false,
                    &[],
                );
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        assert!(!content.contains("total"), "zen: no metric cards");
        assert!(!content.contains("disk"), "zen: no disk gauge");
        assert!(!content.contains('▀'), "zen: no wheel, full or mini");
        assert!(
            geometry.errors_card.is_none(),
            "zen: no errors card to hit-test"
        );
    }

    /// ASCII rung: no wheel, `#` bars, plain borders — and still no
    /// non-ASCII glyph anywhere outside the footer's fixed key hints.
    #[test]
    fn ascii_rung_renders_hash_bars() {
        let ctx = ctx(GlyphLevel::Ascii, ColorLevel::Mono);
        let ui = UiState::new(sample_snapshot());
        let mut table_state = TableState::default();
        let mut motion = no_motion();
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &ui,
                    &Phase::Transitioning,
                    &mut table_state,
                    '|',
                    None,
                    &[],
                    &mut motion,
                    &ctx,
                    false,
                    &[],
                );
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        assert!(content.contains('#'), "ASCII proportion bars");
        assert!(!content.contains('▀'), "no wheel on the ASCII rung");
        assert!(!content.contains('█'), "no block glyphs on the ASCII rung");
    }

    fn render(ui: &UiState, toasts: &[String], flash: Option<&str>) -> String {
        let ctx = ctx(GlyphLevel::HalfBlock, ColorLevel::Truecolor);
        let mut table_state = TableState::default();
        let mut motion = no_motion();
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    ui,
                    &Phase::Transitioning,
                    &mut table_state,
                    '⠋',
                    flash,
                    toasts,
                    &mut motion,
                    &ctx,
                    false,
                    &[],
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect()
    }

    /// No marks: no basket strip glyph anywhere (design slice 4 — the
    /// layout must not jump for users who never mark anything). One
    /// mark: the strip shows up with the count and size.
    #[test]
    fn basket_strip_appears_only_once_something_is_marked() {
        let empty = UiState::new(markable_snapshot());
        assert!(
            !render(&empty, &[], None).contains("basket:"),
            "nothing marked: no strip"
        );

        let mut marked = UiState::new(markable_snapshot());
        marked.toggle_mark().unwrap(); // "big", 6000 disk bytes
        let content = render(&marked, &[], None);
        assert!(content.contains("basket:"), "one mark: strip shown");
        assert!(content.contains("1 item"), "singular noun for one entry");
    }

    #[test]
    fn basket_strip_pluralizes_and_sums_several_marks() {
        let mut ui = UiState::new(markable_snapshot());
        ui.toggle_mark().unwrap();
        ui.toggle_mark().unwrap();
        let content = render(&ui, &[], None);
        assert!(content.contains("2 items"), "plural noun for two entries");
    }

    /// Toasts render top-right and are skipped outright while the
    /// confirm modal is open (must not obstruct it).
    #[test]
    fn toasts_render_and_are_suppressed_under_the_confirm_modal() {
        let ui = UiState::new(markable_snapshot());
        let toasts = vec!["dump written: /tmp/x.cmbt".to_owned()];
        let content = render(&ui, &toasts, None);
        assert!(content.contains("dump written"));

        let mut confirming = UiState::new(markable_snapshot());
        confirming.toggle_mark().unwrap();
        confirming.open_confirm(0, None);
        let content = render(&confirming, &toasts, None);
        assert!(
            !content.contains("dump written"),
            "confirm modal open: toast suppressed"
        );
    }

    /// The review list renders the marked path and its "row N of M" note
    /// once there are more marks than fit — and never panics at
    /// degenerate terminal sizes.
    #[test]
    fn review_modal_renders_marked_paths() {
        let mut ui = UiState::new(markable_snapshot());
        ui.toggle_mark().unwrap();
        ui.toggle_mark().unwrap();
        assert!(ui.open_review());
        let content = render(&ui, &[], None);
        assert!(content.contains("review marked entries"));
        assert!(content.contains("big"));
        assert!(content.contains("mid"));

        for (width, height) in [(120, 35), (40, 10), (10, 5), (3, 2), (1, 1)] {
            let ctx = ctx(GlyphLevel::HalfBlock, ColorLevel::Truecolor);
            let mut table_state = TableState::default();
            let mut motion = no_motion();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &ui,
                        &Phase::Transitioning,
                        &mut table_state,
                        '⠋',
                        None,
                        &[],
                        &mut motion,
                        &ctx,
                        false,
                        &[],
                    );
                })
                .unwrap();
        }
    }

    /// The `?` cheatsheet lists entries from every `keymap` table (the
    /// generated-from-one-table guarantee, visible at the render layer)
    /// and never panics at degenerate sizes.
    #[test]
    fn cheatsheet_modal_lists_keymap_entries() {
        let mut ui = UiState::new(markable_snapshot());
        ui.open_cheatsheet();
        let content = render(&ui, &[], None);
        assert!(content.contains("keys & mouse"));
        // One representative row from each of the three tables.
        assert!(content.contains("move down"), "SIMPLE entry present");
        assert!(content.contains("delete the marked"), "EXTRA entry present");
        assert!(content.contains("scroll the cursor"), "MOUSE entry present");

        for (width, height) in [(120, 35), (40, 10), (10, 5), (3, 2), (1, 1)] {
            let ctx = ctx(GlyphLevel::HalfBlock, ColorLevel::Truecolor);
            let mut table_state = TableState::default();
            let mut motion = no_motion();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &ui,
                        &Phase::Transitioning,
                        &mut table_state,
                        '⠋',
                        None,
                        &[],
                        &mut motion,
                        &ctx,
                        false,
                        &[],
                    );
                })
                .unwrap();
        }
    }

    // ---- `handle_key` modal routing (design slice 4) ----
    //
    // These drive the real key handler over a real (tiny, tempdir) scan
    // instead of hand-built fixtures — `Phase::Done` needs an actual
    // `ScanOutcome`, and the point of these tests is exactly the routing
    // glue in `handle_key`/`open_delete_confirm`/`try_open_review`, not
    // the pure `UiState` methods already covered in `state`'s own tests.

    use camembert_core::scan::{ScanOptions, Scanner};
    use camembert_core::view;

    /// Scan `path` to completion (2 threads is plenty for a handful of
    /// files) and finalize hardlinks, matching what `finish_scan` does
    /// before the frozen arena starts serving views.
    fn scan_dir(path: &Path) -> ScanOutcome {
        let mut outcome = Scanner::new(ScanOptions {
            threads: 2,
            ..ScanOptions::default()
        })
        .scan(path)
        .expect("scan of a tempdir never fails");
        outcome.finalize_hardlinks();
        outcome
    }

    /// A `Phase::Done` UI over a tempdir with one file, cursor already on
    /// it — ready for `toggle_mark`/`handle_key` tests.
    fn done_ui_with_one_file() -> (UiState, Phase) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a"), b"hello").expect("write fixture file");
        let outcome = scan_dir(tmp.path());
        let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
        let snapshot = view::build_snapshot(
            outcome.tree(),
            outcome.root(),
            1,
            stats,
            outcome.hardlink_inodes,
            false,
        );
        let ui = UiState::new(Arc::new(snapshot));
        (ui, Phase::Done(Arc::new(RwLock::new(outcome))))
    }

    fn press(
        code: KeyCode,
        ui: &mut UiState,
        phase: &mut Phase,
        generation: &mut u64,
        flash: &mut Flash,
        toasts: &mut ToastQueue,
    ) -> Action {
        // A fresh runtime per call is fine here: none of the pre-existing
        // routing tests exercise the palette (their own dedicated tests
        // below construct/thread a `PaletteRuntime` explicitly instead).
        let mut palette_rt = PaletteRuntime::new(Vec::new());
        let flat_config = FlatConfig::default();
        handle_key(
            code,
            KeyModifiers::NONE,
            ui,
            phase,
            generation,
            flash,
            toasts,
            false, // no_proc_sweep: not under test here
            &mut palette_rt,
            &flat_config,
        )
    }

    /// `v` opens the review list; `D` from inside it closes the list and
    /// opens the same delete-confirmation modal `D` opens from the main
    /// view — "D from within the review list should work too".
    #[test]
    fn v_opens_review_and_d_from_within_it_opens_confirm() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        ui.toggle_mark().expect("marking the only row succeeds");

        press(
            KeyCode::Char('v'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.review().is_some(), "v opened the review list");

        press(
            KeyCode::Char('D'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.review().is_none(), "D closed the review list");
        assert!(ui.confirm().is_some(), "D opened the confirm modal");
    }

    // ---- D3 flat view + pattern breakdown: mode/Esc state machine,
    // epoch recompute on delete, flat-row jump mapping ----

    /// A `Phase::Done` UI over a tempdir with two files of very different
    /// sizes ("big", "small"), cursor at the top — ready for flat-view
    /// tests that need a real arena (jump-to-dir, marking, epoch
    /// recompute).
    /// Returns the `TempDir` guard too (unlike `done_ui_with_one_file`,
    /// which never actually executes a real deletion in its callers): a
    /// test that presses `y` for real needs the directory to still exist
    /// on disk at that point, and `TempDir` removes it on `Drop` — so the
    /// guard must outlive the whole test, not just this constructor.
    fn done_ui_with_two_files() -> (UiState, Phase, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("big"), vec![0u8; 8192]).expect("write big");
        std::fs::write(tmp.path().join("small"), vec![0u8; 16]).expect("write small");
        let outcome = scan_dir(tmp.path());
        let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
        let snapshot = view::build_snapshot(
            outcome.tree(),
            outcome.root(),
            1,
            stats,
            outcome.hardlink_inodes,
            false,
        );
        let ui = UiState::new(Arc::new(snapshot));
        (ui, Phase::Done(Arc::new(RwLock::new(outcome))), tmp)
    }

    /// D4 composition, end-to-end: applying a real filter over a real
    /// two-file scan hides the non-matching row from the tree table and
    /// renders the pill, without panicking — the render-level counterpart
    /// to `state`'s `directory_marks_are_refused_under_an_active_filter...`
    /// and `a_filter_that_matches_nothing_hides_every_row...` tests, which
    /// cover the same composition at the `UiState` level.
    #[test]
    fn filtered_render_shows_the_pill_and_hides_the_non_matching_row() {
        let (mut ui, phase, _tmp) = done_ui_with_two_files();
        let parsed = query::parse("big");
        assert!(parsed.errors.is_empty());
        let Phase::Done(lock) = &phase else {
            panic!("done_ui_with_two_files always returns Phase::Done")
        };
        let outcome = read_outcome(lock);
        let hardlinks = HardlinkIndex::build(&outcome, 0);
        let result = query::apply(
            outcome.tree(),
            &parsed.query,
            &FlatConfig::default().patterns,
            &hardlinks,
            &ApplyOptions {
                cap: 10,
                epoch: 0,
                now_unix: 0,
                threads: 1,
            },
        );
        drop(outcome);
        ui.set_active_filter("big".to_owned(), Arc::new(result));
        assert_eq!(ui.row_count(), 1, "only the matching file survives");

        let ctx = ctx(GlyphLevel::Sextant, ColorLevel::Truecolor);
        let mut table_state = TableState::default();
        let mut motion = no_motion();
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &ui,
                    &phase,
                    &mut table_state,
                    '⠋',
                    None,
                    &[],
                    &mut motion,
                    &ctx,
                    false,
                    &[],
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("big"), "the matching file is shown: {text}");
        assert!(
            !text.contains("small"),
            "the non-matching file must be hidden: {text}"
        );
        assert!(
            text.contains("matched"),
            "the pill/cards mention the match: {text}"
        );
    }

    /// `t`/`b` toggle into and back out of their modes through the real
    /// key handler, and `Esc` is contextual: it leaves a flat/breakdown
    /// mode instead of quitting, but still quits from tree view (D3).
    #[test]
    fn t_b_and_contextual_esc_state_machine_through_handle_key() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        let mut press_code = |code, ui: &mut UiState, phase: &mut Phase| {
            press(code, ui, phase, &mut generation, &mut flash, &mut toasts)
        };

        assert_eq!(ui.mode(), ViewMode::Tree);
        press_code(KeyCode::Char('t'), &mut ui, &mut phase);
        assert_eq!(ui.mode(), ViewMode::FlatTop);

        // Esc leaves the mode — does not quit.
        let action = press_code(KeyCode::Esc, &mut ui, &mut phase);
        assert!(matches!(action, Action::Continue), "Esc did not quit");
        assert_eq!(ui.mode(), ViewMode::Tree, "Esc left the mode");

        // From tree view, Esc quits.
        let action = press_code(KeyCode::Esc, &mut ui, &mut phase);
        assert!(matches!(action, Action::Quit), "Esc quits from tree view");

        // `b` toggles breakdown the same way; `q` always quits, mode or
        // not (D3: "q always quits").
        press_code(KeyCode::Char('b'), &mut ui, &mut phase);
        assert_eq!(ui.mode(), ViewMode::Breakdown);
        let action = press_code(KeyCode::Char('q'), &mut ui, &mut phase);
        assert!(matches!(action, Action::Quit), "q quits even mid-mode");
    }

    /// Sort keys the active mode has no column for are refused with a
    /// flash instead of silently reordering nothing (D3): `m` (mtime) in
    /// breakdown mode, where a group total has no mtime.
    #[test]
    fn sort_key_not_applicable_in_a_mode_flashes_instead_of_applying() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        ui.toggle_breakdown();
        let before = ui.sort();

        press(
            KeyCode::Char('m'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert_eq!(ui.sort(), before, "mtime sort did not apply in breakdown");
        assert_eq!(flash.current(), Some(SORT_NOT_APPLICABLE));
    }

    /// The attack's exact scenario (finding 1): mark a flat row, delete it
    /// *from within* flat mode, and confirm the very next render-time
    /// check recomputes the summary — the deleted file must never appear
    /// as still occupying space.
    #[test]
    fn epoch_recompute_removes_a_file_deleted_from_within_flat_mode() {
        let (mut ui, mut phase, _tmp) = done_ui_with_two_files();
        let flat_config = FlatConfig::default();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        let mut press_code = |code, ui: &mut UiState, phase: &mut Phase| {
            press(code, ui, phase, &mut generation, &mut flash, &mut toasts)
        };

        press_code(KeyCode::Char('t'), &mut ui, &mut phase);
        assert_eq!(ui.mode(), ViewMode::FlatTop);
        ensure_flat_summary_fresh(&phase, &flat_config, &mut ui);
        let summary = ui.flat_summary().expect("summary computed post-scan");
        assert!(!summary.provisional);
        assert_eq!(summary.top_files.len(), 2, "both files present up front");

        // Default sort is disk descending: the cursor starts on "big".
        // Mark it, delete it, all without ever leaving flat mode.
        press_code(KeyCode::Char(' '), &mut ui, &mut phase);
        assert_eq!(ui.marked_summary().map(|(n, _)| n), Some(1));
        press_code(KeyCode::Char('D'), &mut ui, &mut phase);
        assert!(ui.confirm().is_some(), "confirm modal opened");
        press_code(KeyCode::Char('y'), &mut ui, &mut phase);
        assert!(ui.confirm().is_none(), "deletion executed");

        // The render-time epoch check (`event_loop` step 3.5) must
        // recompute before the very next frame draws.
        ensure_flat_summary_fresh(&phase, &flat_config, &mut ui);
        let summary = ui
            .flat_summary()
            .expect("summary recomputed after the delete");
        assert_eq!(
            summary.top_files.len(),
            1,
            "the deleted file is gone from the very next frame"
        );
        assert!(!summary.provisional, "authoritative, not a stale snapshot");
    }

    /// Enter on a flat top-files row jumps to its containing directory,
    /// cursor on the file itself (D3) — exercised over a real nested
    /// arena so the ancestor-chain walk and the node lookup are both real.
    #[test]
    fn enter_on_a_flat_row_jumps_to_its_containing_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("sub")).expect("mkdir sub");
        std::fs::write(tmp.path().join("sub/big"), vec![0u8; 8192]).expect("write big");
        std::fs::write(tmp.path().join("tiny"), vec![0u8; 4]).expect("write tiny");
        let outcome = scan_dir(tmp.path());
        let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
        let snapshot = view::build_snapshot(
            outcome.tree(),
            outcome.root(),
            1,
            stats,
            outcome.hardlink_inodes,
            false,
        );
        let mut ui = UiState::new(Arc::new(snapshot));
        let mut phase = Phase::Done(Arc::new(RwLock::new(outcome)));
        let flat_config = FlatConfig::default();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        let mut press_code = |code, ui: &mut UiState, phase: &mut Phase| {
            press(code, ui, phase, &mut generation, &mut flash, &mut toasts)
        };

        press_code(KeyCode::Char('t'), &mut ui, &mut phase);
        ensure_flat_summary_fresh(&phase, &flat_config, &mut ui);
        // Disk-desc default: "sub/big" is the cursor's row.
        press_code(KeyCode::Enter, &mut ui, &mut phase);
        assert_eq!(ui.mode(), ViewMode::Tree, "jumping leaves the mode");

        // Resolve the pending nav the same way `event_loop` step 3 would.
        let dir = ui.pending_nav().expect("jump requested a nav");
        serve_local(&phase, dir, &mut generation, &mut ui);
        assert_eq!(
            ui.snapshot().path.file_name().and_then(|n| n.to_str()),
            Some("sub"),
            "landed in the containing directory"
        );
        assert_eq!(
            &*ui.selected().expect("cursor on a row").name,
            b"big",
            "cursor on the file itself, not the top of the listing"
        );
    }

    /// `?` opens the cheatsheet from the main view; `?`/`Esc` closes it.
    #[test]
    fn cheatsheet_opens_and_closes() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());

        press(
            KeyCode::Char('?'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.cheatsheet_open());

        press(
            KeyCode::Esc,
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(!ui.cheatsheet_open());
    }

    /// Modal precedence (confirm > review > cheatsheet): once the confirm
    /// modal is open, every key belongs to it alone — `v`/`?` do not leak
    /// through and open another modal underneath.
    #[test]
    fn confirm_modal_captures_keys_that_would_open_other_modals() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        ui.toggle_mark().expect("marking the only row succeeds");
        open_delete_confirm(&mut ui, &phase, &mut flash, false);
        assert!(ui.confirm().is_some());

        // `v` is not `y`: the confirm modal treats it as "cancel", not as
        // a request to open the review list underneath.
        press(
            KeyCode::Char('v'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.confirm().is_none(), "any non-y key cancels confirm");
        assert!(ui.review().is_none(), "and never opened review instead");
    }

    /// While the review list is open, `?` is not handled by it (only
    /// move/unmark/`D`/`v`/`Esc` are) — it is silently ignored rather
    /// than leaking through to open the cheatsheet underneath.
    #[test]
    fn review_modal_does_not_leak_unhandled_keys_to_the_cheatsheet() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        ui.toggle_mark().expect("marking the only row succeeds");
        assert!(ui.open_review());

        press(
            KeyCode::Char('?'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.review().is_some(), "still in the review list");
        assert!(!ui.cheatsheet_open(), "? did not leak through to it");
    }

    // ---- `f` freeable panel (freeable phase 1) ----

    /// `f` opens the panel from the main view; `f`/`Esc` closes it, same
    /// shape as the cheatsheet's own open/close test.
    #[test]
    fn f_key_opens_and_closes_the_freeable_panel() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());

        press(
            KeyCode::Char('f'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.freeable_open(), "f opened the panel");

        press(
            KeyCode::Esc,
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(!ui.freeable_open(), "Esc closed it");
    }

    /// D5's precedence (confirm > review > freeable panel > cheatsheet):
    /// with the confirm modal open, `f` is just another non-`y` cancel key
    /// — it never leaks through to open the panel underneath.
    #[test]
    fn confirm_modal_captures_f_too() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        ui.toggle_mark().expect("marking the only row succeeds");
        open_delete_confirm(&mut ui, &phase, &mut flash, false);
        assert!(ui.confirm().is_some());

        press(
            KeyCode::Char('f'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.confirm().is_none(), "f is treated as a non-y cancel key");
        assert!(!ui.freeable_open(), "never opened the panel underneath");
    }

    /// While the freeable panel is open, `?` does not leak through to the
    /// cheatsheet underneath (same non-leaking guarantee as the review
    /// list's own test).
    #[test]
    fn freeable_panel_does_not_leak_unhandled_keys_to_the_cheatsheet() {
        let (mut ui, mut phase) = done_ui_with_one_file();
        let (mut generation, mut flash, mut toasts) = (1u64, Flash::new(), ToastQueue::new());
        ui.open_freeable_panel();

        press(
            KeyCode::Char('?'),
            &mut ui,
            &mut phase,
            &mut generation,
            &mut flash,
            &mut toasts,
        );
        assert!(ui.freeable_open(), "still in the freeable panel");
        assert!(!ui.cheatsheet_open(), "? did not leak through to it");
    }

    /// A real deleted-open file (same "gold case" technique as
    /// `camembert_core::freeable`'s own tests): once swept into the
    /// ledger, the disk gauge grows a clickable "· X.X GiB freeable"
    /// suffix, and the `f` panel lists the entry (grouped into the
    /// catch-all, since it lives outside the scanned tempdir).
    #[test]
    fn gauge_suffix_and_panel_show_a_real_deleted_open_file() {
        if !std::path::Path::new("/proc/self/fd").exists() {
            eprintln!("skipping: /proc/self/fd unavailable on this host");
            return;
        }
        let (mut ui, phase) = done_ui_with_one_file();

        let freeable_dir = tempfile::tempdir().expect("tempdir");
        let path = freeable_dir.path().join("gone.bin");
        let mut file = std::fs::File::create(&path).expect("create");
        std::io::Write::write_all(&mut file, &[0xABu8; 256 * 1024]).expect("write");
        std::io::Write::flush(&mut file).expect("flush");
        let root_dev = {
            use std::os::unix::fs::MetadataExt;
            file.metadata().expect("metadata").dev()
        };
        std::fs::remove_file(&path).expect("unlink while still open");

        let ledger = freeable::sweep(root_dev);
        assert!(
            ledger.root_fs_freeable_bytes() > 0,
            "our own deleted-open file should be swept up"
        );
        ui.set_freeable_ledger(ledger);

        let content = render(&ui, &[], None);
        assert!(
            content.contains("freeable"),
            "gauge suffix visible: {content}"
        );

        open_freeable_panel(&mut ui, &phase);
        assert!(ui.freeable_open());
        let content = render(&ui, &[], None);
        assert!(
            content.contains("gone.bin"),
            "panel lists the entry's evidence path: {content}"
        );
        assert!(
            content.contains("outside the scan"),
            "grouped into the catch-all: not under the scanned tempdir: {content}"
        );

        drop(file);
    }

    /// D6 amendment's primary real-world scenario, with a real open fd:
    /// marking a *directory* (a database's data directory, in spirit)
    /// whose own `(dev, ino)` is never open, but a file *inside* it is.
    /// The original files-only check would show no warning at all here —
    /// exactly the false reassurance the review verdict flagged. This
    /// confirms the path-prefix containment channel catches it.
    #[test]
    fn marked_directory_containment_finds_a_real_open_file_inside_it() {
        if !std::path::Path::new("/proc/self/fd").exists() {
            eprintln!("skipping: /proc/self/fd unavailable on this host");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir(&data_dir).expect("mkdir data");
        let mut file = std::fs::File::create(data_dir.join("hot.bin")).expect("create");
        std::io::Write::write_all(&mut file, b"still open, never unlinked").expect("write");

        let outcome = scan_dir(tmp.path());
        let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
        let snapshot = view::build_snapshot(
            outcome.tree(),
            outcome.root(),
            1,
            stats,
            outcome.hardlink_inodes,
            false,
        );
        let mut ui = UiState::new(Arc::new(snapshot));
        // The scan root's only child is the "data" directory: cursor
        // already on it.
        assert!(
            ui.selected().expect("one row").is_dir,
            "the only root row is the data directory"
        );
        ui.toggle_mark()
            .expect("marking the data directory succeeds");
        assert!(ui.marks()[0].is_dir, "marked the directory, not a file");

        let warning = pre_deletion_open_warning(&ui).expect(
            "an open file inside the marked directory must produce a warning \
             even though the directory's own (dev, ino) matches nothing",
        );
        assert_eq!(warning.entries_open, 0, "no marked *file* is directly open");
        assert_eq!(
            warning.contained_open, 1,
            "one open file found strictly under the marked directory"
        );
        let me = std::process::id();
        assert!(
            warning.top_holders.iter().any(|&(pid, _)| pid == me),
            "our own pid should be named as a holder: {:?}",
            warning.top_holders
        );

        drop(file);
    }

    /// A sibling directory named to collide at the byte level ("data-old"
    /// starts with "data") must never be treated as contained by a mark on
    /// "data" — same path-boundary rule as the panel's ancestor grouping,
    /// exercised here through the real containment code path.
    #[test]
    fn marked_directory_containment_respects_path_boundaries() {
        if !std::path::Path::new("/proc/self/fd").exists() {
            eprintln!("skipping: /proc/self/fd unavailable on this host");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let marked_dir = tmp.path().join("data");
        let sibling_dir = tmp.path().join("data-old");
        std::fs::create_dir(&marked_dir).expect("mkdir data");
        std::fs::create_dir(&sibling_dir).expect("mkdir data-old");
        // Open a file only under the byte-colliding sibling, never under
        // the marked directory itself.
        let mut file = std::fs::File::create(sibling_dir.join("cold.bin")).expect("create");
        std::io::Write::write_all(&mut file, b"unrelated").expect("write");

        let outcome = scan_dir(tmp.path());
        let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
        let snapshot = view::build_snapshot(
            outcome.tree(),
            outcome.root(),
            1,
            stats,
            outcome.hardlink_inodes,
            false,
        );
        let mut ui = UiState::new(Arc::new(snapshot));
        // Two rows now ("data", "data-old"); find and mark "data" only.
        let position = ui
            .rows_indexed()
            .position(|(_, row)| &*row.name == b"data")
            .expect("the data row exists");
        ui.select_at(position);
        ui.toggle_mark().expect("marking data succeeds");
        assert_eq!(ui.marks().len(), 1);
        assert_eq!(ui.marks()[0].path, marked_dir);

        let warning = pre_deletion_open_warning(&ui);
        assert!(
            warning.is_none(),
            "the open file lives under data-old, not data: no false containment match, got {warning:?}"
        );

        drop(file);
    }

    /// Before the sweep lands, the panel shows an explanatory empty state
    /// rather than nothing — and the message differs when
    /// `--no-proc-sweep`/`NO_PROC_SWEEP` is why there is no data at all.
    #[test]
    fn freeable_panel_empty_state_distinguishes_no_data_yet_from_disabled() {
        fn render_with(no_proc_sweep: bool, ui: &mut UiState) -> String {
            ui.open_freeable_panel();
            let mut ctx = ctx(GlyphLevel::HalfBlock, ColorLevel::Truecolor);
            ctx.no_proc_sweep = no_proc_sweep;
            let mut table_state = TableState::default();
            let mut motion = no_motion();
            let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        ui,
                        &Phase::Transitioning,
                        &mut table_state,
                        '⠋',
                        None,
                        &[],
                        &mut motion,
                        &ctx,
                        false,
                        &[],
                    );
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol().to_owned())
                .collect()
        }

        let mut enabled_ui = UiState::new(markable_snapshot());
        let content = render_with(false, &mut enabled_ui);
        assert!(
            content.contains("no data yet"),
            "enabled, nothing swept yet: {content}"
        );

        let mut disabled_ui = UiState::new(markable_snapshot());
        let content = render_with(true, &mut disabled_ui);
        assert!(
            content.contains("no-proc-sweep") || content.contains("disabled"),
            "--no-proc-sweep: says so explicitly: {content}"
        );
    }
}
