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
mod fmt;
mod keymap;
mod osc11;
mod state;
pub mod theme;
mod toast;
mod wheel;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

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
use camembert_core::scan::{LiveScan, ScanOutcome, Scanner};
use camembert_core::size::HumanSize;
use camembert_core::tree::{DirId, NodeId};
use camembert_core::view::{self, RowState, ViewSnapshot};

use caps::{Caps, GlyphLevel};
use fmt::DiskSpace;
use state::{
    ConfirmState, FrameGeometry, MarkRefusal, ReviewState, SortKey, TableGeometry, UiState,
    WheelGeometry, show_hardlink_note, show_updating_note,
};
use theme::{Theme, ThemeName};
use toast::ToastQueue;

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
    /// Scan over: this thread owns the frozen arena and serves its own
    /// navigation (see `Scanner::scan_live` for why no owner survives).
    Done(Box<ScanOutcome>),
    /// Transient marker while moving between the two.
    Transitioning,
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
/// light background, before the alternate screen opens.
pub fn run(
    scanner: Scanner,
    path: &Path,
    output: Option<PathBuf>,
    caps: Caps,
    animate: bool,
    theme_choice: Option<ThemeName>,
) -> io::Result<()> {
    info!(
        ?caps,
        animate,
        ?theme_choice,
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
    };
    let result = event_loop(&mut terminal, live, output, &ctx);
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
    // Last left-click's time and cell, for double-click detection —
    // independent of the click-already-selected-row shortcut.
    let mut last_click: Option<(Instant, u16, u16)> = None;
    // Bar/donut animation state (design slice 5) — see the `anim` module
    // doc. `ctx.animate` is `false` for `--no-motion`/`NO_MOTION`.
    let mut motion = anim::Motion::new(ctx.animate);

    loop {
        // 1. Input (drain everything pending; block at most one frame
        //    while something needs a timely redraw of its own accord —
        //    otherwise idle: a quiescent UI costs nothing between
        //    keypresses, design slice 5).
        let mut deadline = if needs_frequent_polling(&phase, &flash, &toasts, &motion) {
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
                    handle_mouse(mouse, &mut ui, &phase, &mut last_click);
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
            local_generation = ui.snapshot().generation;
            phase = Phase::Done(Box::new(outcome));
            // Re-view the current dir so states/totals show final values,
            // resolving any nav request the owner no longer serves.
            let dir = ui.pending_nav().unwrap_or(ui.snapshot().dir);
            serve_local(&phase, dir, &mut local_generation, &mut ui);
        }

        // 3. Snapshot for this frame (wait-free).
        match &phase {
            Phase::Scanning(_) => ui.apply_snapshot(bus.load()),
            Phase::Done(_) => {
                if let Some(dir) = ui.pending_nav() {
                    serve_local(&phase, dir, &mut local_generation, &mut ui);
                }
            }
            Phase::Transitioning => unreachable!("resolved in step 2"),
        }

        // 4. Render.
        table_state.select(if ui.selected().is_some() {
            Some(ui.cursor())
        } else {
            None
        });
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
                &mut table_state,
                spinner,
                flash_text.as_deref(),
                &toast_texts,
                &mut motion,
                ctx,
            );
        })?;
        // Recomputed every frame (design slice 3): mouse events hit-test
        // against exactly what is on screen right now.
        ui.set_geometry(geometry);
    }
}

/// Whether the render loop needs to keep polling at [`FRAME`] cadence
/// even without new input: a running scan (progress arrives off the
/// input stream), an in-flight bar/donut animation, or a toast/flash
/// that still needs to expire on schedule. `false` means nothing on
/// screen changes until the user does something, so the loop idles at
/// [`IDLE_POLL`] instead (design slice 5).
fn needs_frequent_polling(
    phase: &Phase,
    flash: &Flash,
    toasts: &ToastQueue,
    motion: &anim::Motion,
) -> bool {
    matches!(phase, Phase::Scanning(_))
        || motion.is_active()
        || flash.is_set()
        || !toasts.is_empty()
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

/// Modal precedence (design slice 4): confirm > review > cheatsheet, only
/// one open at a time, keys route to the open modal only. Each modal
/// branch below `return`s unconditionally, so the normal-mode match at
/// the bottom is only ever reached with none of them open — which is
/// also what keeps that invariant true: opening a modal from normal mode
/// can never happen while a higher-precedence one is up.
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    ui: &mut UiState,
    phase: &mut Phase,
    generation: &mut u64,
    flash: &mut Flash,
    toasts: &mut ToastQueue,
) -> Action {
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
                open_delete_confirm(ui, phase, flash);
            }
            KeyCode::Char('v') | KeyCode::Esc => ui.close_review(),
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
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Action::Quit,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => try_descend(ui, phase),
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => try_ascend(ui, phase),
        KeyCode::Char(' ') => try_toggle_mark(ui, phase, flash),
        KeyCode::Char('D') => open_delete_confirm(ui, phase, flash),
        KeyCode::Char('v') => try_open_review(ui, flash),
        // Every other key (movement, sort, `p`, `u`, `?`) is stateless
        // enough to live in the keymap dispatch table (`ui::keymap`) —
        // the single source the `?` cheatsheet also renders from.
        _ => {
            keymap::dispatch_simple(code, ui);
        }
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

