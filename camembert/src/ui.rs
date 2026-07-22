//! Interactive TUI: browse the tree **while the scan runs** (D5).
//!
//! The render loop is wait-free: every frame loads the current
//! [`ViewSnapshot`] from the [`ViewBus`] (arc-swap) and re-sorts only when
//! the generation or the sort changed; it never blocks on the scan.
//! Navigation goes the other way through the capacity-1 latest-wins nav
//! cell. Once the scan finishes the owner thread exits and this module
//! serves navigation itself from the frozen arena
//! ([`camembert_core::view::build_snapshot`] on the [`ScanOutcome`]).
//!
//! Diagnostics: `tracing` only — stdout/stderr belong to the terminal UI
//! while it runs (redirect stderr to a file to capture logs).

mod state;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row as TableRow, Table, TableState};
use ratatui::{DefaultTerminal, Frame};
use tracing::{debug, error, info, warn};

use camembert_core::dump::{self, DumpMeta};
use camembert_core::scan::{LiveScan, ScanOutcome, Scanner};
use camembert_core::size::HumanSize;
use camembert_core::tree::DirId;
use camembert_core::view::{self, RowState};

use state::{SortKey, UiState, show_hardlink_note, show_updating_note};

/// Frame budget: poll timeout of the render loop (~30 fps, D5).
const FRAME: Duration = Duration::from_millis(33);

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
pub fn run(scanner: Scanner, path: &Path, output: Option<PathBuf>) -> io::Result<()> {
    let live = scanner.scan_live(path);
    // ratatui::init enters the alternate screen, enables raw mode, and
    // installs a panic hook that restores the terminal first.
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, live, output);
    ratatui::restore();
    result
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

    loop {
        // 1. Input (drain everything pending; block at most one frame).
        let mut deadline = FRAME;
        while event::poll(deadline)? {
            deadline = Duration::ZERO;
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                match handle_key(key.code, key.modifiers, &mut ui, &phase) {
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
        let spinner = SPINNER[(started.elapsed().as_millis() / 80) as usize % SPINNER.len()];
        terminal.draw(|frame| draw(frame, &ui, &mut table_state, spinner))?;
    }
}

enum Action {
    Continue,
    Quit,
}

fn handle_key(code: KeyCode, modifiers: KeyModifiers, ui: &mut UiState, phase: &Phase) -> Action {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => ui.move_down(),
        KeyCode::Up | KeyCode::Char('k') => ui.move_up(),
        KeyCode::Char('g') => ui.move_top(),
        KeyCode::Char('G') => ui.move_bottom(),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            if let Some(dir) = ui.descend() {
                request_nav(phase, dir);
            }
        }
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
            if let Some(dir) = ui.ascend() {
                request_nav(phase, dir);
            }
        }
        KeyCode::Char('d') => ui.press_sort(SortKey::Disk),
        KeyCode::Char('a') => ui.press_sort(SortKey::Apparent),
        KeyCode::Char('n') => ui.press_sort(SortKey::Name),
        KeyCode::Char('m') => ui.press_sort(SortKey::Mtime),
        KeyCode::Char('c') => ui.press_sort(SortKey::Items),
        KeyCode::Char('e') => ui.press_sort(SortKey::Errors),
        KeyCode::Char('p') => ui.show_apparent = !ui.show_apparent,
        _ => {}
    }
    Action::Continue
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
        outcome.hardlink_inodes > 0,
        false,
    );
    ui.apply_snapshot(Arc::new(snapshot));
}

fn draw(frame: &mut Frame<'_>, ui: &UiState, table_state: &mut TableState, spinner: char) {
    let snapshot = ui.snapshot();
    let [header_area, table_area, footer_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    // Header: viewed path + live whole-scan stats.
    let stats = &snapshot.stats;
    let status: Span<'_> = if stats.root_complete {
        "done".green()
    } else {
        format!("{spinner} scanning").yellow()
    };
    let header = Paragraph::new(vec![
        Line::from(vec![
            " camembert ".bold(),
            snapshot.path.display().to_string().cyan().bold(),
        ]),
        Line::from(vec![
            Span::raw(format!(
                " {} entries · {} dirs · {} errors · {} · {:.1}s · ",
                stats.entries,
                stats.dirs,
                stats.errors,
                HumanSize(stats.disk_bytes),
                stats.elapsed.as_secs_f64(),
            )),
            status,
        ]),
    ]);
    frame.render_widget(header, header_area);

    // Table.
    let sort = ui.sort();
    let arrow = |key: SortKey| -> &'static str {
        if sort.key != key {
            ""
        } else if sort.descending {
            "▼"
        } else {
            "▲"
        }
    };
    let mut header_cells = vec![
        Cell::from(" "),
        Cell::from(format!("real{}", arrow(SortKey::Disk))),
    ];
    let mut widths = vec![Constraint::Length(1), Constraint::Length(10)];
    if ui.show_apparent {
        header_cells.push(Cell::from(format!("apparent{}", arrow(SortKey::Apparent))));
        widths.push(Constraint::Length(10));
    }
    header_cells.extend([
        Cell::from("%"),
        Cell::from(format!("items{}", arrow(SortKey::Items))),
        Cell::from(format!("name{}", arrow(SortKey::Name))),
    ]);
    widths.extend([
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Min(10),
    ]);

    let parent_disk = snapshot.totals.disk;
    let rows = ui.rows().map(|row| {
        let marker = match row.state {
            RowState::Scanning => Span::from(spinner.to_string()).yellow(),
            RowState::Error => "!".red().bold(),
            RowState::Complete | RowState::File if row.errors > 0 => "!".red(),
            RowState::Complete | RowState::File => Span::raw(" "),
        };
        let pct = if parent_disk > 0 {
            format!("{:5.1}", 100.0 * row.disk as f64 / parent_disk as f64)
        } else {
            format!("{:>5}", "-")
        };
        let name = String::from_utf8_lossy(&row.name).into_owned();
        let name = if row.is_dir {
            Span::from(format!("{name}/")).bold().fg(Color::Blue)
        } else {
            Span::from(name)
        };
        let mut cells = vec![
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
            Cell::from(format!("{:>8}", row.items)),
            Cell::from(name),
        ]);
        TableRow::new(cells)
    });
    let table = Table::new(rows, widths)
        .header(TableRow::new(header_cells).style(Style::new().add_modifier(Modifier::UNDERLINED)))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(table, table_area, table_state);

    // Footer: keys + status notes.
    let mut notes: Vec<Span<'_>> = Vec::new();
    if show_hardlink_note(snapshot) {
        notes.push(
            "provisional totals (hardlinks) — corrected at scan end"
                .italic()
                .yellow(),
        );
    }
    if show_updating_note(snapshot) {
        if !notes.is_empty() {
            notes.push(Span::raw(" · "));
        }
        notes.push("updating…".italic().dim());
    }
    let footer = Paragraph::new(vec![
        Line::from(
            " ↑↓/jk move · ⏎/l/→ open · ⌫/h/← up · g/G ends · d/a/n/m/c/e sort · p apparent · q quit"
                .dim(),
        ),
        Line::from(notes),
    ])
    .alignment(Alignment::Left);
    frame.render_widget(footer, footer_area);
}
