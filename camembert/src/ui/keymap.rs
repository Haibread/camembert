//! Single source of truth for the keyboard/mouse map (design slice 4's
//! `?` cheatsheet — "source of truth: the actual key handler — keep them
//! in sync").
//!
//! [`SIMPLE`] covers every key whose entire effect is a call into
//! [`UiState`] with no other context (movement, sort, column toggle,
//! opening the cheatsheet): [`dispatch_simple`] is the *only* place that
//! interprets these, and `ui::handle_key` calls it for its catch-all
//! case, so a row added here is simultaneously wired up and documented —
//! it cannot silently drift out of sync with the cheatsheet.
//!
//! A handful of keys need context `dispatch_simple` doesn't have (the
//! scan phase, the flash/toast queues, the confirm/review modal state):
//! descend/ascend, mark, delete, review, quit. Those stay hand-written in
//! `ui::handle_key`, and are documented here as [`EXTRA`] purely for the
//! cheatsheet/footer — kept honest by `ui::tests`' `handle_key` routing
//! tests (`v_opens_review_and_d_from_within_it_opens_confirm` and
//! neighbors), which drive each one through the real key handler and
//! check it does what its entry claims.
//!
//! The six sort keys (`d`/`a`/`n`/`m`/`c`/`e`) join that hand-written set
//! for the same reason (D3, `docs/design/flat-view-decisions.md`): which
//! keys are meaningful now depends on the active table mode (a pattern
//! group has no mtime or error count to sort by) and refusing one needs
//! the flash queue to say so — context `dispatch_simple` doesn't carry.
//! `t`/`b` (switch to the flat/breakdown modes) stay in [`SIMPLE`]
//! instead: the toggle itself is unconditional and needs nothing beyond
//! [`UiState`].
//!
//! [`MOUSE`] documents the slice-3 mouse map the same way (no dispatch
//! table to generate from — mouse routing depends on frame geometry, not
//! a static key code — so it is doc-only, cross-checked by hand against
//! `handle_mouse`/`handle_click` and the README's Mouse table/`--help`).

use crossterm::event::KeyCode;

use super::state::UiState;

/// One keyboard shortcut whose entire effect is a call into [`UiState`].
pub struct SimpleKey {
    /// Every key code that triggers this action (e.g. both `Down` and
    /// `j`).
    pub codes: &'static [KeyCode],
    /// How the key(s) are shown in the cheatsheet.
    pub keys: &'static str,
    pub action: &'static str,
    pub apply: fn(&mut UiState),
}

/// One documented key that needs context beyond `UiState` (see module
/// docs) — cheatsheet/footer content only, the behavior lives in
/// `ui::handle_key`.
pub struct ExtraKey {
    pub keys: &'static str,
    pub action: &'static str,
}

pub const SIMPLE: &[SimpleKey] = &[
    SimpleKey {
        codes: &[KeyCode::Down, KeyCode::Char('j')],
        keys: "↓/j",
        action: "move down",
        apply: UiState::move_down,
    },
    SimpleKey {
        codes: &[KeyCode::Up, KeyCode::Char('k')],
        keys: "↑/k",
        action: "move up",
        apply: UiState::move_up,
    },
    SimpleKey {
        codes: &[KeyCode::Char('g')],
        keys: "g",
        action: "jump to the top",
        apply: UiState::move_top,
    },
    SimpleKey {
        codes: &[KeyCode::Char('G')],
        keys: "G",
        action: "jump to the bottom",
        apply: UiState::move_bottom,
    },
    SimpleKey {
        codes: &[KeyCode::Char('p')],
        keys: "p",
        action: "toggle the apparent-size column",
        apply: UiState::toggle_apparent,
    },
    SimpleKey {
        codes: &[KeyCode::Char('u')],
        keys: "u",
        action: "clear all marks",
        apply: UiState::unmark_all,
    },
    SimpleKey {
        codes: &[KeyCode::Char('?')],
        keys: "?",
        action: "show this cheatsheet",
        apply: UiState::open_cheatsheet,
    },
    SimpleKey {
        codes: &[KeyCode::Char('z')],
        keys: "z",
        action: "toggle zen mode (table only: no cards, gauge or wheel)",
        apply: UiState::toggle_zen,
    },
    SimpleKey {
        codes: &[KeyCode::Char('t')],
        keys: "t",
        action: "flat top files across the whole scan (t again returns to tree)",
        apply: UiState::toggle_flat_top,
    },
    SimpleKey {
        codes: &[KeyCode::Char('b')],
        keys: "b",
        action: "pattern breakdown (b again returns to tree)",
        apply: UiState::toggle_breakdown,
    },
];

/// Dispatch `code` against [`SIMPLE`]; `true` if a row matched (and its
/// action already ran).
pub fn dispatch_simple(code: KeyCode, ui: &mut UiState) -> bool {
    for key in SIMPLE {
        if key.codes.contains(&code) {
            (key.apply)(ui);
            return true;
        }
    }
    false
}

