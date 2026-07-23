//! Pure interactive-mode state: sorting, cursor, navigation stack,
//! snapshot application, and the mark-then-confirm deletion state
//! (HANDOFF §5). No terminal types anywhere — everything here is
//! unit-testable with synthetic snapshots.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use camembert_core::flat::FlatSummary;
use camembert_core::freeable::Ledger;
use camembert_core::query::FilterResult;
use camembert_core::tree::{DirId, NodeId};
use camembert_core::view::{Row, ViewSnapshot};

use super::freeable_panel::{FreeableGroup, OpenWarning};
use super::palette::PaletteState;

/// Which table mode is active (D3, `docs/design/flat-view-decisions.md`):
/// the tree browser, or one of the two flat-view modes toggled by `t`/`b`.
/// Cards, gauge, basket and footer stay in place across all three — only
/// the table (and the donut's data source) change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Tree,
    /// `t`: flat top files across the whole scan.
    FlatTop,
    /// `b`: pattern breakdown (disjoint groups + uncategorized rest, D1).
    Breakdown,
}

/// Column a view is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Real (disk) size — the default.
    Disk,
    Apparent,
    Name,
    Mtime,
    /// Subtree item count.
    Items,
    /// Subtree error count — jump straight to what could not be read.
    Errors,
}

impl SortKey {
    /// Direction a key starts in when first selected: sizes, mtime and
    /// counts read best largest/newest first; names read best A→Z.
    fn default_descending(self) -> bool {
        !matches!(self, Self::Name)
    }
}

/// Active sort: key + direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Disk,
            descending: true,
        }
    }
}

impl SortSpec {
    /// Apply a sort keypress: a new key selects it with its default
    /// direction, the active key toggles direction.
    pub fn press(&mut self, key: SortKey) {
        if self.key == key {
            self.descending = !self.descending;
        } else {
            *self = Self {
                key,
                descending: key.default_descending(),
            };
        }
    }
}

/// One row marked for deletion (the mark-then-confirm flow, HANDOFF §5).
///
/// Captured from the row at mark time — legal because marking is only
/// possible once the scan completed, when the arena (and therefore every
/// row's node id, path, and aggregates) is frozen. A marked directory is
/// the unit: its subtree is implied, there are no per-descendant marks.
#[derive(Debug, Clone)]
pub struct MarkedEntry {
    /// The row's node in the frozen arena.
    pub node: NodeId,
    /// Full path (viewed dir's path + the row's name) for display; the
    /// deletion executor rebuilds its own path from the tree.
    pub path: PathBuf,
    pub is_dir: bool,
    /// Disk bytes (subtree total for a dir) at mark time.
    pub disk: u64,
}

/// Why a mark keypress was refused (shown as a footer flash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkRefusal {
    /// Marks are inactive until the scan completes: deletion mutates the
    /// aggregates, and during the scan the arena has another writer.
    ScanRunning,
    /// Excluded mount-point row: its subtree was never scanned, deleting
    /// it would act blind. (Rows in state Error stay markable — deleting
    /// an unreadable directory is a legitimate cleanup.)
    MountPoint,
    /// D4: a directory row under an active filter shows only its matching
    /// descendants — marking it would delete everything in it, matched or
    /// not (the "42 MB shown / 300 GB deleted" trap query-attack-a warns
    /// about). File marks are unaffected; clear the filter first to mark
    /// a directory.
    FilterActive,
}

/// The currently applied filter (D2/D4/D6, `docs/design/query-decisions.md`):
/// the query text as typed (the pill's label, and what re-opening the
/// palette pre-fills) alongside the last accepted [`FilterResult`].
/// `ui.rs` only ever calls [`UiState::set_active_filter`] after checking
/// the (query fingerprint, deletion epoch) guard itself (D5) — a stale
/// background fold never reaches this far, so whatever is stored here is
/// always the freshest applied result.
#[derive(Debug, Clone)]
pub struct ActiveFilter {
    pub query_text: String,
    pub result: Arc<FilterResult>,
}

/// State of the delete-confirmation modal, opened by
/// [`UiState::open_confirm`]. While it exists, every key belongs to the
/// modal: `y` confirms, anything else cancels.
#[derive(Debug, Clone)]
pub struct ConfirmState {
    /// Hardlinked files among the marked selection (incl. inside marked
    /// dirs); when > 0 the dialog warns that freeing depends on deleting
    /// all links.
    pub hardlink_files: u64,
    /// D6 pre-deletion advisory: which marked entries are currently open,
    /// by which processes. `None` when nothing marked is open, or the
    /// check was skipped outright (`--no-proc-sweep`/`NO_PROC_SWEEP`, D7)
    /// — either way, no warning line at all. Never blocks confirmation.
    pub open_warning: Option<OpenWarning>,
}

/// State of the `v` review-list modal, opened by [`UiState::open_review`]:
/// a floating, scrollable list of every marked entry (design slice 4).
/// Modal precedence is confirm > review > cheatsheet — `ui.rs` only opens
/// this when the confirm modal is not already up, and closes it before
/// opening confirm from within the list (`D`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewState {
    /// Position in [`UiState::marks`] (mark order) under the cursor.
    pub cursor: usize,
}

/// Screen geometry of the most recently drawn frame, for mouse
/// hit-testing. Plain coordinates only (no ratatui types) so this stays
/// as terminal-free and unit-testable as the rest of this module; `ui.rs`
/// fills it in from the actual layout on every draw and consults it when
/// a mouse event arrives. Absent fields (`None`/empty) mean that element
/// was not drawn this frame (e.g. the wheel below the width threshold).
#[derive(Debug, Clone, Default)]
pub struct FrameGeometry {
    pub table: Option<TableGeometry>,
    /// Screen row the breadcrumb path is drawn on.
    pub breadcrumb_row: u16,
    /// Half-open column range of each path-component span, paired with
    /// the ancestor directory clicking it jumps to (`None` = the current
    /// directory's own trailing segment, or a root-prefix segment before
    /// the first descend — nothing to jump to yet).
    pub breadcrumb: Vec<(u16, u16, Option<DirId>)>,
    /// The errors metric card's screen rect `(x, y, width, height)`.
    pub errors_card: Option<(u16, u16, u16, u16)>,
    pub wheel: Option<WheelGeometry>,
    /// The disk gauge's screen rect `(x, y, width, height)`, when the
    /// freeable suffix is showing (D5: clickable, opens the `f` panel).
    /// `None` when there is nothing freeable to click through to.
    pub gauge_freeable: Option<(u16, u16, u16, u16)>,
    /// Total content rows the freeable panel drew this frame, when it was
    /// open — fed back into [`UiState::clamp_freeable_cursor`] right after
    /// the frame is drawn (same feedback idiom as `set_geometry` itself),
    /// so the scroll cursor can never run away past what was actually
    /// rendered. `None` while the panel is closed.
    pub freeable_rows: Option<usize>,
}

/// Table body geometry: which screen rows hold which display-order rows.
#[derive(Debug, Clone, Copy)]
pub struct TableGeometry {
    pub x: u16,
    /// Screen row of the first body row (below the header row).
    pub y: u16,
    pub width: u16,
    /// Number of body rows visible (may be fewer than the table's row
    /// count if it doesn't fit).
    pub height: u16,
    /// Display-order position of the first visible row (`TableState`'s
    /// scroll offset).
    pub offset: usize,
}

impl TableGeometry {
    /// The display-order position under `(col, row)`, if it falls inside
    /// the table body — a cursor position, not yet bounds-checked against
    /// the current row count (the caller re-validates before using it, in
    /// case rows changed since the frame that produced this geometry).
    pub fn hit_test(&self, col: u16, row: u16) -> Option<usize> {
        if col < self.x || col >= self.x + self.width {
            return None;
        }
        if row < self.y || row >= self.y + self.height {
            return None;
        }
        Some(self.offset + (row - self.y) as usize)
    }
}

/// Donut geometry: which screen cell holds which slice, and which display
/// row each slice represents.
#[derive(Debug, Clone)]
pub struct WheelGeometry {
    pub x: u16,
    pub y: u16,
    pub width: usize,
    pub height: usize,
    /// Slice index per screen cell, row-major (`width` per row); `None`
    /// outside the disc (the hole, or the wheel's card padding).
    pub cells: Vec<Option<u16>>,
    /// Slice index -> display-order row position; `None` for the merged
    /// "rest" slice (see [`super::wheel::build_slice_targets`]), which
    /// does not correspond to a single row and is not navigable.
    pub targets: Vec<Option<usize>>,
}

impl WheelGeometry {
    /// The display-order row position a click at `(col, row)` should
    /// navigate to, if the cell is lit and its slice maps to one.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<usize> {
        if col < self.x || row < self.y {
            return None;
        }
        let (x, y) = ((col - self.x) as usize, (row - self.y) as usize);
        if x >= self.width || y >= self.height {
            return None;
        }
        let slice = self.cells.get(y * self.width + x).copied().flatten()?;
        self.targets.get(slice as usize).copied().flatten()
    }
}

impl FrameGeometry {
    /// The ancestor directory a click at `(col, row)` on the breadcrumb
    /// should jump to, if any.
    pub fn breadcrumb_hit(&self, col: u16, row: u16) -> Option<DirId> {
        if row != self.breadcrumb_row {
            return None;
        }
        self.breadcrumb
            .iter()
            .find(|&&(start, end, _)| col >= start && col < end)
            .and_then(|&(_, _, dir)| dir)
    }

    /// Whether `(col, row)` falls inside the errors metric card.
    pub fn errors_card_hit(&self, col: u16, row: u16) -> bool {
        matches!(
            self.errors_card,
            Some((x, y, w, h)) if col >= x && col < x + w && row >= y && row < y + h
        )
    }

    /// Whether `(col, row)` falls inside the disk gauge's freeable suffix
    /// (D5: clicking it opens the `f` panel, same rect as the whole gauge
    /// line for a generous, simple hit target).
    pub fn gauge_freeable_hit(&self, col: u16, row: u16) -> bool {
        matches!(
            self.gauge_freeable,
            Some((x, y, w, h)) if col >= x && col < x + w && row >= y && row < y + h
        )
    }
}

/// Cache key for the sorted permutation: re-sort only when any part
/// changes (new generation, different dir, different sort, or the active
/// filter's identity — D4 composition: the tree table's row *set*, not
/// just its totals, changes under a filter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrderKey {
    generation: u64,
    dir: DirId,
    sort: SortSpec,
    /// `(query fingerprint, deletion epoch)` of the applied
    /// [`ActiveFilter::result`], `None` with no filter active.
    filter: Option<(u64, u64)>,
}

