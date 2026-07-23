//! The Ctrl-K / `/` command-and-filter palette (D6,
//! `docs/design/query-decisions.md`; amendments
//! `docs/design/query-attack-a.md` finding 2 (q-typability), finding 3
//! (`<`/`>` collision — bare size sugar stays, so command mode uses `>` as
//! the *first character* of the buffer, never conflicting with a `>100M`
//! size term which can only ever appear after at least one other
//! character or as the sole content of a *query*-mode buffer — see
//! [`PaletteState::mode`]'s doc), `docs/design/query-attack-c.md`
//! (the `/` graft: identical widget, no second surface, no split
//! histories).
//!
//! One widget serves two entry points: Ctrl-K opens it query-first; `/`
//! opens the exact same [`PaletteState`], pre-scoped to filter mode by
//! virtue of already defaulting to it (there is no separate "scope" flag
//! to drift out of sync — attack C's rejected split is exactly the
//! bug this sameness prevents). Typed text with no leading `>` is a filter
//! query, parsed live with [`camembert_core::query::parse`] on every
//! edit (cheap — tokenizing, not folding) so parse errors render inline
//! immediately; a leading `>` switches to fuzzy command mode.
//!
//! Deliberately clock- and terminal-free (no `Instant`, no
//! crossterm/ratatui types) — same split as [`super::state`] and
//! [`super::freeable_panel`]: `ui.rs` owns the debounce timer, the
//! background fold's channel, and history/saved-query I/O, mirroring the
//! freeable sweep's spawn+channel idiom and the toast queue's `_at`
//! testability pattern.

use camembert_core::query::{Parsed, parse};

use super::keymap;
use super::state::UiState;

/// Which half of the palette is showing — derived live from the buffer's
/// first character, never stored redundantly (so it can never drift from
/// what is actually typed): a leading `>` is command mode, anything else
/// (including empty) is query mode. `>100M` a user types as a *filter*
/// term never collides with this: a bare `>` alone, with nothing typed
/// yet, is exactly the moment command mode starts, and from then on every
/// character (including a digit) is command-search text, not a query —
/// there is no ambiguity because the two modes are never live at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Query,
    Command,
}

/// One command the palette can fuzzy-match and run: every
/// [`keymap::SIMPLE`] entry (dispatched exactly like the real keypress)
/// plus a handful of hand-written "mode/panel" commands that need context
/// beyond a bare `&mut UiState` (mirroring why those same actions are
/// hand-written in `ui::handle_key` rather than living in `SIMPLE`).
#[derive(Clone, Copy)]
pub struct PaletteCommand {
    pub label: &'static str,
    /// Key(s) that already do this, shown as a hint.
    pub hint: &'static str,
    pub action: CommandAction,
}

/// What running a matched command does. `Simple` calls straight into
/// `UiState` like `keymap::dispatch_simple`; the rest need the caller's
/// extra context (`Phase`, the flash queue, `no_proc_sweep`) and are
/// executed by `ui.rs` alongside its existing hand-written key handlers
/// for the same actions.
#[derive(Clone, Copy)]
pub enum CommandAction {
    Simple(fn(&mut UiState)),
    ReviewMarks,
    DeleteMarked,
    FreeablePanel,
    ClearFilter,
    Quit,
}

