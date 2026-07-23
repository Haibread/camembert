//! Pure interactive-mode state: sorting, cursor, navigation stack,
//! snapshot application, and the mark-then-confirm deletion state
//! (HANDOFF §5). No terminal types anywhere — everything here is
//! unit-testable with synthetic snapshots.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use camembert_core::tree::{DirId, NodeId};
use camembert_core::view::{Row, ViewSnapshot};

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
}

/// State of the delete-confirmation modal, opened by
/// [`UiState::open_confirm`]. While it exists, every key belongs to the
/// modal: `y` confirms, anything else cancels.
#[derive(Debug, Clone, Copy)]
pub struct ConfirmState {
    /// Hardlinked files among the marked selection (incl. inside marked
    /// dirs); when > 0 the dialog warns that freeing depends on deleting
    /// all links.
    pub hardlink_files: u64,
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
}

/// Cache key for the sorted permutation: re-sort only when any part
/// changes (new generation, different dir, or different sort).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrderKey {
    generation: u64,
    dir: DirId,
    sort: SortSpec,
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
        if self.pending_nav == Some(snapshot.dir) {
            self.pending_nav = None;
            self.cursor = self.pending_cursor;
        } else if dir_changed {
            // A dir change we did not ask for (stale pending overwritten
            // by a later request): start at the top.
            self.cursor = 0;
        }
        self.snapshot = snapshot;
        self.ensure_sorted();
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
        if self.cursor + 1 < self.order.len() {
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
        self.cursor = self.order.len().saturating_sub(1);
        self.clear_hover();
    }

    /// Number of rows in the active (sorted) view.
    pub fn row_count(&self) -> usize {
        self.order.len()
    }

    /// Move the cursor directly to a display-order position (a mouse
    /// click) — clamped defensively in case the row count changed since
    /// the frame the click's geometry came from.
    pub fn select_at(&mut self, position: usize) {
        self.cursor = position.min(self.order.len().saturating_sub(1));
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
    /// [`camembert_core::delete::hardlink_files_in`]).
    pub fn open_confirm(&mut self, hardlink_files: u64) -> bool {
        if self.marks.is_empty() || !self.snapshot.stats.root_complete {
            return false;
        }
        self.confirm = Some(ConfirmState { hardlink_files });
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

    fn ensure_sorted(&mut self) {
        let key = OrderKey {
            generation: self.snapshot.generation,
            dir: self.snapshot.dir,
            sort: self.sort,
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
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.order.len().saturating_sub(1));
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
        assert!(!state.open_confirm(0), "confirm locked out too");
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
        assert!(!state.open_confirm(0), "nothing marked: refuses to open");
        assert!(state.take_confirmed_marks().is_none(), "nothing to confirm");

        state.toggle_mark().unwrap();
        assert!(state.open_confirm(2));
        assert_eq!(state.confirm().unwrap().hardlink_files, 2);

        // Esc / any non-y key: cancel, marks intact.
        state.cancel_confirm();
        assert!(state.confirm().is_none());
        assert_eq!(state.marks().len(), 1, "cancel keeps the marks");

        // Reopen and confirm: marks handed over and cleared.
        assert!(state.open_confirm(0));
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
}
