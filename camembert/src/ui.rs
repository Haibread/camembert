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

pub mod caps;
mod fmt;
mod state;
mod theme;
mod wheel;

use std::io;
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
    ConfirmState, FrameGeometry, MarkRefusal, SortKey, TableGeometry, UiState, WheelGeometry,
    show_hardlink_note, show_updating_note,
};
use theme::Theme;

/// Frame budget: poll timeout of the render loop (~30 fps, D5).
const FRAME: Duration = Duration::from_millis(33);

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

/// Below this terminal width the wheel panel is hidden entirely and the
/// table takes the full width (the mini-donut collapse is slice 5).
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
/// scan completes — never on a mid-scan cancel.
pub fn run(scanner: Scanner, path: &Path, output: Option<PathBuf>, caps: Caps) -> io::Result<()> {
    info!(?caps, "terminal capabilities detected");
    let disk = disk_space(path);
    let live = scanner.scan_live(path);
    // ratatui::init enters the alternate screen, enables raw mode, and
    // installs a panic hook that restores the terminal first.
    let mut terminal = ratatui::init();
    enable_mouse_capture();
    let ctx = RenderCtx {
        caps,
        theme: Theme::new(caps.color),
        disk,
    };
    let result = event_loop(&mut terminal, live, output, &ctx);
    disable_mouse_capture();
    ratatui::restore();
    result
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

/// Finalize hardlink attribution and, when requested, write the dump.
/// Dump failures are logged, not fatal — the browsing session goes on.
fn finish_scan(outcome: &mut ScanOutcome, output: Option<PathBuf>) {
    outcome.finalize_hardlinks();
    let Some(path) = output else { return };
    if outcome.cancelled {
        warn!(path = %path.display(), "scan cancelled mid-run: dump not written");
        return;
    }
    let meta = DumpMeta {
        timestamp: SystemTime::now(),
    };
    match dump::write_dump_to_path(outcome, &path, &meta) {
        Ok(()) => info!(path = %path.display(), "dump written"),
        Err(err) => error!(%err, path = %path.display(), "dump write failed"),
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
    // Last left-click's time and cell, for double-click detection —
    // independent of the click-already-selected-row shortcut.
    let mut last_click: Option<(Instant, u16, u16)> = None;

    loop {
        // 1. Input (drain everything pending; block at most one frame).
        let mut deadline = FRAME;
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
            finish_scan(&mut outcome, output.take());
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
        let mut geometry = FrameGeometry::default();
        terminal.draw(|frame| {
            geometry = draw(
                frame,
                &ui,
                &mut table_state,
                spinner,
                flash_text.as_deref(),
                ctx,
            );
        })?;
        // Recomputed every frame (design slice 3): mouse events hit-test
        // against exactly what is on screen right now.
        ui.set_geometry(geometry);
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

fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    ui: &mut UiState,
    phase: &mut Phase,
    generation: &mut u64,
    flash: &mut Flash,
) -> Action {
    // The confirmation modal captures every key: `y` confirms, anything
    // else cancels (Ctrl-C keeps quitting — safety hatch).
    if ui.confirm().is_some() {
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if code == KeyCode::Char('y') {
            execute_deletion(ui, phase, generation, flash);
        } else {
            ui.cancel_confirm();
        }
        return Action::Continue;
    }
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => ui.move_down(),
        KeyCode::Up | KeyCode::Char('k') => ui.move_up(),
        KeyCode::Char('g') => ui.move_top(),
        KeyCode::Char('G') => ui.move_bottom(),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => try_descend(ui, phase),
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => try_ascend(ui, phase),
        KeyCode::Char('d') => ui.press_sort(SortKey::Disk),
        KeyCode::Char('a') => ui.press_sort(SortKey::Apparent),
        KeyCode::Char('n') => ui.press_sort(SortKey::Name),
        KeyCode::Char('m') => ui.press_sort(SortKey::Mtime),
        KeyCode::Char('c') => ui.press_sort(SortKey::Items),
        KeyCode::Char('e') => ui.press_sort(SortKey::Errors),
        KeyCode::Char('p') => ui.show_apparent = !ui.show_apparent,
        KeyCode::Char(' ') => try_toggle_mark(ui, phase, flash),
        KeyCode::Char('u') => ui.unmark_all(),
        KeyCode::Char('D') => open_delete_confirm(ui, phase, flash),
        _ => {}
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
/// support, design slice 3). Inert while the delete-confirmation modal is
/// open — it only listens to the keyboard.
fn handle_mouse(
    mouse: MouseEvent,
    ui: &mut UiState,
    phase: &Phase,
    last_click: &mut Option<(Instant, u16, u16)>,
) {
    if ui.confirm().is_some() {
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
/// re-view from the nearest surviving directory.
fn execute_deletion(ui: &mut UiState, phase: &mut Phase, generation: &mut u64, flash: &mut Flash) {
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
        flash.set(format!(
            "deleted {} ({} freed), failed {}, skipped {} — see log",
            report.deleted,
            HumanSize(report.freed.real),
            report.failed,
            report.skipped
        ));
    } else {
        flash.set(format!(
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
fn draw(
    frame: &mut Frame<'_>,
    ui: &UiState,
    table_state: &mut TableState,
    spinner: char,
    flash: Option<&str>,
    ctx: &RenderCtx,
) -> FrameGeometry {
    let snapshot = ui.snapshot();
    let [header_area, cards_area, gauge_area, main_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    // Identity colors: assigned once per frame from the snapshot rows (in
    // snapshot order), shared by the table bars/names and the wheel so
    // they can never disagree.
    let disks: Vec<u64> = snapshot.rows.iter().map(|row| row.disk).collect();
    let ranks = theme::assign_identity(&disks, theme::IDENTITY.len());

    let breadcrumb = draw_header(frame, header_area, ui, spinner, ctx);
    let errors_card = draw_metric_cards(frame, cards_area, snapshot, ctx);
    draw_disk_gauge(frame, gauge_area, snapshot, ctx);

    // Main split: table (with selection card) left, wheel right — the
    // wheel hides entirely below MIN_WHEEL_TERMINAL_WIDTH or on ASCII.
    let show_wheel = !ctx.ascii() && frame.area().width >= MIN_WHEEL_TERMINAL_WIDTH;
    let (left_area, wheel_area) = if show_wheel {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main_area);
        (left, Some(right))
    } else {
        (main_area, None)
    };
    let show_selection_card = left_area.height >= 9;
    let (table_area, card_area) = if show_selection_card {
        let [table, card] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(left_area);
        (table, Some(card))
    } else {
        (left_area, None)
    };

    let table = draw_table(frame, table_area, ui, table_state, spinner, &ranks, ctx);
    if let Some(card_area) = card_area {
        draw_selection_card(frame, card_area, ui, ctx);
    }
    let wheel = wheel_area.and_then(|wheel_area| draw_wheel(frame, wheel_area, ui, &ranks, ctx));

    draw_footer(frame, footer_area, ui, flash, ctx);

    if let Some(confirm) = ui.confirm() {
        draw_confirm_modal(frame, ui, confirm, ctx);
    }

    FrameGeometry {
        table: Some(table),
        breadcrumb_row: header_area.y,
        breadcrumb,
        errors_card,
        wheel,
    }
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
    let cards: [(&str, String, theme::PaletteEntry); 4] = [
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

fn draw_table(
    frame: &mut Frame<'_>,
    area: Rect,
    ui: &UiState,
    table_state: &mut TableState,
    spinner: char,
    ranks: &[Option<usize>],
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
        let bar = Span::from(wheel::proportion_bar(frac, BAR_WIDTH, ctx.ascii()))
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
    let (fracs, slice_ranks) = wheel::build_slices(&slice_rows, snapshot.totals.disk);
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
    if let Some((count, disk)) = ui.marked_summary() {
        push_note(
            &mut notes,
            Span::from(format!("marked: {count} entries, {}", HumanSize(disk)))
                .fg(theme.color(theme::ERROR)),
        );
    }
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
            " ↑↓/jk move · ⏎/l/→ open · ⌫/h/← up · g/G ends · d/a/n/m/c/e sort · p apparent · Space mark · u unmark · D delete · q quit"
                .dim(),
        ),
        Line::from(notes),
    ])
    .alignment(Alignment::Left);
    frame.render_widget(footer, area);
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

    fn ctx(glyphs: GlyphLevel, color: ColorLevel) -> RenderCtx {
        RenderCtx {
            caps: Caps { color, glyphs },
            theme: Theme::new(color),
            disk: Some(DiskSpace {
                capacity: 100_000,
                used: 40_000,
            }),
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
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        draw(frame, &ui, &mut table_state, '⠋', Some("note"), &ctx);
                    })
                    .unwrap();
            }
        }
    }

    /// Wide terminal: the wheel panel renders half-block pixels; narrow
    /// terminal: it disappears and the table takes the full width.
    #[test]
    fn wheel_appears_only_on_wide_terminals() {
        let render = |width: u16| -> String {
            let ctx = ctx(GlyphLevel::HalfBlock, ColorLevel::Truecolor);
            let ui = UiState::new(sample_snapshot());
            let mut table_state = TableState::default();
            let mut terminal = Terminal::new(TestBackend::new(width, 35)).unwrap();
            terminal
                .draw(|frame| {
                    draw(frame, &ui, &mut table_state, '⠋', None, &ctx);
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            buffer
                .content()
                .iter()
                .map(|c| c.symbol().to_owned())
                .collect()
        };
        assert!(render(120).contains('▀'), "wheel pixels on a wide terminal");
        assert!(!render(80).contains('▀'), "no wheel below 100 columns");
    }

    /// ASCII rung: no wheel, `#` bars, plain borders — and still no
    /// non-ASCII glyph anywhere outside the footer's fixed key hints.
    #[test]
    fn ascii_rung_renders_hash_bars() {
        let ctx = ctx(GlyphLevel::Ascii, ColorLevel::Mono);
        let ui = UiState::new(sample_snapshot());
        let mut table_state = TableState::default();
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &ui, &mut table_state, '|', None, &ctx);
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
}