/// The interactive session state around the current snapshot.
pub struct UiState {
    snapshot: Arc<ViewSnapshot>,
    sort: SortSpec,
    /// Cursor position in *sorted* order.
    cursor: usize,
    /// Cached permutation of `snapshot.rows` under `sort`.
    order: Vec<usize>,
    order_key: Option<OrderKey>,
    /// Directory requested but whose snapshot has not arrived yet: the
    /// current rows keep rendering (optimistic navigation).
    pending_nav: Option<DirId>,
    /// Cursor to adopt when the pending snapshot arrives.
    pending_cursor: usize,
    /// Node to put the cursor on once the pending nav resolves, when it
    /// isn't just `pending_cursor`'s position 0 — set by
    /// [`Self::jump_to_directory`] (Enter on a flat row, D3: "jumps to its
    /// containing directory... cursor on the file"). Cleared the instant
    /// it is consumed or superseded by an unrelated snapshot arriving
    /// first (see [`Self::apply_snapshot`]) so it can never misapply to
    /// the wrong directory's rows.
    pending_focus_node: Option<NodeId>,
    /// Directories descended through: `(dir, cursor within it)`, so going
    /// back up restores the cursor.
    stack: Vec<(DirId, usize)>,
    pub show_apparent: bool,
    /// Rows marked for deletion, in mark order (drives the confirm
    /// dialog's path list). Marks persist across navigation.
    marks: Vec<MarkedEntry>,
    /// Same nodes, for O(1) `is_marked` at render time.
    marked_set: HashSet<NodeId>,
    /// `Some` while the delete-confirmation modal is open.
    confirm: Option<ConfirmState>,
    /// `Some` while the `v` review-list modal is open.
    review: Option<ReviewState>,
    /// Whether the `?` cheatsheet overlay is open. No extra data, unlike
    /// the other two modals, so a plain flag is enough.
    cheatsheet_open: bool,
    /// Display-order position of the row under the mouse cursor, while it
    /// sits over the table without clicking — the selection card prefers
    /// this over the keyboard cursor when present. Cleared by any
    /// keyboard action or click, and whenever the row set changes, so it
    /// never survives past the frame it was observed in.
    hover: Option<usize>,
    /// Hit-testing geometry of the last drawn frame (mouse support,
    /// design slice 3).
    geometry: FrameGeometry,
    /// `z` zen mode (design slice 5): table-only view — no metric cards,
    /// no disk gauge, no donut wheel (full or mini). Header, table,
    /// footer and the basket strip stay.
    zen: bool,
    /// Bumped every time the *displayed* rows change identity or order
    /// for a reason other than live scan progress: a navigation that
    /// actually changed directory, or a sort keypress. `ui::anim::Motion`
    /// compares this across frames to know when to start a fresh bar/
    /// donut animation — a scan's continuous live updates never bump it,
    /// so the morph never fights the live growth (design slice 5).
    view_change_seq: u64,
    /// The freeable ledger (D1/D4/D8): a scan-level side artifact, never
    /// tree nodes or view-snapshot data. `None` until the post-scan sweep
    /// lands (or forever, under `--no-proc-sweep`/`NO_PROC_SWEEP` or a
    /// dump-loaded session, D7).
    freeable: Option<Ledger>,
    /// Root-fs entries grouped display-only under their deepest
    /// still-existing ancestor (D5) — built lazily the first time the `f`
    /// panel opens (needs the frozen tree's live directory paths) and
    /// cached here; invalidated whenever a new ledger lands.
    freeable_groups: Vec<FreeableGroup>,
    /// Whether `freeable_groups` reflects the current `freeable` ledger —
    /// distinct from "groups is empty", which can legitimately happen when
    /// there are zero root-fs entries.
    freeable_groups_built: bool,
    /// Whether the `f` freeable panel is open. Modal precedence (D5) is
    /// confirm > review > freeable panel > cheatsheet.
    freeable_open: bool,
    /// Scroll position in the freeable panel's flat content-row list.
    /// Clamped every frame from [`FrameGeometry::freeable_rows`] (see
    /// [`Self::clamp_freeable_cursor`]) rather than at each keypress, so it
    /// never needs to know the panel's rendered row count itself — the
    /// same feedback idiom `set_geometry` already uses for mouse
    /// hit-testing.
    freeable_cursor: usize,
    /// Active table mode (D3): tree, flat top files, or pattern breakdown.
    mode: ViewMode,
    /// Current flat-view summary: the live provisional accumulator
    /// snapshot while scanning, the authoritative post-scan fold once
    /// done — `ui.rs` decides which and calls [`Self::set_flat_summary`]
    /// (during a scan: every frame, mirroring the tree snapshot; post-scan:
    /// only when [`Self::flat_epoch`] disagrees with the cached summary's
    /// own `epoch`, i.e. a render-time check, never "on first `t`/`b`" —
    /// see the attack finding this exists to close). `None` before the
    /// first summary of either kind has arrived.
    flat: Option<Arc<FlatSummary>>,
    /// Bumped by [`Self::bump_flat_epoch`] after every successful deletion
    /// (D2/D3): the flat/breakdown summary is recomputed whenever this
    /// disagrees with the cached summary's `epoch`, so a delete performed
    /// *from within* a flat/breakdown mode can never leave a stale,
    /// already-deleted row on screen past the very next frame. Dual
    /// -purpose since the query filter (D5, D6) landed: `ui.rs` also
    /// stamps every background filter fold request with this same value
    /// and drops the result on arrival if it has since moved — one
    /// deletion epoch serves both recompute paths, rather than inventing
    /// a second counter for what is the same underlying event ("the
    /// frozen arena just changed under a live view").
    flat_epoch: u64,
    /// `Some` while the Ctrl-K/`/` palette is open (D6); every keystroke
    /// but Esc/Enter/arrows/Ctrl-C belongs to it while it is (see
    /// `ui::handle_key`'s topmost precedence rung).
    palette: Option<PaletteState>,
    /// The currently applied filter (D2/D4/D5/D6), `None` when no filter
    /// is active. Composition (tree/t/b totals, dir-mark refusal, the
    /// pill) all key off this.
    filter: Option<ActiveFilter>,
}

impl UiState {
    pub fn new(snapshot: Arc<ViewSnapshot>) -> Self {
        let mut state = Self {
            snapshot,
            sort: SortSpec::default(),
            cursor: 0,
            order: Vec::new(),
            order_key: None,
            pending_nav: None,
            pending_cursor: 0,
            pending_focus_node: None,
            stack: Vec::new(),
            show_apparent: true,
            marks: Vec::new(),
            marked_set: HashSet::new(),
            confirm: None,
            review: None,
            cheatsheet_open: false,
            hover: None,
            geometry: FrameGeometry::default(),
            zen: false,
            view_change_seq: 0,
            freeable: None,
            freeable_groups: Vec::new(),
            freeable_groups_built: false,
            freeable_open: false,
            freeable_cursor: 0,
            mode: ViewMode::default(),
            flat: None,
            flat_epoch: 0,
            palette: None,
            filter: None,
        };
        state.ensure_sorted();
        state
    }

    pub fn snapshot(&self) -> &ViewSnapshot {
        &self.snapshot
    }

    pub fn sort(&self) -> SortSpec {
        self.sort
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn pending_nav(&self) -> Option<DirId> {
        self.pending_nav
    }

    /// Rows in the active sort order, with each row's index in
    /// *snapshot* order — the key identity-color assignment is computed
    /// under, so the table and the wheel dereference the same ranks.
    pub fn rows_indexed(&self) -> impl Iterator<Item = (usize, &Row)> {
        self.order.iter().map(|&i| (i, &self.snapshot.rows[i]))
    }

    /// The row under the cursor, if any.
    pub fn selected(&self) -> Option<&Row> {
        self.order.get(self.cursor).map(|&i| &self.snapshot.rows[i])
    }

    /// The row the selection card should describe: the mouse-hovered row
    /// while present, otherwise the keyboard cursor's row (mouse hover
    /// and keyboard selection both drive the card — design slice 3).
    pub fn card_row(&self) -> Option<&Row> {
        let position = self.hover.unwrap_or(self.cursor);
        self.order.get(position).map(|&i| &self.snapshot.rows[i])
    }

    /// Row under the mouse, set on `MouseEventKind::Moved` over the table.
    pub fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Set the hovered row (mouse moved over the table body).
    pub fn set_hover(&mut self, position: usize) {
        self.hover = Some(position);
    }

    /// Clear the hovered row: the mouse left the table, a click landed,
    /// or a keyboard action moved the cursor (the card should follow
    /// whichever input the user is actually using).
    pub fn clear_hover(&mut self) {
        self.hover = None;
    }

    /// DirIds of every directory currently open above the viewed one,
    /// root-first — mirrors the descend/ascend stack. Used to build the
    /// clickable breadcrumb: each entry is one `ascend()` away in that
    /// many steps.
    pub fn stack_dirs(&self) -> impl Iterator<Item = DirId> + '_ {
        self.stack.iter().map(|&(dir, _)| dir)
    }

    /// Hit-testing geometry captured from the last drawn frame.
    pub fn geometry(&self) -> &FrameGeometry {
        &self.geometry
    }

    /// Replace the hit-testing geometry after a frame is drawn.
    pub fn set_geometry(&mut self, geometry: FrameGeometry) {
        self.geometry = geometry;
    }

    /// Adopt a (possibly unchanged) snapshot: resolve pending navigation,
    /// re-sort when the generation/dir/sort changed, clamp the cursor.
    pub fn apply_snapshot(&mut self, snapshot: Arc<ViewSnapshot>) {
        let dir_changed = snapshot.dir != self.snapshot.dir;
        let mut focus = None;
        if self.pending_nav == Some(snapshot.dir) {
            self.pending_nav = None;
            self.cursor = self.pending_cursor;
            focus = self.pending_focus_node.take();
        } else if dir_changed {
            // A dir change we did not ask for (stale pending overwritten
            // by a later request): start at the top. Any focus request
            // belonged to a *different* pending nav than the one that
            // just resolved — discard it rather than misapplying it to
            // these rows.
            self.cursor = 0;
            self.pending_focus_node = None;
        }
        self.snapshot = snapshot;
        self.ensure_sorted();
        if let Some(node) = focus
            && let Some(position) = self
                .order
                .iter()
                .position(|&i| self.snapshot.rows[i].node == node)
        {
            self.cursor = position;
        }
        self.clamp_cursor();
        // The row set may have changed shape entirely; a stale hover
        // position would describe the wrong row until the mouse moves
        // again, so drop it (it is recomputed on the next `Moved` event).
        self.clear_hover();
        // Only an actual navigation is a "view change" for animation
        // purposes (design slice 5): a scan's continuous live updates
        // reapply the same dir over and over and must never retrigger
        // the bar/donut morph, or it would fight the live growth the
        // design explicitly says to leave alone.
        if dir_changed {
            self.view_change_seq += 1;
        }
    }

    /// Handle a sort keypress; re-sorts immediately.
    pub fn press_sort(&mut self, key: SortKey) {
        self.sort.press(key);
        self.ensure_sorted();
        self.clamp_cursor();
        self.clear_hover(); // order changed under a stale hover position
        // A sort reorders the donut slices and every bar's screen row
        // even though no value changed — worth the same reveal/morph
        // flourish as a navigation (design slice 5).
        self.view_change_seq += 1;
    }

