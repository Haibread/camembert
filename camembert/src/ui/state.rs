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

    /// Rows in the active sort order.
    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        self.order.iter().map(|&i| &self.snapshot.rows[i])
    }

    /// The row under the cursor, if any.
    pub fn selected(&self) -> Option<&Row> {
        self.order.get(self.cursor).map(|&i| &self.snapshot.rows[i])
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
    }

    /// Handle a sort keypress; re-sorts immediately.
    pub fn press_sort(&mut self, key: SortKey) {
        self.sort.press(key);
        self.ensure_sorted();
        self.clamp_cursor();
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.order.len() {
            self.cursor += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_top(&mut self) {
        self.cursor = 0;
    }

    pub fn move_bottom(&mut self) {
        self.cursor = self.order.len().saturating_sub(1);
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
    snapshot.hardlinks_seen && !snapshot.stats.root_complete
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
            hardlinks_seen: false,
            degraded: false,
        })
    }

    fn names(state: &UiState) -> Vec<&[u8]> {
        state.rows().map(|r| &*r.name).collect()
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
    fn footer_notes_follow_the_scan_state() {
        let mut snap = ViewSnapshot {
            generation: 1,
            dir: DirId::from_raw(0),
            parent: None,
            path: PathBuf::from("/x"),
            rows: Vec::new(),
            totals: DirTotals::default(),
            stats: stats(false),
            hardlinks_seen: true,
            degraded: true,
        };
        assert!(show_hardlink_note(&snap), "hardlinks + scanning: shown");
        assert!(show_updating_note(&snap), "degraded + scanning: shown");

        snap.stats.root_complete = true;
        assert!(!show_hardlink_note(&snap), "corrected at scan end (D3)");
        assert!(!show_updating_note(&snap));

        snap.stats.root_complete = false;
        snap.hardlinks_seen = false;
        snap.degraded = false;
        assert!(!show_hardlink_note(&snap));
        assert!(!show_updating_note(&snap));
    }
}