/// Every command the palette offers in command mode: one per
/// [`keymap::SIMPLE`] row (documented, dispatchable, and — because it's
/// the exact same table the `?` cheatsheet renders from — can never list a
/// command that doesn't actually do what it says), plus the mode/panel
/// actions `SIMPLE` can't carry (see [`CommandAction`]'s doc). Movement,
/// per-row marking and sorting are deliberately *not* commands here: they
/// need a row/context under the cursor a name-only palette entry doesn't
/// have, exactly like they need hand-written context in `handle_key`
/// instead of living in `SIMPLE`/`EXTRA`.
pub fn all_commands() -> Vec<PaletteCommand> {
    let mut commands: Vec<PaletteCommand> = keymap::SIMPLE
        .iter()
        .map(|key| PaletteCommand {
            label: key.action,
            hint: key.keys,
            action: CommandAction::Simple(key.apply),
        })
        .collect();
    commands.extend([
        PaletteCommand {
            label: "review marked entries",
            hint: "v",
            action: CommandAction::ReviewMarks,
        },
        PaletteCommand {
            label: "delete marked entries",
            hint: "D",
            action: CommandAction::DeleteMarked,
        },
        PaletteCommand {
            label: "freeable files panel",
            hint: "f",
            action: CommandAction::FreeablePanel,
        },
        PaletteCommand {
            label: "clear active filter",
            hint: "Esc",
            action: CommandAction::ClearFilter,
        },
        PaletteCommand {
            label: "quit",
            hint: "q",
            action: CommandAction::Quit,
        },
    ]);
    commands
}

/// Subsequence fuzzy score, ASCII-case-insensitive: every character of
/// `needle` must appear in `haystack` in order (not necessarily
/// contiguous). Higher is better; `None` when `needle` cannot be found as
/// a subsequence at all. An empty needle matches everything (the palette
/// with just `>` typed shows every command, unfiltered).
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    let haystack: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut hi = 0usize;
    let mut score = 0i32;
    let mut last_match: Option<usize> = None;
    for &nc in &needle {
        let mut found = None;
        while hi < haystack.len() {
            if haystack[hi] == nc {
                found = Some(hi);
                break;
            }
            hi += 1;
        }
        let idx = found?;
        score += 10;
        if last_match == Some(idx.wrapping_sub(1)) {
            score += 5; // contiguous-run bonus
        }
        if idx == 0 {
            score += 3; // matches right at the start
        }
        last_match = Some(idx);
        hi = idx + 1;
    }
    Some(score)
}