    /// See [`Self::view_change_seq`]'s field doc.
    pub fn view_change_seq(&self) -> u64 {
        self.view_change_seq
    }

    /// `z`: toggle zen mode (table-only view, design slice 5).
    pub fn toggle_zen(&mut self) {
        self.zen = !self.zen;
    }

    pub fn zen(&self) -> bool {
        self.zen
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.row_count() {
            self.cursor += 1;
        }
        self.clear_hover();
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.clear_hover();
    }

    pub fn move_top(&mut self) {
        self.cursor = 0;
        self.clear_hover();
    }

    pub fn move_bottom(&mut self) {
        self.cursor = self.row_count().saturating_sub(1);
        self.clear_hover();
    }

    /// Number of rows in the active view: the sorted tree rows in
    /// [`ViewMode::Tree`], or the flat/breakdown summary's row count in
    /// the other two modes (top files; groups + the trailing uncategorized
    /// row) — `0` before the first summary of the relevant kind has
    /// arrived. Every cursor-bound method below goes through this so mode
    /// switches never need their own clamping logic.
    pub fn row_count(&self) -> usize {
        match self.mode {
            ViewMode::Tree => self.order.len(),
            ViewMode::FlatTop => self.flat.as_ref().map_or(0, |f| f.top_files.len()),
            ViewMode::Breakdown => self.flat.as_ref().map_or(0, |f| f.groups.len() + 1),
        }
    }

    /// Move the cursor directly to a display-order position (a mouse
    /// click) — clamped defensively in case the row count changed since
    /// the frame the click's geometry came from.
    pub fn select_at(&mut self, position: usize) {
        self.cursor = position.min(self.row_count().saturating_sub(1));
        self.clear_hover();
    }

    /// Start descending into the directory under the cursor. Returns the
    /// directory to request; the rows keep rendering until its snapshot
    /// arrives. Ignored while another navigation is pending (the arrival
    /// resolves in well under a frame in practice).
    pub fn descend(&mut self) -> Option<DirId> {
        if self.pending_nav.is_some() {
            return None;
        }
        let target = self.selected()?.dir?;
        self.stack.push((self.snapshot.dir, self.cursor));
        self.pending_nav = Some(target);
        self.pending_cursor = 0;
        Some(target)
    }

    /// Start going up to the parent. Returns the directory to request;
    /// restores the remembered cursor when the parent's snapshot arrives.
    pub fn ascend(&mut self) -> Option<DirId> {
        if self.pending_nav.is_some() {
            return None;
        }
        let parent = self.snapshot.parent?;
        self.pending_cursor = match self.stack.pop() {
            Some((dir, cursor)) if dir == parent => cursor,
            Some(other) => {
                // Not where we came from (deep-linked view): keep the
                // stack intact and land at the top.
                self.stack.push(other);
                0
            }
            None => 0,
        };
        self.pending_nav = Some(parent);
        Some(parent)
    }

    /// Jump straight to an open ancestor directory (breadcrumb click):
    /// equivalent to pressing `ascend` repeatedly until reaching `dir`,
    /// but resolved as a single navigation request instead of one per
    /// frame. `dir` must be a directory on the current descend stack (as
    /// built by [`Self::stack_dirs`]); anything else — a stale click after
    /// navigating away, the current directory itself, or another
    /// navigation already pending — is a no-op.
    pub fn jump_to_ancestor(&mut self, dir: DirId) -> Option<DirId> {
        if self.pending_nav.is_some() || dir == self.snapshot.dir {
            return None;
        }
        let position = self.stack.iter().position(|&(d, _)| d == dir)?;
        let (_, cursor) = self.stack[position];
        // Discard the descends between `dir` and here — they are behind
        // us now; keep everything above it (its own ancestors).
        self.stack.truncate(position);
        self.pending_cursor = cursor;
        self.pending_nav = Some(dir);
        Some(dir)
    }

    /// Show/hide the apparent-size column (`p`) — a plain toggle, but
    /// given a method (rather than field mutation at the call site) so it
    /// can sit in the keyboard dispatch table alongside every other
    /// stateless key (see `ui::keymap::SIMPLE`).
    pub fn toggle_apparent(&mut self) {
        self.show_apparent = !self.show_apparent;
    }

    // ---- mark-then-confirm deletion (HANDOFF §5) ----

    /// Mark/unmark the row under the cursor, then move down one (like
    /// dua). Guards at mark time:
    /// - refused while the scan runs (the arena has another writer);
    /// - refused on excluded mount-point rows;
    /// - the scan root is never a row of its own view, so it cannot be
    ///   marked by construction (there is no `..` row);
    /// - rows in state Error are allowed — deleting an unreadable
    ///   directory is legitimate.
    ///
    /// No row under the cursor (empty view) is a silent no-op.
    pub fn toggle_mark(&mut self) -> Result<(), MarkRefusal> {
        if !self.snapshot.stats.root_complete {
            return Err(MarkRefusal::ScanRunning);
        }
        let Some(row) = self.selected() else {
            return Ok(());
        };
        // Excluded mount points are the dir rows without directory
        // metadata (never descended into).
        if row.is_dir && row.dir.is_none() {
            return Err(MarkRefusal::MountPoint);
        }
        // D4: a directory under an active filter shows only its matching
        // descendants — marking it would delete everything inside it,
        // matched or not. File marks are unaffected (checked implicitly:
        // this branch only fires for `is_dir` rows).
        if row.is_dir && self.filter.is_some() {
            return Err(MarkRefusal::FilterActive);
        }
        use std::os::unix::ffi::OsStrExt;
        let entry = MarkedEntry {
            node: row.node,
            path: self
                .snapshot
                .path
                .join(std::ffi::OsStr::from_bytes(&row.name)),
            is_dir: row.is_dir,
            disk: row.disk,
        };
        if self.marked_set.remove(&entry.node) {
            self.marks.retain(|mark| mark.node != entry.node);
        } else {
            self.marked_set.insert(entry.node);
            self.marks.push(entry);
        }
        self.move_down();
        Ok(())
    }

    /// Clear every mark (the `u` key).
    pub fn unmark_all(&mut self) {
        self.marks.clear();
        self.marked_set.clear();
    }

    /// Whether a row's node is marked (render-time lookup).
    pub fn is_marked(&self, node: NodeId) -> bool {
        self.marked_set.contains(&node)
    }

    /// Marked rows in mark order.
    pub fn marks(&self) -> &[MarkedEntry] {
        &self.marks
    }

    /// Footer summary: `(entry count, total disk bytes)`, `None` when
    /// nothing is marked. A marked dir counts its subtree total; nested
    /// marks (a mark inside a marked dir) are summed twice — accepted,
    /// the confirm/executor path handles containment exactly.
    pub fn marked_summary(&self) -> Option<(usize, u64)> {
        if self.marks.is_empty() {
            return None;
        }
        Some((
            self.marks.len(),
            self.marks.iter().map(|mark| mark.disk).sum(),
        ))
    }

    /// Open the delete-confirmation modal. Refused (returns `false`) when
    /// nothing is marked or the scan still runs. `hardlink_files` is
    /// computed by the caller from the tree (see
    /// [`camembert_core::delete::hardlink_files_in`]); `open_warning` is the
    /// D6 pre-deletion advisory, computed by the caller from a fresh
    /// [`camembert_core::freeable::open_file_index`] (`None` when nothing
    /// marked is open, or `--no-proc-sweep`/`NO_PROC_SWEEP` skipped the
    /// check outright, D7).
    pub fn open_confirm(&mut self, hardlink_files: u64, open_warning: Option<OpenWarning>) -> bool {
        if self.marks.is_empty() || !self.snapshot.stats.root_complete {
            return false;
        }
        self.confirm = Some(ConfirmState {
            hardlink_files,
            open_warning,
        });
        true
    }

    /// The open confirmation modal, if any. While `Some`, every keypress
    /// belongs to the modal.
    pub fn confirm(&self) -> Option<&ConfirmState> {
        self.confirm.as_ref()
    }

    /// Close the modal without deleting (any key but `y`).
    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    /// Confirm the modal (`y`): closes it and hands the marked entries to
    /// the caller for execution, clearing every mark. `None` when the
    /// modal was not open (nothing to confirm).
    pub fn take_confirmed_marks(&mut self) -> Option<Vec<MarkedEntry>> {
        self.confirm.take()?;
        self.marked_set.clear();
        Some(std::mem::take(&mut self.marks))
    }

    // ---- `v` review list (design slice 4) ----

    /// Open the review list over the marked entries (`v`). Refused
    /// (returns `false`) when nothing is marked — the caller flashes a
    /// hint, matching `D`'s "nothing marked" behavior.
    pub fn open_review(&mut self) -> bool {
        if self.marks.is_empty() {
            return false;
        }
        self.review = Some(ReviewState::default());
        true
    }

    /// The open review modal, if any. While `Some`, keys route to the
    /// list (move, unmark, `D`, close) instead of the main view.
    pub fn review(&self) -> Option<&ReviewState> {
        self.review.as_ref()
    }

    /// Close the review list without changing any mark (`v` or `Esc`).
    pub fn close_review(&mut self) {
        self.review = None;
    }

    /// Move the review cursor down one row, clamped at the last mark.
    pub fn review_move_down(&mut self) {
        if let Some(review) = &mut self.review
            && review.cursor + 1 < self.marks.len()
        {
            review.cursor += 1;
        }
    }

    /// Move the review cursor up one row, clamped at the first mark.
    pub fn review_move_up(&mut self) {
        if let Some(review) = &mut self.review {
            review.cursor = review.cursor.saturating_sub(1);
        }
    }

    /// Unmark the entry under the review cursor (`Space` inside the
    /// list) — the only way to unmark a single entry without hunting for
    /// its row back in the table. Closes the list once the last mark is
    /// gone (an empty basket has nothing left to review); otherwise
    /// clamps the cursor onto the new last row if it pointed past it.
    pub fn unmark_at_review_cursor(&mut self) {
        let Some(review) = self.review else { return };
        let Some(entry) = self.marks.get(review.cursor) else {
            return;
        };
        let node = entry.node;
        self.marked_set.remove(&node);
        self.marks.remove(review.cursor);
        if self.marks.is_empty() {
            self.review = None;
        } else {
            let cursor = review.cursor.min(self.marks.len() - 1);
            self.review = Some(ReviewState { cursor });
        }
    }

    // ---- `?` cheatsheet (design slice 4) ----

    pub fn cheatsheet_open(&self) -> bool {
        self.cheatsheet_open
    }

    pub fn open_cheatsheet(&mut self) {
        self.cheatsheet_open = true;
    }

    pub fn close_cheatsheet(&mut self) {
        self.cheatsheet_open = false;
    }