/// Route a mouse event against the last frame's [`FrameGeometry`] (mouse
/// support, design slice 3). Inert while any modal is open — confirm,
/// review or cheatsheet (design slice 4) — they only listen to the
/// keyboard; a click through to a hidden row underneath would be
/// surprising.
fn handle_mouse(
    mouse: MouseEvent,
    ui: &mut UiState,
    phase: &Phase,
    last_click: &mut Option<(Instant, u16, u16)>,
) {
    if ui.confirm().is_some() || ui.review().is_some() || ui.cheatsheet_open() {
        return;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            handle_click(mouse.column, mouse.row, ui, phase, last_click);
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
) {
    if ui.geometry().errors_card_hit(col, row) {
        ui.press_sort(SortKey::Errors);
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
        // navigates, there is no separate "select" step.
        ui.select_at(position);
        try_descend(ui, phase);
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
    let already_selected = ui.selected().is_some() && ui.cursor() == position;
    let double_click = matches!(*last_click, Some((at, c, r)) if c == col && r == row && at.elapsed() < DOUBLE_CLICK);
    ui.select_at(position);
    if already_selected || double_click {
        try_descend(ui, phase);
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
fn try_toggle_mark(ui: &mut UiState, phase: &Phase, flash: &mut Flash) {
    if matches!(phase, Phase::Scanning(_)) {
        flash.set(DELETION_LOCKED);
        return;
    }
    match ui.toggle_mark() {
        Ok(()) => {}
        Err(MarkRefusal::ScanRunning) => flash.set(DELETION_LOCKED),
        Err(MarkRefusal::MountPoint) => {
            flash.set("mount points cannot be marked for deletion");
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

/// `D`: open the confirmation modal over the marked entries, computing
/// the hardlink warning from the frozen arena.
fn open_delete_confirm(ui: &mut UiState, phase: &Phase, flash: &mut Flash) {
    let Phase::Done(outcome) = phase else {
        flash.set(DELETION_LOCKED);
        return;
    };
    if ui.marked_summary().is_none() {
        flash.set("nothing marked — Space marks the row under the cursor");
        return;
    }
    let nodes: Vec<NodeId> = ui.marks().iter().map(|mark| mark.node).collect();
    let hardlinks = delete::hardlink_files_in(outcome, &nodes);
    ui.open_confirm(hardlinks);
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
    let Phase::Done(outcome) = phase else {
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
    let report = delete::delete_nodes(outcome, &nodes);
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
    // The viewed directory may sit inside a deleted subtree: climb to the
    // nearest surviving ancestor before rebuilding the view.
    let mut dir = ui.snapshot().dir;
    {
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
    let Phase::Done(outcome) = phase else {
        return;
    };
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
    table_state: &mut TableState,
    spinner: char,
    flash: Option<&str>,
    toasts: &[String],
    motion: &mut anim::Motion,
    ctx: &RenderCtx,
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
    let (cards_height, gauge_height) = cards_and_gauge_heights(ui.zen());
    let [
        header_area,
        cards_area,
        gauge_area,
        main_area,
        basket_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(cards_height),
        Constraint::Length(gauge_height),
        Constraint::Min(3),
        Constraint::Length(basket_height),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    // Identity colors: assigned once per frame from the snapshot rows (in
    // snapshot order), shared by the table bars/names and the wheel so
    // they can never disagree.
    let disks: Vec<u64> = snapshot.rows.iter().map(|row| row.disk).collect();
    let ranks = theme::assign_identity(&disks, theme::IDENTITY_LEN);

    let breadcrumb = draw_header(frame, header_area, ui, spinner, ctx);
    let errors_card = if ui.zen() {
        None
    } else {
        draw_metric_cards(frame, cards_area, snapshot, ctx)
    };
    if !ui.zen() {
        draw_disk_gauge(frame, gauge_area, snapshot, ctx);
    }

    // Main split: table (with selection card) left, wheel right — see
    // `wheel_layout` for the responsive-collapse/zen-mode rules (design
    // slice 5).
    let layout = wheel_layout(frame.area().width, ctx.ascii(), ui.zen());
    if layout == WheelLayout::Mini {
        draw_mini_donut(frame, header_area, ui, &ranks, motion, ctx);
    }
    let (left_area, wheel_area) = if layout == WheelLayout::Full {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main_area);
        (left, Some(right))
    } else {
        (main_area, None)
    };
    let show_selection_card = !ui.zen() && left_area.height >= 9;
    let (table_area, card_area) = if show_selection_card {
        let [table, card] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(left_area);
        (table, Some(card))
    } else {
        (left_area, None)
    };

    let bar_progress = motion.bar_progress();
    let table = draw_table(
        frame,
        table_area,
        ui,
        table_state,
        spinner,
        &ranks,
        bar_progress,
        ctx,
    );
    if let Some(card_area) = card_area {
        draw_selection_card(frame, card_area, ui, ctx);
    }
    let wheel =
        wheel_area.and_then(|wheel_area| draw_wheel(frame, wheel_area, ui, &ranks, motion, ctx));

    draw_basket_strip(frame, basket_area, ui, ctx);
    draw_footer(frame, footer_area, ui, flash, ctx);

    // Toasts must not obstruct the confirm modal (design slice 4): they
    // sit top-right of the main content, well clear of the centered
    // confirm dialog, but are skipped outright whenever it is open —
    // simpler than reasoning about overlap and correct for every
    // terminal size, not just the common ones.
    if ui.confirm().is_none() {
        draw_toasts(frame, main_area, toasts, ctx);
    }

    if let Some(confirm) = ui.confirm() {
        draw_confirm_modal(frame, ui, confirm, ctx);
    } else if let Some(review) = ui.review() {
        draw_review_modal(frame, ui, review, ctx);
    } else if ui.cheatsheet_open() {
        draw_cheatsheet_modal(frame, ctx);
    }

    FrameGeometry {
        table: Some(table),
        breadcrumb_row: header_area.y,
        breadcrumb,
        errors_card,
        wheel,
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
    let header = Line::from(vec![
        Span::from(" "),
        Span::from(signature).fg(theme.color(theme::ACCENT)).bold(),
        Span::from("  "),
        Span::from(path.clone()).bold(),
        Span::from("  "),
        status,
    ]);
    frame.render_widget(Paragraph::new(header), area);

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

/// Metric cards row: total real · entries · errors · hardlinks, one
/// rounded-border card each with its own accent color. The errors card is
/// clickable (toggles sort-by-errors); its screen rect is returned for
/// that hit-test.
fn draw_metric_cards(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &ViewSnapshot,
    ctx: &RenderCtx,
) -> Option<(u16, u16, u16, u16)> {
    let theme = &ctx.theme;
    let stats = &snapshot.stats;
    let error_entry = if stats.errors > 0 {
        theme::ERROR
    } else {
        theme::MUTED
    };
    let cards: [(&str, String, theme::Slot); 4] = [
        (
            "total",
            HumanSize(stats.disk_bytes).to_string(),
            theme::ACCENT,
        ),
        ("entries", stats.entries.to_string(), theme::INFO),
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
/// concurrent writes can transiently overshoot).
fn draw_disk_gauge(frame: &mut Frame<'_>, area: Rect, snapshot: &ViewSnapshot, ctx: &RenderCtx) {
    let theme = &ctx.theme;
    let Some(disk) = ctx.disk else {
        frame.render_widget(
            Paragraph::new(Line::from(
                Span::from(" disk stats unavailable").fg(theme.color(theme::MUTED)),
            )),
            area,
        );
        return;
    };
    let used = disk.used_fraction();
    let coverage = disk.coverage_fraction(snapshot.stats.disk_bytes);
    let text = format!(
        " {} · {:.0}% used · this scan covers {:.0}% of used ",
        HumanSize(disk.capacity),
        used * 100.0,
        coverage * 100.0,
    );
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
}

// Same call as `draw`'s: each parameter is an independent per-frame
// input, none of them a natural subgroup.
#[allow(clippy::too_many_arguments)]
fn draw_table(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
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

    let parent_disk = snapshot.totals.disk;
    let muted = theme.color(theme::MUTED);
    let coral = theme.color(theme::ERROR);
    let rows = ui.rows_indexed().map(|(index, row)| {
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
            row.disk as f64 / parent_disk as f64
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
        let name = String::from_utf8_lossy(&row.name).into_owned();
        let name = if row.is_dir {
            Span::from(format!("{name}/")).bold()
        } else {
            Span::from(name)
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
            Cell::from(format!("{:>9}", HumanSize(row.disk).to_string())),
        ];
        if ui.show_apparent {
            cells.push(Cell::from(format!(
                "{:>9}",
                HumanSize(row.apparent).to_string()
            )));
        }
        cells.extend([
            Cell::from(pct),
            Cell::from(bar),
            Cell::from(format!("{:>8}", row.items)),
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

/// The donut camembert: the viewed directory's children as slices, in the
/// current sort order, colored with the same identity colors as the
/// table. Small/unranked slices merge into a gray rest slice; under the
/// wheel: the viewed path (abbreviated) and its total.
fn draw_wheel(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    ranks: &[Option<usize>],
    motion: &mut anim::Motion,
    ctx: &RenderCtx,
) -> Option<WheelGeometry> {
    let theme = &ctx.theme;
    let snapshot = ui.snapshot();
    let block = card_block(ctx).border_style(Style::new().fg(theme.color(theme::MUTED)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height < 4 {
        return None;
    }
    // Reserve the bottom two lines for path + total.
    let [donut_area, caption_area] =
        Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).areas(inner);

    // Same rows, same order, same identity ranks as the table — and, since
    // `slice_rows` is built in cursor/display order, its position doubles
    // as the cursor position a click on that slice should land on.
    let slice_rows: Vec<(u64, Option<usize>)> = ui
        .rows_indexed()
        .map(|(index, row)| (row.disk, ranks.get(index).copied().flatten()))
        .collect();
    let (target_fracs, slice_ranks) = wheel::build_slices(&slice_rows, snapshot.totals.disk);
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

        let targets = wheel::build_slice_targets(&slice_rows, snapshot.totals.disk);
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
                &snapshot.path.display().to_string(),
                inner.width.saturating_sub(2) as usize,
            ))
            .fg(theme.color(theme::MUTED)),
        )
        .alignment(Alignment::Center),
        Line::from(Span::from(HumanSize(snapshot.totals.disk).to_string()).bold())
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
    ui: &UiState,
    ranks: &[Option<usize>],
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
    let snapshot = ui.snapshot();
    let slice_rows: Vec<(u64, Option<usize>)> = ui
        .rows_indexed()
        .map(|(index, row)| (row.disk, ranks.get(index).copied().flatten()))
        .collect();
    let (target_fracs, slice_ranks) = wheel::build_slices(&slice_rows, snapshot.totals.disk);
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
    let footer = Paragraph::new(vec![
        Line::from(
            " ↑↓/jk move · ⏎/l/→ open · ⌫/h/← up · g/G ends · d/a/n/m/c/e sort · p apparent · \
             Space mark · u unmark · v review · D delete · ? help · q quit"
                .dim(),
        ),
        Line::from(notes),
    ])
    .alignment(Alignment::Left);
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
    use camembert_core::view::{DirTotals, Row, ScanStats};
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
        }
    }

    /// A disabled `Motion` for tests that do not care about animation:
    /// bars/donut always render at their exact target value, matching
    /// what every pre-slice-5 assertion here already expected.
    fn no_motion() -> anim::Motion {
        anim::Motion::new(false)
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
                            &mut table_state,
                            '⠋',
                            Some("note"),
                            &[],
                            &mut motion,
                            &ctx,
                        );
                    })
                    .unwrap();
            }
        }
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
                        &mut table_state,
                        '⠋',
                        None,
                        &[],
                        &mut motion,
                        &ctx,
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
                    &mut table_state,
                    '⠋',
                    None,
                    &[],
                    &mut motion,
                    &ctx,
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
                    &mut table_state,
                    '⠋',
                    None,
                    &[],
                    &mut motion,
                    &ctx,
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
                    &mut table_state,
                    '|',
                    None,
                    &[],
                    &mut motion,
                    &ctx,
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
                    &mut table_state,
                    '⠋',
                    flash,
                    toasts,
                    &mut motion,
                    &ctx,
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
        confirming.open_confirm(0);
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
                        &mut table_state,
                        '⠋',
                        None,
                        &[],
                        &mut motion,
                        &ctx,
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
                        &mut table_state,
                        '⠋',
                        None,
                        &[],
                        &mut motion,
                        &ctx,
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
            cross_filesystems: false,
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
        (ui, Phase::Done(Box::new(outcome)))
    }

    fn press(
        code: KeyCode,
        ui: &mut UiState,
        phase: &mut Phase,
        generation: &mut u64,
        flash: &mut Flash,
        toasts: &mut ToastQueue,
    ) -> Action {
        handle_key(
            code,
            KeyModifiers::NONE,
            ui,
            phase,
            generation,
            flash,
            toasts,
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
        open_delete_confirm(&mut ui, &phase, &mut flash);
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
}