/// Indices into `commands` whose label fuzzy-matches `needle`, best match
/// first (ties keep `commands`' own order — stable, so the list doesn't
/// jitter as you type).
pub fn filter_commands(needle: &str, commands: &[PaletteCommand]) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = commands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| fuzzy_score(needle, c.label).map(|score| (i, score)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// The palette's live state: the text buffer (edited as chars, so
/// backspace/left/right are correct on multi-byte names), the mode it
/// implies, and the one selection index shared by whichever transient
/// list is showing (saved queries on an empty query-mode buffer, or
/// fuzzy-matched commands in command mode — the two are mutually
/// exclusive, so one field suffices, same idea as [`super::state::UiState`]
/// reusing one cursor across tree/flat/breakdown row counts).
#[derive(Debug, Clone)]
pub struct PaletteState {
    buffer: Vec<char>,
    cursor: usize,
    list_selected: usize,
    /// Position in the history list currently loaded into the buffer via
    /// Up/Down (query mode only); `None` when the buffer holds fresh,
    /// un-historied text. Any edit detaches (cleared), so typing while
    /// browsing history starts a new entry rather than mutating the old
    /// one in place.
    history_cursor: Option<usize>,
    /// The in-progress buffer stashed the moment Up first walks into
    /// history, restored verbatim when Down walks back past the newest
    /// entry.
    draft: Option<String>,
}

impl PaletteState {
    /// Open with an empty buffer (Ctrl-K/`/` with no active filter).
    pub fn new() -> Self {
        Self::with_text("")
    }

    /// Open pre-filled (Ctrl-K/`/` while a filter is already active: the
    /// query stays visible and editable rather than vanishing into an
    /// empty box behind its own effect).
    pub fn with_text(text: &str) -> Self {
        let buffer: Vec<char> = text.chars().collect();
        let cursor = buffer.len();
        Self {
            buffer,
            cursor,
            list_selected: 0,
            history_cursor: None,
            draft: None,
        }
    }

    pub fn mode(&self) -> PaletteMode {
        if self.buffer.first() == Some(&'>') {
            PaletteMode::Command
        } else {
            PaletteMode::Query
        }
    }

    pub fn text(&self) -> String {
        self.buffer.iter().collect()
    }

    /// The buffer's content after the mode sigil: the whole buffer in
    /// query mode (there is no sigil to strip), or everything after the
    /// leading `>` in command mode.
    pub fn content(&self) -> String {
        match self.mode() {
            PaletteMode::Query => self.text(),
            PaletteMode::Command => self.buffer[1..].iter().collect(),
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Live parse of the query-mode content. Cheap (tokenizing a
    /// keystroke-length string, not folding) — recomputed on every call so
    /// there is no cached copy to invalidate; called once per frame for
    /// the inline error echo and once per frame for the debounce/apply
    /// check, both trivial.
    pub fn parsed(&self) -> Parsed {
        parse(&self.content())
    }

    fn detach_from_history(&mut self) {
        self.history_cursor = None;
        self.draft = None;
        self.list_selected = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += 1;
        self.detach_from_history();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buffer.remove(self.cursor);
            self.detach_from_history();
        }
    }

    pub fn delete_forward(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
            self.detach_from_history();
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buffer.len());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    /// Replace the whole buffer (a history/saved-query load), cursor at
    /// the end and the list selection reset. Does *not* touch
    /// `history_cursor`/`draft` itself — [`Self::history_up`]/
    /// [`Self::history_down`] manage those explicitly around their own
    /// calls to this.
    pub fn set_text(&mut self, text: &str) {
        self.buffer = text.chars().collect();
        self.cursor = self.buffer.len();
        self.list_selected = 0;
    }

    // ---- selection index (saved queries / command list) ----

    pub fn list_selected(&self) -> usize {
        self.list_selected
    }

    pub fn move_selection_down(&mut self, len: usize) {
        if len > 0 && self.list_selected + 1 < len {
            self.list_selected += 1;
        }
    }

    pub fn move_selection_up(&mut self) {
        self.list_selected = self.list_selected.saturating_sub(1);
    }

    // ---- history (query mode only) ----

    /// Walk one entry further into the past (Up). `history` is newest-last
    /// (append order); the first press stashes the in-progress buffer as
    /// the draft to restore once the user walks back past the newest
    /// entry.
    pub fn history_up(&mut self, history: &[String]) {
        if history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => {
                self.draft = Some(self.text());
                history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        self.set_text(&history[next]);
    }

    /// Walk one entry back toward the present (Down); past the newest
    /// entry, restores the stashed draft and detaches from history.
    pub fn history_down(&mut self, history: &[String]) {
        let Some(i) = self.history_cursor else {
            return;
        };
        if i + 1 < history.len() {
            self.history_cursor = Some(i + 1);
            self.set_text(&history[i + 1]);
        } else {
            let draft = self.draft.take().unwrap_or_default();
            self.set_text(&draft);
            self.history_cursor = None;
        }
    }
}

impl Default for PaletteState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_derived_from_a_leading_sigil() {
        let p = PaletteState::new();
        assert_eq!(p.mode(), PaletteMode::Query);
        let mut p = PaletteState::with_text(">flat");
        assert_eq!(p.mode(), PaletteMode::Command);
        assert_eq!(p.content(), "flat");
        // ">flat" is 5 characters; 4 backspaces remove "flat", leaving just
        // the sigil itself.
        p.backspace();
        p.backspace();
        p.backspace();
        p.backspace();
        assert_eq!(p.text(), ">");
        assert_eq!(p.mode(), PaletteMode::Command, "still command mode");
        assert_eq!(p.content(), "", "empty content past the sigil");
    }

    #[test]
    fn size_sugar_never_collides_with_command_mode() {
        // A filter term starting with `>` immediately followed by a digit
        // is size sugar in *query* mode; command mode only ever starts
        // when `>` is the buffer's very first character, so typing
        // `>100M` from empty starts command mode (as intended: `>` alone
        // is the sigil) but a filter like `size >100M` (space first) never
        // does, because the leading character is not `>`.
        let p = PaletteState::with_text("*.log >100M");
        assert_eq!(p.mode(), PaletteMode::Query);
        let parsed = p.parsed();
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.query.terms().len(), 2);
    }

    #[test]
    fn insert_backspace_and_cursor_movement() {
        let mut p = PaletteState::new();
        p.insert_char('a');
        p.insert_char('b');
        p.insert_char('c');
        assert_eq!(p.text(), "abc");
        assert_eq!(p.cursor(), 3);
        p.move_left();
        p.insert_char('X');
        assert_eq!(p.text(), "abXc");
        p.move_home();
        p.delete_forward();
        assert_eq!(p.text(), "bXc");
        p.move_end();
        p.backspace();
        assert_eq!(p.text(), "bX");
    }

    #[test]
    fn editing_detaches_from_a_loaded_history_entry() {
        let history = vec!["old query".to_owned(), "newer query".to_owned()];
        let mut p = PaletteState::new();
        p.insert_char('x');
        p.history_up(&history);
        assert_eq!(p.text(), "newer query");
        p.insert_char('!');
        // Editing detaches: a further Up starts fresh from history's end
        // again rather than continuing from where the detached edit left
        // off, and the in-progress ("newer query!") text is not clobbered
        // by drifting internal state.
        p.history_up(&history);
        assert_eq!(p.text(), "newer query");
    }

    #[test]
    fn history_up_then_down_restores_the_draft() {
        let history = vec!["alpha".to_owned(), "beta".to_owned()];
        let mut p = PaletteState::new();
        p.insert_char('d');
        p.insert_char('r');
        p.insert_char('a');
        p.insert_char('f');
        p.insert_char('t');
        assert_eq!(p.text(), "draft");

        p.history_up(&history);
        assert_eq!(p.text(), "beta", "walked to the newest entry first");
        p.history_up(&history);
        assert_eq!(p.text(), "alpha", "walked further back");
        p.history_up(&history);
        assert_eq!(p.text(), "alpha", "clamped at the oldest entry");

        p.history_down(&history);
        assert_eq!(p.text(), "beta");
        p.history_down(&history);
        assert_eq!(p.text(), "draft", "past the newest entry: draft restored");
    }

    #[test]
    fn fuzzy_score_matches_in_order_only() {
        assert!(fuzzy_score("tb", "toggle breakdown").is_some());
        assert!(
            fuzzy_score("bt", "toggle breakdown").is_none(),
            "out of order"
        );
        assert!(
            fuzzy_score("", "anything").is_some(),
            "empty needle matches all"
        );
        assert!(fuzzy_score("zzz", "toggle breakdown").is_none());
    }

    #[test]
    fn fuzzy_score_prefers_contiguous_and_prefix_matches() {
        let contiguous = fuzzy_score("top", "top files").unwrap();
        // Same three letters, in order, but scattered across the label
        // and not starting at position 0.
        let scattered = fuzzy_score("top", "the old prompt").unwrap();
        assert!(
            contiguous > scattered,
            "a contiguous prefix match should outscore a scattered one"
        );
    }

    #[test]
    fn filter_commands_orders_by_score_then_stable_position() {
        let commands = vec![
            PaletteCommand {
                label: "toggle zen mode",
                hint: "z",
                action: CommandAction::Simple(UiState::toggle_zen),
            },
            PaletteCommand {
                label: "top files",
                hint: "t",
                action: CommandAction::Simple(UiState::toggle_flat_top),
            },
            PaletteCommand {
                label: "toggle breakdown",
                hint: "b",
                action: CommandAction::Simple(UiState::toggle_breakdown),
            },
        ];
        let order = filter_commands("top", &commands);
        assert_eq!(commands[order[0]].label, "top files");
    }

    #[test]
    fn all_commands_includes_every_simple_row_and_the_extras() {
        let commands = all_commands();
        assert_eq!(commands.len(), keymap::SIMPLE.len() + 5);
        assert!(commands.iter().any(|c| c.label == "quit"));
        assert!(commands.iter().any(|c| c.label == "clear active filter"));
    }
}
