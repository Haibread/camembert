//! Pure interactive-mode state: sorting, cursor, navigation stack, and
//! snapshot application. No terminal types anywhere — everything here is
//! unit-testable with synthetic snapshots.

use std::sync::Arc;

use camembert_core::tree::DirId;
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