    // ---- `f` freeable panel (freeable phase 1, D4/D5) ----

    /// Adopt a freshly-swept ledger (called once the scan-end sweep's
    /// off-thread result lands, D4). Invalidates any cached grouping — it
    /// is rebuilt lazily the next time the panel opens.
    pub fn set_freeable_ledger(&mut self, ledger: Ledger) {
        self.freeable = Some(ledger);
        self.freeable_groups = Vec::new();
        self.freeable_groups_built = false;
    }

    /// The current ledger, if the sweep has completed (`None` before it
    /// lands, under `--no-proc-sweep`/`NO_PROC_SWEEP`, or in a session with
    /// no sweep at all, D7). Drives the gauge suffix (D5) and the panel.
    pub fn freeable_ledger(&self) -> Option<&Ledger> {
        self.freeable.as_ref()
    }

    /// Whether [`Self::freeable_groups`] reflects the current ledger (set
    /// by [`Self::set_freeable_groups`], cleared on a new
    /// [`Self::set_freeable_ledger`]) — lets the caller skip rebuilding the
    /// (tree-walk-dependent) grouping on every `f` press.
    pub fn freeable_groups_built(&self) -> bool {
        self.freeable_groups_built
    }

    /// Cache the display grouping computed by the caller (D5:
    /// [`super::freeable_panel::group_by_ancestor`] against the frozen
    /// tree's live directory paths).
    pub fn set_freeable_groups(&mut self, groups: Vec<FreeableGroup>) {
        self.freeable_groups = groups;
        self.freeable_groups_built = true;
    }

    /// The cached display grouping, largest-entry group first (D5). Empty
    /// (and not yet "built") until the panel's first open.
    pub fn freeable_groups(&self) -> &[FreeableGroup] {
        &self.freeable_groups
    }

    pub fn freeable_open(&self) -> bool {
        self.freeable_open
    }

    /// Open the panel (`f` or a gauge-suffix click), resetting the scroll
    /// to the top. Always succeeds — even with no ledger yet, the panel
    /// shows an explanatory empty state rather than refusing to open.
    pub fn open_freeable_panel(&mut self) {
        self.freeable_open = true;
        self.freeable_cursor = 0;
    }

    /// Close the panel (`f` or `Esc`).
    pub fn close_freeable_panel(&mut self) {
        self.freeable_open = false;
    }

    pub fn freeable_cursor(&self) -> usize {
        self.freeable_cursor
    }

    /// Scroll down one row. Unbounded here by design: the true bound
    /// (how many rows the panel actually rendered) is only known at draw
    /// time, so [`Self::clamp_freeable_cursor`] reins this in right after
    /// every frame — well before the next keypress, so it never visibly
    /// overshoots.
    pub fn freeable_move_down(&mut self) {
        self.freeable_cursor = self.freeable_cursor.saturating_add(1);
    }

    pub fn freeable_move_up(&mut self) {
        self.freeable_cursor = self.freeable_cursor.saturating_sub(1);
    }

    /// Clamp the scroll cursor to `total_rows` (from
    /// [`FrameGeometry::freeable_rows`]) — called once per frame right
    /// after drawing, the same feedback idiom [`Self::set_geometry`] uses
    /// for mouse hit-testing.
    pub fn clamp_freeable_cursor(&mut self, total_rows: usize) {
        self.freeable_cursor = self.freeable_cursor.min(total_rows.saturating_sub(1));
    }

    fn ensure_sorted(&mut self) {
        let filter_id = self
            .filter
            .as_ref()
            .map(|f| (f.result.query_hash, f.result.epoch));
        let key = OrderKey {
            generation: self.snapshot.generation,
            dir: self.snapshot.dir,
            sort: self.sort,
            filter: filter_id,
        };
        if self.order_key == Some(key) {
            return;
        }
        self.order_key = Some(key);
        self.order.clear();
        self.order.extend(0..self.snapshot.rows.len());
        let rows = &self.snapshot.rows;
        let sort = self.sort;
        self.order.sort_by(|&a, &b| {
            let (ra, rb) = (&rows[a], &rows[b]);
            let primary = match sort.key {
                SortKey::Disk => ra.disk.cmp(&rb.disk),
                SortKey::Apparent => ra.apparent.cmp(&rb.apparent),
                SortKey::Name => ra.name.cmp(&rb.name),
                SortKey::Mtime => ra.mtime.cmp(&rb.mtime),
                SortKey::Items => ra.items.cmp(&rb.items),
                SortKey::Errors => ra.errors.cmp(&rb.errors),
            };
            let primary = if sort.descending {
                primary.reverse()
            } else {
                primary
            };
            // Stable tie-break: name, raw bytes ascending — never affected
            // by the direction toggle.
            primary.then_with(|| ra.name.cmp(&rb.name))
        });
        // D4 composition: under an active filter, a scanned-directory row
        // is kept only when its filtered subtree has a match (its own
        // totals are swapped for the filtered ones at render time, in
        // `ui.rs`'s `draw_table`); every other row (file, symlink, device,
        // or an excluded-mount/stat-failed dir stub — all candidates in
        // their own right) is kept only when it is itself in the match
        // set, hardlink extras included (attack finding 1: present at 0
        // bytes, never silently absent). This is exactly the row set
        // `Self::row_count`/`Self::rows_indexed`/`Self::selected` already
        // read through `self.order`, so the cursor, mouse hit-testing and
        // mark toggling automatically agree with what's rendered without
        // any of them needing to know a filter exists.
        if let Some(filter) = self.filter.as_ref() {
            let result = &filter.result;
            self.order.retain(|&i| {
                let row = &rows[i];
                match row.dir {
                    Some(dir) => result.dir_total(dir).entries > 0,
                    None => result.matched.contains(row.node),
                }
            });
        }
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.row_count().saturating_sub(1));
    }

    // ---- flat view + pattern breakdown modes (D3) ----

    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    /// Switch modes, resetting the cursor to the top (like a fresh view)
    /// and clearing the mouse hover — the same "new view" treatment a tree
    /// navigation gets. A no-op switch to the mode already active leaves
    /// the cursor alone (so `t` while already in `FlatTop` — handled by
    /// [`Self::toggle_flat_top`] as a switch *back to Tree* — never
    /// silently resets position for nothing).
    pub fn set_mode(&mut self, mode: ViewMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.cursor = 0;
        self.clear_hover();
        // A mode switch changes every row and slice on screen just like a
        // navigation or a sort — worth the same reveal/morph flourish
        // (design slice 5).
        self.view_change_seq += 1;
    }

    /// `t`: flat top files. Pressing it again while already in that mode
    /// returns to the tree (D3's "`t`/`b` toggle back to tree").
    pub fn toggle_flat_top(&mut self) {
        let target = if self.mode == ViewMode::FlatTop {
            ViewMode::Tree
        } else {
            ViewMode::FlatTop
        };
        self.set_mode(target);
    }

    /// `b`: pattern breakdown. Pressing it again while already in that
    /// mode returns to the tree.
    pub fn toggle_breakdown(&mut self) {
        let target = if self.mode == ViewMode::Breakdown {
            ViewMode::Tree
        } else {
            ViewMode::Breakdown
        };
        self.set_mode(target);
    }

    /// Contextual Esc's mode-leaving step (D3/attack): back to the tree
    /// from either flat mode. A no-op from the tree itself — the caller
    /// only calls this after checking [`Self::mode`] is not already
    /// [`ViewMode::Tree`] (that case quits instead).
    pub fn leave_mode(&mut self) {
        self.set_mode(ViewMode::Tree);
    }

    /// The flat/breakdown summary currently held (provisional during a
    /// scan, authoritative post-scan — see the field doc on
    /// [`Self::flat`]).
    pub fn flat_summary(&self) -> Option<&FlatSummary> {
        self.flat.as_deref()
    }

    /// Adopt a fresh flat-view summary — called every frame during a scan
    /// (mirroring [`Self::apply_snapshot`]) and, post-scan, only when
    /// [`Self::flat_epoch`] disagrees with the cached summary (the
    /// render-time epoch check `ui.rs` performs before drawing a
    /// flat/breakdown frame).
    pub fn set_flat_summary(&mut self, summary: Arc<FlatSummary>) {
        self.flat = Some(summary);
    }

    /// The deletion epoch marks/deletes have advanced to (see
    /// [`Self::bump_flat_epoch`]) — compared against the cached summary's
    /// own `epoch` to decide whether a recompute is due.
    pub fn flat_epoch(&self) -> u64 {
        self.flat_epoch
    }

    /// Advance the deletion epoch (called once per successful deletion,
    /// regardless of which mode it was performed from) so the very next
    /// render-time check in `ui.rs` recomputes the flat/breakdown summary
    /// before drawing — never showing a just-deleted row as still
    /// occupying space (attack finding 1).
    pub fn bump_flat_epoch(&mut self) {
        self.flat_epoch += 1;
    }

    /// Jump straight to `dir` (Enter on a flat row, D3: "jumps to its
    /// containing directory in tree view, cursor on the file") — unlike
    /// [`Self::jump_to_ancestor`], `dir` need not be on the current
    /// descend stack at all (a flat row can live anywhere in the tree), so
    /// the caller supplies the *full* root-first ancestor chain above it
    /// (walked from the frozen arena) to rebuild the breadcrumb stack from
    /// scratch. `focus` is the node the cursor should land on once the
    /// directory's rows arrive (resolved in [`Self::apply_snapshot`]).
    /// Refused (returns `None`), like every other nav request, while
    /// another one is already in flight.
    pub fn jump_to_directory(
        &mut self,
        dir: DirId,
        ancestors: Vec<DirId>,
        focus: NodeId,
    ) -> Option<DirId> {
        if self.pending_nav.is_some() {
            return None;
        }
        self.stack = ancestors.into_iter().map(|d| (d, 0)).collect();
        self.pending_cursor = 0;
        self.pending_focus_node = Some(focus);
        self.pending_nav = Some(dir);
        Some(dir)
    }

    /// Mark/unmark a flat top-files row by its resolved node/path/disk
    /// size (D3: "marks work on flat rows — real NodeIds, shared basket").
    /// Same guard and toggle mechanics as [`Self::toggle_mark`], just fed
    /// from the caller-resolved flat row instead of a tree [`Row`] (a flat
    /// row's `NodeId`/path/size only exist once the arena is readable,
    /// i.e. the caller already has a [`camembert_core::scan::ScanOutcome`]
    /// in hand). Regular files only (D3), so there is no mount-point
    /// refusal to make here.
    pub fn toggle_mark_flat(
        &mut self,
        node: NodeId,
        path: PathBuf,
        disk: u64,
    ) -> Result<(), MarkRefusal> {
        if !self.snapshot.stats.root_complete {
            return Err(MarkRefusal::ScanRunning);
        }
        let entry = MarkedEntry {
            node,
            path,
            is_dir: false,
            disk,
        };
        if self.marked_set.remove(&entry.node) {
            self.marks.retain(|mark| mark.node != entry.node);
        } else {
            self.marked_set.insert(entry.node);
            self.marks.push(entry);
        }
        self.move_down();
        Ok(())
    }

    // ---- Ctrl-K / `/` palette (D6) ----

    /// Open the palette. `prefill` pre-fills the buffer (used when a
    /// filter is already active: the query stays visible and editable
    /// rather than vanishing into an empty box behind its own effect);
    /// `None`/empty opens with an empty buffer. Always succeeds — even
    /// mid-scan (D2: only *applying* a query is gated, not opening the
    /// palette or using command mode).
    pub fn open_palette(&mut self, prefill: Option<&str>) {
        self.palette = Some(match prefill {
            Some(text) if !text.is_empty() => PaletteState::with_text(text),
            _ => PaletteState::new(),
        });
    }

    /// Close the palette (Esc, or committing a query/command with Enter).
    /// Never touches [`Self::filter`] — whatever was last applied (live,
    /// while typing) stays active (attack finding 12's off-by-one: closing
    /// the palette must not also clear the filter).
    pub fn close_palette(&mut self) {
        self.palette = None;
    }

    pub fn palette_open(&self) -> bool {
        self.palette.is_some()
    }

    pub fn palette(&self) -> Option<&PaletteState> {
        self.palette.as_ref()
    }

    pub fn palette_mut(&mut self) -> Option<&mut PaletteState> {
        self.palette.as_mut()
    }

    // ---- active filter (D2/D4/D5/D6) ----

    pub fn active_filter(&self) -> Option<&ActiveFilter> {
        self.filter.as_ref()
    }

    /// Adopt a freshly accepted filter result. Callers (`ui.rs`) only
    /// reach this after the (fingerprint, epoch) staleness check on the
    /// background fold's arrival — never called with a stale result.
    /// Re-sorts immediately (D4: the row *set* changes, not just totals)
    /// and clamps the cursor/hover the same way a mode switch does.
    pub fn set_active_filter(&mut self, query_text: String, result: Arc<FilterResult>) {
        self.filter = Some(ActiveFilter { query_text, result });
        self.ensure_sorted();
        self.clamp_cursor();
        self.clear_hover();
        self.view_change_seq += 1;
    }

    /// Clear the active filter (Esc from tree view with nothing else to
    /// close, or the "clear active filter" palette command). A no-op when
    /// nothing is active; otherwise restores every row and re-sorts.
    pub fn clear_filter(&mut self) {
        if self.filter.take().is_none() {
            return;
        }
        self.ensure_sorted();
        self.clamp_cursor();
        self.clear_hover();
        self.view_change_seq += 1;
    }
}