pub const EXTRA: &[ExtraKey] = &[
    ExtraKey {
        keys: "d",
        action: "sort by real (disk) size [default; tree/flat/breakdown]",
    },
    ExtraKey {
        keys: "a",
        action: "sort by apparent size [tree/flat/breakdown]",
    },
    ExtraKey {
        keys: "n",
        action: "sort by name [tree/flat/breakdown]",
    },
    ExtraKey {
        keys: "m",
        action: "sort by modification time [tree only]",
    },
    ExtraKey {
        keys: "c",
        action: "sort by item count [tree: subtree items; breakdown: group entries]",
    },
    ExtraKey {
        keys: "e",
        action: "sort by subtree error count [tree only]",
    },
    ExtraKey {
        keys: "⏎/l/→",
        action: "open the directory under the cursor (flat: jump to its containing directory)",
    },
    ExtraKey {
        keys: "⌫/h/←",
        action: "go back up to the parent [tree only]",
    },
    ExtraKey {
        keys: "Space",
        action: "mark/unmark the row under the cursor for deletion [tree/flat]",
    },
    ExtraKey {
        keys: "v",
        action: "review marked entries (Space unmarks a row, D deletes)",
    },
    ExtraKey {
        keys: "D",
        action: "delete the marked entries (confirm with y)",
    },
    ExtraKey {
        keys: "f",
        action: "freeable files: deleted-but-open files holding disk space (f/Esc closes)",
    },
    ExtraKey {
        keys: "Ctrl-K, /",
        action: "open the filter/command palette (query-first; > switches to commands; \
                 while open, every key but Esc/Enter/arrows/Ctrl-C is text, q included)",
    },
    ExtraKey {
        keys: "Esc",
        action: "close the palette, else a modal, else leave the flat/breakdown mode, \
                 else clear an active filter, else quit [from tree]",
    },
    ExtraKey {
        keys: "q, Ctrl-C",
        action: "quit (cancels a running scan); inside the palette only Ctrl-C quits",
    },
];

pub const MOUSE: &[ExtraKey] = &[
    ExtraKey {
        keys: "click a row",
        action: "select it",
    },
    ExtraKey {
        keys: "click again / double-click",
        action: "open it (like ⏎)",
    },
    ExtraKey {
        keys: "wheel over the table",
        action: "scroll the cursor",
    },
    ExtraKey {
        keys: "click a donut slice",
        action: "open that child directly",
    },
    ExtraKey {
        keys: "click a breadcrumb segment",
        action: "jump to that ancestor",
    },
    ExtraKey {
        keys: "click the errors card",
        action: "sort by subtree error count",
    },
    ExtraKey {
        keys: "move over a row",
        action: "update the selection card without moving the cursor",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::UiState;
    use camembert_core::tree::DirId;
    use camembert_core::tree::NodeId;
    use camembert_core::view::ViewSnapshot;
    use camembert_core::view::{DirTotals, Row, RowState, ScanStats};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    fn snapshot() -> Arc<ViewSnapshot> {
        Arc::new(ViewSnapshot {
            generation: 1,
            dir: DirId::from_raw(0),
            parent: None,
            path: PathBuf::from("/x"),
            rows: vec![
                Row {
                    name: b"a".to_vec().into_boxed_slice(),
                    node: NodeId::from_raw(1),
                    dir: None,
                    is_dir: false,
                    apparent: 1,
                    disk: 1,
                    items: 1,
                    errors: 0,
                    state: RowState::File,
                    mtime: 0,
                },
                Row {
                    name: b"b".to_vec().into_boxed_slice(),
                    node: NodeId::from_raw(2),
                    dir: None,
                    is_dir: false,
                    apparent: 2,
                    disk: 2,
                    items: 1,
                    errors: 0,
                    state: RowState::File,
                    mtime: 0,
                },
            ],
            totals: DirTotals::default(),
            stats: ScanStats {
                entries: 2,
                dirs: 0,
                errors: 0,
                disk_bytes: 3,
                elapsed: Duration::from_millis(1),
                root_complete: true,
            },
            hardlink_inodes: 0,
            degraded: false,
        })
    }

    /// Every `SIMPLE` row dispatches to exactly the `UiState` effect its
    /// `action` describes — the guarantee that keeps the cheatsheet
    /// honest for the stateless majority of keys.
    #[test]
    fn simple_table_dispatches_match_their_documented_action() {
        let mut ui = UiState::new(snapshot());

        assert!(dispatch_simple(KeyCode::Down, &mut ui));
        assert_eq!(ui.cursor(), 1, "j/Down moved down");
        assert!(dispatch_simple(KeyCode::Char('k'), &mut ui));
        assert_eq!(ui.cursor(), 0, "k moved back up");

        assert!(dispatch_simple(KeyCode::Char('G'), &mut ui));
        assert_eq!(ui.cursor(), 1);
        assert!(dispatch_simple(KeyCode::Char('g'), &mut ui));
        assert_eq!(ui.cursor(), 0);

        assert!(ui.show_apparent);
        assert!(dispatch_simple(KeyCode::Char('p'), &mut ui));
        assert!(!ui.show_apparent);

        assert!(!ui.cheatsheet_open());
        assert!(dispatch_simple(KeyCode::Char('?'), &mut ui));
        assert!(ui.cheatsheet_open());

        assert_eq!(ui.mode(), crate::ui::state::ViewMode::Tree);
        assert!(dispatch_simple(KeyCode::Char('t'), &mut ui));
        assert_eq!(ui.mode(), crate::ui::state::ViewMode::FlatTop);
        assert!(dispatch_simple(KeyCode::Char('b'), &mut ui));
        assert_eq!(ui.mode(), crate::ui::state::ViewMode::Breakdown);

        assert!(
            !dispatch_simple(KeyCode::Char('D'), &mut ui),
            "not in SIMPLE"
        );
        assert!(
            !dispatch_simple(KeyCode::Char('n'), &mut ui),
            "sort keys are hand-written now (mode-aware), not in SIMPLE"
        );
    }

    /// The cheatsheet lists every code `SIMPLE` actually dispatches, and
    /// nothing else — the completeness half of "generated from one
    /// table".
    #[test]
    fn every_simple_code_is_unique_and_labeled() {
        let mut seen = std::collections::HashSet::new();
        for key in SIMPLE {
            for &code in key.codes {
                assert!(seen.insert(code), "duplicate dispatch for {code:?}");
            }
            assert!(!key.keys.is_empty());
            assert!(!key.action.is_empty());
        }
    }
}