/// D3 footer note: totals are provisional while hardlinks were seen and
/// the scan still runs; the note drops once the root completes.
pub fn show_hardlink_note(snapshot: &ViewSnapshot) -> bool {
    snapshot.hardlink_inodes > 0 && !snapshot.stats.root_complete
}

/// D5 "updating…" indicator: the viewed dir publishes on the degraded
/// cadence. Moot once the scan is done (nothing republishes).
pub fn show_updating_note(snapshot: &ViewSnapshot) -> bool {
    snapshot.degraded && !snapshot.stats.root_complete
}

#[cfg(test)]
mod tests {
    use super::*;
    use camembert_core::tree::NodeId;
    use camembert_core::view::{DirTotals, RowState, ScanStats};
    use std::path::PathBuf;
    use std::time::Duration;

    fn stats(root_complete: bool) -> ScanStats {
        ScanStats {
            entries: 10,
            dirs: 3,
            errors: 0,
            disk_bytes: 1024,
            elapsed: Duration::from_millis(50),
            root_complete,
        }
    }

    fn file_row(name: &[u8], disk: u64, apparent: u64, mtime: i64) -> Row {
        Row {
            name: name.into(),
            node: NodeId::from_raw(0),
            dir: None,
            is_dir: false,
            apparent,
            disk,
            items: 1,
            errors: 0,
            state: RowState::File,
            mtime,
        }
    }

    fn dir_row(name: &[u8], dir: u32, disk: u64, items: u64) -> Row {
        Row {
            name: name.into(),
            node: NodeId::from_raw(0),
            dir: Some(DirId::from_raw(dir)),
            is_dir: true,
            apparent: disk,
            disk,
            items,
            errors: 0,
            state: RowState::Scanning,
            mtime: 0,
        }
    }

    fn snapshot(
        generation: u64,
        dir: u32,
        parent: Option<u32>,
        rows: Vec<Row>,
        root_complete: bool,
    ) -> Arc<ViewSnapshot> {
        Arc::new(ViewSnapshot {
            generation,
            dir: DirId::from_raw(dir),
            parent: parent.map(DirId::from_raw),
            path: PathBuf::from("/x"),
            rows,
            totals: DirTotals::default(),
            stats: stats(root_complete),
            hardlink_inodes: 0,
            degraded: false,
        })
    }

    fn names(state: &UiState) -> Vec<&[u8]> {
        state.rows_indexed().map(|(_, r)| &*r.name).collect()
    }

    #[test]
    fn default_sort_is_disk_descending_with_name_tiebreak() {
        let state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![
                file_row(b"small", 10, 1, 0),
                file_row(b"tie-b", 50, 2, 0),
                file_row(b"big", 100, 3, 0),
                file_row(b"tie-a", 50, 4, 0),
            ],
            false,
        ));
        assert_eq!(
            names(&state),
            [b"big" as &[u8], b"tie-a", b"tie-b", b"small"],
            "disk desc, ties by name ascending"
        );
    }

    #[test]
    fn each_sort_key_orders_and_toggles() {
        let mut c = dir_row(b"c", 1, 15, 7);
        c.mtime = 7;
        let rows = vec![file_row(b"b", 10, 300, 5), file_row(b"a", 20, 200, 9), c];
        let mut state = UiState::new(snapshot(1, 0, None, rows, false));

        state.press_sort(SortKey::Apparent);
        assert_eq!(names(&state), [b"b" as &[u8], b"a", b"c"]);

        state.press_sort(SortKey::Name);
        assert_eq!(names(&state), [b"a" as &[u8], b"b", b"c"], "name ascends");
        state.press_sort(SortKey::Name);
        assert_eq!(names(&state), [b"c" as &[u8], b"b", b"a"], "toggled");

        state.press_sort(SortKey::Mtime);
        assert_eq!(names(&state), [b"a" as &[u8], b"c", b"b"], "newest first");

        state.press_sort(SortKey::Items);
        assert_eq!(names(&state), [b"c" as &[u8], b"a", b"b"]);

        state.press_sort(SortKey::Disk);
        assert_eq!(names(&state), [b"a" as &[u8], b"c", b"b"]);
        state.press_sort(SortKey::Disk);
        assert_eq!(names(&state), [b"b" as &[u8], b"c", b"a"], "disk asc");
    }

    #[test]
    fn cursor_moves_and_clamps_on_shrinking_rows() {
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![
                file_row(b"a", 3, 3, 0),
                file_row(b"b", 2, 2, 0),
                file_row(b"c", 1, 1, 0),
            ],
            false,
        ));
        state.move_down();
        state.move_down();
        state.move_down(); // clamped at the last row
        assert_eq!(state.cursor(), 2);
        state.move_bottom();
        assert_eq!(state.cursor(), 2);

        // Same dir, fewer rows (can happen when re-viewing after nav):
        // the cursor clamps instead of pointing past the end.
        state.apply_snapshot(snapshot(2, 0, None, vec![file_row(b"a", 3, 3, 0)], false));
        assert_eq!(state.cursor(), 0);

        // Empty view: cursor pinned to 0, selection is None.
        state.apply_snapshot(snapshot(3, 0, None, Vec::new(), false));
        assert_eq!(state.cursor(), 0);
        assert!(state.selected().is_none());
        state.move_down();
        state.move_bottom();
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn select_at_clamps_and_clears_hover() {
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![
                file_row(b"a", 3, 3, 0),
                file_row(b"b", 2, 2, 0),
                file_row(b"c", 1, 1, 0),
            ],
            false,
        ));
        assert_eq!(state.row_count(), 3);
        state.set_hover(2);
        state.select_at(1);
        assert_eq!(state.cursor(), 1);
        assert_eq!(state.hover(), None, "a click reclaims the card too");

        // Out-of-range position (stale geometry from a shrunk view):
        // clamp instead of pointing past the end.
        state.select_at(99);
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn descend_and_ascend_restore_the_cursor() {
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![
                file_row(b"big", 100, 100, 0),
                dir_row(b"sub", 7, 50, 3),
                file_row(b"small", 1, 1, 0),
            ],
            false,
        ));
        // Sorted: big, sub, small. Select "sub".
        state.move_down();
        assert_eq!(&*state.selected().unwrap().name, b"sub");

        let target = state.descend().expect("dir row descends");
        assert_eq!(target, DirId::from_raw(7));
        assert_eq!(state.pending_nav(), Some(target));
        // Optimistic: rows unchanged until the new snapshot arrives.
        assert_eq!(names(&state), [b"big" as &[u8], b"sub", b"small"]);
        // Further nav ignored while pending.
        assert_eq!(state.descend(), None);
        assert_eq!(state.ascend(), None);

        // The requested dir's snapshot arrives: cursor at the top.
        state.apply_snapshot(snapshot(
            2,
            7,
            Some(0),
            vec![file_row(b"x", 30, 30, 0), file_row(b"y", 20, 20, 0)],
            false,
        ));
        assert_eq!(state.pending_nav(), None);
        assert_eq!(state.cursor(), 0);

        // Go back up: cursor restored onto "sub".
        let parent = state.ascend().expect("has a parent");
        assert_eq!(parent, DirId::from_raw(0));
        state.apply_snapshot(snapshot(
            3,
            0,
            None,
            vec![
                file_row(b"big", 100, 100, 0),
                dir_row(b"sub", 7, 50, 3),
                file_row(b"small", 1, 1, 0),
            ],
            false,
        ));
        assert_eq!(state.cursor(), 1);
        assert_eq!(&*state.selected().unwrap().name, b"sub");
    }

    #[test]
    fn jump_to_ancestor_skips_several_levels_in_one_request() {
        // root (dir 0) -> a (dir 1) -> b (dir 2) -> c (dir 3), descending
        // one level at a time like the keyboard path would.
        let mut state = UiState::new(snapshot(1, 0, None, vec![dir_row(b"a", 1, 10, 1)], false));
        state.descend();
        state.apply_snapshot(snapshot(
            2,
            1,
            Some(0),
            vec![dir_row(b"b", 2, 10, 1)],
            false,
        ));
        state.descend();
        state.apply_snapshot(snapshot(
            3,
            2,
            Some(1),
            vec![dir_row(b"c", 3, 10, 1)],
            false,
        ));
        state.descend();
        state.apply_snapshot(snapshot(
            4,
            3,
            Some(2),
            vec![file_row(b"leaf", 1, 1, 0)],
            false,
        ));

        // Root, a, b are all still open above the current dir.
        assert_eq!(
            state.stack_dirs().collect::<Vec<_>>(),
            vec![DirId::from_raw(0), DirId::from_raw(1), DirId::from_raw(2)]
        );

        // One breadcrumb click on the root jumps straight there — a
        // single navigation request, not three.
        let target = state
            .jump_to_ancestor(DirId::from_raw(0))
            .expect("root is on the stack");
        assert_eq!(target, DirId::from_raw(0));
        assert_eq!(state.pending_nav(), Some(DirId::from_raw(0)));
        // The intermediate levels (a, b) are gone from the stack: from
        // here, ascend() would have nothing left to restore beyond root.
        assert_eq!(state.stack_dirs().collect::<Vec<_>>(), Vec::new());

        state.apply_snapshot(snapshot(5, 0, None, vec![dir_row(b"a", 1, 10, 1)], false));
        assert_eq!(state.pending_nav(), None);
    }

    #[test]
    fn jump_to_ancestor_refuses_the_current_dir_and_unknown_targets() {
        let mut state = UiState::new(snapshot(1, 0, None, vec![dir_row(b"a", 1, 10, 1)], false));
        state.descend();
        state.apply_snapshot(snapshot(
            2,
            1,
            Some(0),
            vec![file_row(b"f", 1, 1, 0)],
            false,
        ));

        assert_eq!(
            state.jump_to_ancestor(DirId::from_raw(1)),
            None,
            "already there"
        );
        assert_eq!(
            state.jump_to_ancestor(DirId::from_raw(99)),
            None,
            "not an open ancestor"
        );
        // Neither refusal disturbed the stack.
        assert_eq!(
            state.stack_dirs().collect::<Vec<_>>(),
            vec![DirId::from_raw(0)]
        );
    }

    #[test]
    fn card_row_prefers_hover_then_falls_back_to_cursor() {
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![file_row(b"a", 20, 20, 0), file_row(b"b", 10, 10, 0)],
            false,
        ));
        // Disk-desc default order: a (cursor 0), b (position 1).
        assert_eq!(&*state.card_row().unwrap().name, b"a", "no hover: cursor");
        state.set_hover(1);
        assert_eq!(&*state.card_row().unwrap().name, b"b", "hover wins");
        assert_eq!(state.hover(), Some(1));

        // Any keyboard movement reclaims the card for the cursor.
        state.move_down();
        assert_eq!(state.hover(), None, "keyboard clears hover");
        assert_eq!(&*state.card_row().unwrap().name, b"b", "now via cursor");
    }

    #[test]
    fn hover_does_not_survive_a_snapshot_change() {
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![file_row(b"a", 20, 20, 0), file_row(b"b", 10, 10, 0)],
            false,
        ));
        state.set_hover(1);
        state.apply_snapshot(snapshot(
            1,
            0,
            None,
            vec![file_row(b"a", 20, 20, 0), file_row(b"b", 10, 10, 0)],
            false,
        ));
        assert_eq!(state.hover(), None, "reapplying a snapshot drops hover");
    }

    #[test]
    fn table_geometry_hit_test() {
        let geometry = TableGeometry {
            x: 2,
            y: 5,
            width: 20,
            height: 3,
            offset: 4,
        };
        assert_eq!(geometry.hit_test(10, 5), Some(4), "first visible row");
        assert_eq!(geometry.hit_test(10, 7), Some(6), "third visible row");
        assert_eq!(geometry.hit_test(10, 8), None, "below the table");
        assert_eq!(geometry.hit_test(1, 5), None, "left of the table");
        assert_eq!(geometry.hit_test(22, 5), None, "right of the table");
    }

    #[test]
    fn wheel_geometry_hit_test() {
        // 2x2 grid: slice 0 top-left, slice 1 (rest) top-right, empty
        // bottom row (outside the disc).
        let geometry = WheelGeometry {
            x: 10,
            y: 3,
            width: 2,
            height: 2,
            cells: vec![Some(0), Some(1), None, None],
            targets: vec![Some(7), None], // slice 1 is the unnavigable rest
        };
        assert_eq!(geometry.hit_test(10, 3), Some(7), "slice 0 -> its row");
        assert_eq!(geometry.hit_test(11, 3), None, "rest slice: not navigable");
        assert_eq!(geometry.hit_test(10, 4), None, "outside the disc");
        assert_eq!(geometry.hit_test(0, 0), None, "outside the wheel area");
    }

    #[test]
    fn frame_geometry_breadcrumb_and_card_hit_tests() {
        let geometry = FrameGeometry {
            breadcrumb_row: 0,
            breadcrumb: vec![(1, 5, Some(DirId::from_raw(0))), (6, 9, None)],
            errors_card: Some((20, 1, 10, 3)),
            ..Default::default()
        };
        assert_eq!(geometry.breadcrumb_hit(2, 0), Some(DirId::from_raw(0)));
        assert_eq!(geometry.breadcrumb_hit(7, 0), None, "current dir segment");
        assert_eq!(geometry.breadcrumb_hit(2, 1), None, "wrong row");
        assert!(geometry.errors_card_hit(25, 2));
        assert!(!geometry.errors_card_hit(5, 2));
    }

    #[test]
    fn descend_on_a_file_or_excluded_dir_is_a_no_op() {
        let mut excluded = dir_row(b"mnt", 0, 5, 1);
        excluded.dir = None; // other-filesystem mount point: not navigable
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![file_row(b"f", 10, 10, 0), excluded],
            false,
        ));
        assert_eq!(state.descend(), None, "file");
        state.move_down();
        assert_eq!(state.descend(), None, "excluded mount point");
        assert!(state.pending_nav().is_none());
    }

    #[test]
    fn ascend_at_the_root_is_a_no_op() {
        let mut state = UiState::new(snapshot(1, 0, None, vec![file_row(b"f", 1, 1, 0)], false));
        assert_eq!(state.ascend(), None);
    }

    #[test]
    fn resort_happens_only_on_generation_or_sort_change() {
        let snap = snapshot(
            1,
            0,
            None,
            vec![file_row(b"a", 1, 1, 0), file_row(b"b", 2, 2, 0)],
            false,
        );
        let mut state = UiState::new(Arc::clone(&snap));
        let before: Vec<Vec<u8>> = names(&state).into_iter().map(<[u8]>::to_vec).collect();
        // Same generation re-applied: cached permutation kept (observable
        // as identical order; the cache key equality is the guard).
        state.apply_snapshot(snap);
        assert_eq!(names(&state), before);

        // New generation with different sizes: re-sorted.
        state.apply_snapshot(snapshot(
            2,
            0,
            None,
            vec![file_row(b"a", 9, 9, 0), file_row(b"b", 2, 2, 0)],
            false,
        ));
        assert_eq!(names(&state), [b"a" as &[u8], b"b"]);
    }

    /// Marking test fixture: big (file, node 1), sub (dir, node 2), mnt
    /// (excluded mount, node 3) — scan complete unless stated otherwise.
    fn markable_rows() -> Vec<Row> {
        let mut big = file_row(b"big", 100, 100, 0);
        big.node = NodeId::from_raw(1);
        let mut sub = dir_row(b"sub", 7, 50, 3);
        sub.node = NodeId::from_raw(2);
        sub.state = RowState::Complete;
        let mut mnt = dir_row(b"mnt", 0, 5, 1);
        mnt.node = NodeId::from_raw(3);
        mnt.dir = None; // excluded mount point: no directory metadata
        vec![big, sub, mnt]
    }

    #[test]
    fn marking_is_locked_out_while_the_scan_runs() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), false));
        assert_eq!(state.toggle_mark(), Err(MarkRefusal::ScanRunning));
        assert!(state.marks().is_empty());
        assert!(!state.open_confirm(0, None), "confirm locked out too");
        assert!(state.confirm().is_none());
    }

    #[test]
    fn marking_toggles_captures_the_path_and_moves_down() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        // Sorted by disk desc: big, sub, mnt.
        assert_eq!(state.toggle_mark(), Ok(()));
        assert_eq!(state.cursor(), 1, "space moves down like dua");
        assert!(state.is_marked(NodeId::from_raw(1)));
        assert_eq!(state.marks().len(), 1);
        let mark = &state.marks()[0];
        assert_eq!(mark.path, PathBuf::from("/x/big"));
        assert!(!mark.is_dir);
        assert_eq!(mark.disk, 100);

        // Marking a dir marks the subtree implicitly: one entry, the dir.
        assert_eq!(state.toggle_mark(), Ok(()));
        assert_eq!(state.marks().len(), 2);
        assert!(state.marks()[1].is_dir);

        // Toggling an already-marked row unmarks it.
        state.move_up();
        assert_eq!(state.toggle_mark(), Ok(()));
        assert!(!state.is_marked(NodeId::from_raw(2)));
        assert_eq!(state.marks().len(), 1);
    }

    #[test]
    fn mount_points_refuse_marks_and_empty_views_are_a_no_op() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        state.move_bottom(); // "mnt", the excluded mount point
        assert_eq!(state.toggle_mark(), Err(MarkRefusal::MountPoint));
        assert!(state.marks().is_empty());

        let mut empty = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert_eq!(empty.toggle_mark(), Ok(()), "no row: silent no-op");
        assert!(empty.marks().is_empty());
    }

    #[test]
    fn error_rows_stay_markable() {
        let mut err_dir = dir_row(b"locked", 4, 10, 1);
        err_dir.node = NodeId::from_raw(9);
        err_dir.state = RowState::Error;
        let mut state = UiState::new(snapshot(1, 0, None, vec![err_dir], true));
        assert_eq!(state.toggle_mark(), Ok(()), "unreadable dirs are deletable");
        assert!(state.is_marked(NodeId::from_raw(9)));
    }

    #[test]
    fn marked_summary_sums_count_and_disk() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        assert_eq!(state.marked_summary(), None);
        state.toggle_mark().unwrap(); // big: 100
        state.toggle_mark().unwrap(); // sub: 50 (subtree total)
        assert_eq!(state.marked_summary(), Some((2, 150)));
        state.unmark_all();
        assert_eq!(state.marked_summary(), None);
        assert!(!state.is_marked(NodeId::from_raw(1)));
    }

    #[test]
    fn confirm_modal_state_machine() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        assert!(
            !state.open_confirm(0, None),
            "nothing marked: refuses to open"
        );
        assert!(state.take_confirmed_marks().is_none(), "nothing to confirm");

        state.toggle_mark().unwrap();
        assert!(state.open_confirm(2, None));
        assert_eq!(state.confirm().unwrap().hardlink_files, 2);

        // Esc / any non-y key: cancel, marks intact.
        state.cancel_confirm();
        assert!(state.confirm().is_none());
        assert_eq!(state.marks().len(), 1, "cancel keeps the marks");

        // Reopen and confirm: marks handed over and cleared.
        assert!(state.open_confirm(0, None));
        let confirmed = state.take_confirmed_marks().expect("modal was open");
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].node, NodeId::from_raw(1));
        assert!(state.confirm().is_none());
        assert!(state.marks().is_empty());
        assert!(!state.is_marked(NodeId::from_raw(1)));
    }

    #[test]
    fn review_refuses_to_open_with_nothing_marked_and_moves_within_bounds() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        assert!(!state.open_review(), "nothing marked: refused");
        assert!(state.review().is_none());

        state.toggle_mark().unwrap(); // big
        state.toggle_mark().unwrap(); // sub
        assert!(state.open_review());
        assert_eq!(state.review().unwrap().cursor, 0);

        state.review_move_down();
        assert_eq!(state.review().unwrap().cursor, 1);
        state.review_move_down(); // clamped: only 2 marks
        assert_eq!(state.review().unwrap().cursor, 1);
        state.review_move_up();
        state.review_move_up(); // clamped at 0
        assert_eq!(state.review().unwrap().cursor, 0);

        state.close_review();
        assert!(state.review().is_none());
        assert_eq!(state.marks().len(), 2, "closing keeps the marks");
    }

    #[test]
    fn unmark_at_review_cursor_removes_exactly_that_entry_and_closes_when_empty() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        state.toggle_mark().unwrap(); // big (node 1)
        state.toggle_mark().unwrap(); // sub (node 2)
        state.open_review();
        state.review_move_down(); // cursor on "sub"

        state.unmark_at_review_cursor();
        assert!(state.review().is_some(), "one mark left: list stays open");
        assert_eq!(state.marks().len(), 1);
        assert!(state.is_marked(NodeId::from_raw(1)), "big unaffected");
        assert!(!state.is_marked(NodeId::from_raw(2)), "sub unmarked");
        assert_eq!(
            state.review().unwrap().cursor,
            0,
            "clamped onto the last row"
        );

        state.unmark_at_review_cursor();
        assert!(state.review().is_none(), "last mark gone: list auto-closes");
        assert!(state.marks().is_empty());
    }

    #[test]
    fn unmark_at_review_cursor_without_a_review_open_is_a_no_op() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        state.toggle_mark().unwrap();
        state.unmark_at_review_cursor(); // no review open
        assert_eq!(state.marks().len(), 1, "untouched");
    }

    #[test]
    fn cheatsheet_toggles() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert!(!state.cheatsheet_open());
        state.open_cheatsheet();
        assert!(state.cheatsheet_open());
        state.close_cheatsheet();
        assert!(!state.cheatsheet_open());
    }

    #[test]
    fn zen_mode_toggles() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert!(!state.zen());
        state.toggle_zen();
        assert!(state.zen());
        state.toggle_zen();
        assert!(!state.zen());
    }

    #[test]
    fn view_change_seq_bumps_on_navigation_and_sort_but_not_on_live_updates() {
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![dir_row(b"sub", 7, 50, 3), file_row(b"f", 1, 1, 0)],
            false,
        ));
        let baseline = state.view_change_seq();

        // A live re-application of the same dir (same generation bumped,
        // scan progress) must never bump the seq — that would fight the
        // donut's continuous live growth (design slice 5).
        state.apply_snapshot(snapshot(
            2,
            0,
            None,
            vec![dir_row(b"sub", 7, 80, 3), file_row(b"f", 1, 1, 0)],
            false,
        ));
        assert_eq!(
            state.view_change_seq(),
            baseline,
            "same dir, values changed: not a view change"
        );

        // An actual navigation (dir changes) bumps it.
        state.descend();
        state.apply_snapshot(snapshot(3, 7, Some(0), Vec::new(), false));
        assert_eq!(state.view_change_seq(), baseline + 1, "navigated");

        // A sort keypress bumps it too, even without navigating.
        state.press_sort(SortKey::Name);
        assert_eq!(state.view_change_seq(), baseline + 2, "sorted");
    }

    #[test]
    fn toggle_apparent_flips_the_flag() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert!(state.show_apparent, "starts shown");
        state.toggle_apparent();
        assert!(!state.show_apparent);
        state.toggle_apparent();
        assert!(state.show_apparent);
    }

    #[test]
    fn footer_notes_follow_the_scan_state() {
        let mut snap = ViewSnapshot {
            generation: 1,
            dir: DirId::from_raw(0),
            parent: None,
            path: PathBuf::from("/x"),
            rows: Vec::new(),
            totals: DirTotals::default(),
            stats: stats(false),
            hardlink_inodes: 2,
            degraded: true,
        };
        assert!(show_hardlink_note(&snap), "hardlinks + scanning: shown");
        assert!(show_updating_note(&snap), "degraded + scanning: shown");

        snap.stats.root_complete = true;
        assert!(!show_hardlink_note(&snap), "corrected at scan end (D3)");
        assert!(!show_updating_note(&snap));

        snap.stats.root_complete = false;
        snap.hardlink_inodes = 0;
        snap.degraded = false;
        assert!(!show_hardlink_note(&snap));
        assert!(!show_updating_note(&snap));
    }

    // ---- `f` freeable panel (freeable phase 1) ----

    #[test]
    fn freeable_panel_open_close_and_scroll() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert!(!state.freeable_open());
        state.open_freeable_panel();
        assert!(state.freeable_open());
        assert_eq!(state.freeable_cursor(), 0);

        state.freeable_move_down();
        state.freeable_move_down();
        assert_eq!(state.freeable_cursor(), 2, "unclamped between frames");
        // Only 2 rows actually rendered this frame: the post-draw feedback
        // reins the cursor in onto the last valid row (index 1).
        state.clamp_freeable_cursor(2);
        assert_eq!(state.freeable_cursor(), 1);

        state.freeable_move_up();
        assert_eq!(state.freeable_cursor(), 0);
        state.freeable_move_up();
        assert_eq!(state.freeable_cursor(), 0, "saturates, never underflows");

        state.close_freeable_panel();
        assert!(!state.freeable_open());
    }

    #[test]
    fn freeable_ledger_and_groups_round_trip_and_invalidate() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert!(state.freeable_ledger().is_none());
        assert!(!state.freeable_groups_built());

        // A degenerate root_dev (unlikely to match anything real) is fine
        // here: this test exercises the Option/cache plumbing, not the
        // sweep's own classification (covered in camembert-core).
        state.set_freeable_ledger(camembert_core::freeable::sweep(0));
        assert!(state.freeable_ledger().is_some());
        assert!(
            !state.freeable_groups_built(),
            "a fresh ledger clears any cached grouping"
        );

        state.set_freeable_groups(Vec::new());
        assert!(state.freeable_groups_built());
        assert!(state.freeable_groups().is_empty());

        state.set_freeable_ledger(camembert_core::freeable::sweep(0));
        assert!(
            !state.freeable_groups_built(),
            "the next ledger invalidates the cache again"
        );
    }

    #[test]
    fn confirm_modal_carries_the_open_file_warning() {
        let mut state = UiState::new(snapshot(1, 0, None, markable_rows(), true));
        state.toggle_mark().unwrap();
        let warning = OpenWarning {
            entries_open: 1,
            contained_open: 0,
            holder_count: 1,
            top_holders: vec![(123, Some("nginx".to_owned()))],
            partial_coverage: None,
        };
        assert!(state.open_confirm(0, Some(warning.clone())));
        assert_eq!(state.confirm().unwrap().open_warning, Some(warning));

        // No warning (nothing open, or --no-proc-sweep): no line to show.
        state.cancel_confirm();
        assert!(state.open_confirm(0, None));
        assert!(state.confirm().unwrap().open_warning.is_none());
    }

    // ---- `t`/`b` flat view modes + pattern breakdown (D3) ----

    fn flat_summary_fixture() -> Arc<FlatSummary> {
        use camembert_core::flat::{GroupTotal, PatternKind, RestTotal, TopFile};
        Arc::new(FlatSummary {
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
                    node: NodeId::from_raw(10),
                    name: "file10".into(),
                    disk: 900,
                    hardlink: false,
                },
                TopFile {
                    node: NodeId::from_raw(11),
                    name: "file11".into(),
                    disk: 500,
                    hardlink: true,
                },
                TopFile {
                    node: NodeId::from_raw(12),
                    name: "file12".into(),
                    disk: 100,
                    hardlink: false,
                },
            ],
            truncated: false,
            provisional: true,
            epoch: 0,
        })
    }

    #[test]
    fn mode_defaults_to_tree_and_t_b_toggle_back() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert_eq!(state.mode(), ViewMode::Tree);

        state.toggle_flat_top();
        assert_eq!(state.mode(), ViewMode::FlatTop);
        state.toggle_flat_top();
        assert_eq!(state.mode(), ViewMode::Tree, "t again returns to tree");

        state.toggle_breakdown();
        assert_eq!(state.mode(), ViewMode::Breakdown);
        state.toggle_breakdown();
        assert_eq!(state.mode(), ViewMode::Tree, "b again returns to tree");

        // Switching directly from one flat mode to the other (t while in
        // breakdown) lands on the other mode, not tree.
        state.toggle_breakdown();
        state.toggle_flat_top();
        assert_eq!(state.mode(), ViewMode::FlatTop);
    }

    #[test]
    fn leave_mode_is_the_esc_step_and_is_a_no_op_from_tree() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        state.leave_mode(); // already Tree: no-op
        assert_eq!(state.mode(), ViewMode::Tree);

        state.toggle_breakdown();
        state.leave_mode();
        assert_eq!(
            state.mode(),
            ViewMode::Tree,
            "contextual Esc leaves the mode"
        );
    }

    #[test]
    fn mode_switch_resets_cursor_and_bumps_view_change_seq() {
        let mut state = UiState::new(snapshot(
            1,
            0,
            None,
            vec![file_row(b"a", 3, 3, 0), file_row(b"b", 2, 2, 0)],
            true,
        ));
        state.move_down();
        assert_eq!(state.cursor(), 1);
        let baseline = state.view_change_seq();

        state.set_flat_summary(flat_summary_fixture());
        state.toggle_flat_top();
        assert_eq!(state.cursor(), 0, "fresh view starts at the top");
        assert!(state.view_change_seq() > baseline);
    }

    #[test]
    fn row_count_and_cursor_bounds_follow_the_active_mode() {
        let mut state = UiState::new(snapshot(1, 0, None, vec![file_row(b"a", 3, 3, 0)], true));
        assert_eq!(state.row_count(), 1, "tree mode: one row");

        state.set_flat_summary(flat_summary_fixture());
        state.toggle_flat_top();
        assert_eq!(state.row_count(), 3, "flat mode: three top files");
        state.move_bottom();
        assert_eq!(state.cursor(), 2);
        state.move_down(); // clamped: no fourth row
        assert_eq!(state.cursor(), 2);

        state.toggle_flat_top(); // back to tree
        state.toggle_breakdown();
        assert_eq!(
            state.row_count(),
            2,
            "breakdown mode: one group + the trailing rest row"
        );
    }

    #[test]
    fn row_count_is_zero_before_any_flat_summary_arrives() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        state.toggle_flat_top();
        assert_eq!(state.row_count(), 0);
        state.move_down(); // must not panic/underflow with nothing to show
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn toggle_mark_flat_marks_a_real_node_and_shares_the_basket() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert_eq!(state.marked_summary(), None);
        assert_eq!(
            state.toggle_mark_flat(NodeId::from_raw(42), PathBuf::from("/x/big.bin"), 900),
            Ok(())
        );
        assert!(state.is_marked(NodeId::from_raw(42)));
        assert_eq!(state.marked_summary(), Some((1, 900)));
        let mark = &state.marks()[0];
        assert!(!mark.is_dir, "flat rows are always regular files");
        assert_eq!(mark.path, PathBuf::from("/x/big.bin"));

        // Toggling again unmarks it — same basket the tree/review/delete
        // flow already reads from.
        assert_eq!(
            state.toggle_mark_flat(NodeId::from_raw(42), PathBuf::from("/x/big.bin"), 900),
            Ok(())
        );
        assert!(!state.is_marked(NodeId::from_raw(42)));
        assert_eq!(state.marked_summary(), None);
    }

    #[test]
    fn toggle_mark_flat_is_locked_while_the_scan_runs() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), false));
        assert_eq!(
            state.toggle_mark_flat(NodeId::from_raw(1), PathBuf::from("/x/f"), 10),
            Err(MarkRefusal::ScanRunning)
        );
        assert!(state.marks().is_empty());
    }

    #[test]
    fn jump_to_directory_rebuilds_the_stack_and_focuses_the_node() {
        // Start somewhere unrelated to the jump target (dir 99): the
        // ancestor chain the caller supplies fully replaces the stack.
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        let target = state.jump_to_directory(
            DirId::from_raw(5),
            vec![DirId::from_raw(0), DirId::from_raw(2)],
            NodeId::from_raw(77),
        );
        assert_eq!(target, Some(DirId::from_raw(5)));
        assert_eq!(state.pending_nav(), Some(DirId::from_raw(5)));
        assert_eq!(
            state.stack_dirs().collect::<Vec<_>>(),
            vec![DirId::from_raw(0), DirId::from_raw(2)]
        );

        // Refused while another nav is already pending.
        assert_eq!(
            state.jump_to_directory(DirId::from_raw(6), vec![], NodeId::from_raw(1)),
            None
        );

        // The target directory's snapshot arrives with the focus node as
        // its second row: cursor lands there, not at position 0.
        let mut a = file_row(b"a", 5, 5, 0);
        a.node = NodeId::from_raw(1);
        let mut focus_row = file_row(b"big", 200, 200, 0);
        focus_row.node = NodeId::from_raw(77);
        state.apply_snapshot(snapshot(2, 5, Some(2), vec![a, focus_row], true));
        assert_eq!(state.pending_nav(), None);
        assert_eq!(
            &*state.selected().unwrap().name,
            b"big",
            "cursor on the file"
        );
    }

    #[test]
    fn jump_to_directory_focus_is_discarded_on_a_stale_snapshot() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        state.jump_to_directory(DirId::from_raw(5), vec![], NodeId::from_raw(77));
        // A snapshot for a *different* directory arrives first (a stale
        // publish overtaken by the request): the focus must not leak into
        // whatever directory happens to resolve next.
        state.apply_snapshot(snapshot(2, 9, None, vec![file_row(b"x", 1, 1, 0)], true));
        assert_eq!(state.cursor(), 0);
        // The real target's snapshot then arrives with no row matching
        // the old focus node: falls back to the top, not a panic.
        state.jump_to_directory(DirId::from_raw(5), vec![], NodeId::from_raw(77));
        state.apply_snapshot(snapshot(3, 5, None, vec![file_row(b"y", 1, 1, 0)], true));
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn flat_epoch_bumps_independently_of_the_cached_summary() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert_eq!(state.flat_epoch(), 0);
        state.bump_flat_epoch();
        state.bump_flat_epoch();
        assert_eq!(state.flat_epoch(), 2);
        assert!(
            state.flat_summary().is_none(),
            "epoch and summary are independent"
        );
    }

    // ---- Ctrl-K / `/` palette + active filter (D6) ----

    #[test]
    fn palette_opens_empty_or_prefilled_and_closes() {
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        assert!(!state.palette_open());
        state.open_palette(None);
        assert!(state.palette_open());
        assert_eq!(state.palette().unwrap().text(), "");
        state.close_palette();
        assert!(!state.palette_open());

        state.open_palette(Some("*.log"));
        assert_eq!(state.palette().unwrap().text(), "*.log");
    }

    #[test]
    fn closing_the_palette_never_clears_an_active_filter() {
        // Attack finding 12's off-by-one: Esc that closes the palette must
        // not also clear the filter it just applied.
        let mut state = UiState::new(snapshot(1, 0, None, Vec::new(), true));
        let result = std::sync::Arc::new(sample_filter_result());
        state.set_active_filter("*.log".to_owned(), result);
        state.open_palette(Some("*.log"));
        state.close_palette();
        assert!(!state.palette_open());
        assert!(
            state.active_filter().is_some(),
            "closing the palette must not clear the filter"
        );
        state.clear_filter();
        assert!(state.active_filter().is_none());
    }

    /// A synthetic `FilterResult`, built with the real `apply()` over a
    /// trivially empty tree — `FilterResult`'s fields are mostly private
    /// to `camembert-core` by design (see `filterview.rs`'s own tests),
    /// so every test here that needs one goes through the public engine
    /// entry point rather than a hand-built literal.
    fn sample_filter_result() -> camembert_core::query::FilterResult {
        use camembert_core::flat::PatternSet;
        use camembert_core::query::{ApplyOptions, HardlinkIndex, apply, parse};
        use camembert_core::scan::{ScanOptions, Scanner};
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.log"), b"hi").unwrap();
        let mut outcome = Scanner::new(ScanOptions {
            statx_engine: Default::default(),
            threads: 1,
            cross_filesystems: false,
        })
        .scan(tmp.path())
        .expect("scan");
        outcome.finalize_hardlinks();
        let parsed = parse("*.log");
        let hardlinks = HardlinkIndex::build(&outcome, 0);
        apply(
            outcome.tree(),
            &parsed.query,
            &PatternSet::default(),
            &hardlinks,
            &ApplyOptions {
                cap: 10,
                epoch: 0,
                now_unix: 0,
                threads: 1,
            },
        )
    }

    #[test]
    fn directory_marks_are_refused_under_an_active_filter_but_files_are_fine() {
        use camembert_core::flat::PatternSet;
        use camembert_core::query::{ApplyOptions, HardlinkIndex, apply, parse};
        use camembert_core::scan::{ScanOptions, Scanner};
        use camembert_core::view;

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("keep.log"), b"hello").unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/other.log"), b"world").unwrap();

        let mut outcome = Scanner::new(ScanOptions {
            statx_engine: Default::default(),
            threads: 1,
            cross_filesystems: false,
        })
        .scan(tmp.path())
        .expect("scan");
        outcome.finalize_hardlinks();
        let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
        let snapshot = view::build_snapshot(
            outcome.tree(),
            outcome.root(),
            1,
            stats,
            outcome.hardlink_inodes,
            false,
        );
        let mut state = UiState::new(Arc::new(snapshot));

        let parsed = parse("*.log");
        assert!(parsed.errors.is_empty());
        let hardlinks = HardlinkIndex::build(&outcome, 0);
        let result = apply(
            outcome.tree(),
            &parsed.query,
            &PatternSet::default(),
            &hardlinks,
            &ApplyOptions {
                cap: 10,
                epoch: 0,
                now_unix: 0,
                threads: 1,
            },
        );
        state.set_active_filter("*.log".to_owned(), Arc::new(result));

        // Both rows survive: "keep.log" matches directly, "sub" has a
        // matching descendant ("sub/other.log").
        assert_eq!(state.row_count(), 2, "neither row is fully unmatched");
        let file_position = state
            .rows_indexed()
            .position(|(_, row)| !row.is_dir)
            .expect("the file row survived the filter");
        let dir_position = state
            .rows_indexed()
            .position(|(_, row)| row.is_dir)
            .expect("the directory row survived the filter");

        state.select_at(file_position);
        assert_eq!(state.toggle_mark(), Ok(()), "file marks still work");
        assert_eq!(state.marks().len(), 1);

        state.select_at(dir_position);
        assert_eq!(
            state.toggle_mark(),
            Err(MarkRefusal::FilterActive),
            "directory marks refused while a filter is active"
        );
        assert_eq!(state.marks().len(), 1, "the refused mark did not land");

        state.clear_filter();
        state.select_at(dir_position);
        assert_eq!(
            state.toggle_mark(),
            Ok(()),
            "clearing the filter restores directory marking"
        );
        assert_eq!(state.marks().len(), 2);
    }

    #[test]
    fn a_filter_that_matches_nothing_hides_every_row_but_stays_navigable() {
        use camembert_core::flat::PatternSet;
        use camembert_core::query::{ApplyOptions, HardlinkIndex, apply, parse};
        use camembert_core::scan::{ScanOptions, Scanner};
        use camembert_core::view;

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.bin"), b"hello").unwrap();
        let mut outcome = Scanner::new(ScanOptions {
            statx_engine: Default::default(),
            threads: 1,
            cross_filesystems: false,
        })
        .scan(tmp.path())
        .expect("scan");
        outcome.finalize_hardlinks();
        let stats = view::scan_stats(outcome.tree(), outcome.root(), outcome.elapsed);
        let snapshot = view::build_snapshot(
            outcome.tree(),
            outcome.root(),
            1,
            stats,
            outcome.hardlink_inodes,
            false,
        );
        let mut state = UiState::new(Arc::new(snapshot));
        let parsed = parse("*.nomatch");
        let hardlinks = HardlinkIndex::build(&outcome, 0);
        let result = apply(
            outcome.tree(),
            &parsed.query,
            &PatternSet::default(),
            &hardlinks,
            &ApplyOptions {
                cap: 10,
                epoch: 0,
                now_unix: 0,
                threads: 1,
            },
        );
        state.set_active_filter("*.nomatch".to_owned(), Arc::new(result));
        // Attack finding 10's amendment: the viewed dir always renders
        // (here as a legitimately empty table), never panics, and the
        // cursor stays sanely at 0 rather than pointing past the end.
        assert_eq!(state.row_count(), 0);
        assert_eq!(state.cursor(), 0);
        assert!(state.selected().is_none());
        state.move_down();
        assert_eq!(state.cursor(), 0);
    }
}
