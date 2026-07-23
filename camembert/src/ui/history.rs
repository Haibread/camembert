//! Query history file (D6, `docs/design/query-decisions.md`; write-safety
//! amendments `docs/design/query-attack-a.md` finding 13):
//! `$XDG_STATE_HOME/camembert/history`, falling back to
//! `~/.local/state/camembert/history` when `XDG_STATE_HOME` is unset or
//! empty (the XDG base-directory spec's own fallback, same shape
//! `config.rs` already uses for `XDG_CONFIG_HOME`). This is the first
//! writable surface in a tool whose configuration
//! (`camembert.toml`) is deliberately read-only.
//!
//! Format: one query string per line, oldest first, bounded to
//! [`MAX_ENTRIES`] (the oldest are dropped once the bound is hit — never a
//! fatal error, this is convenience history, not an audit log). Writes are
//! atomic: the new content lands in a sibling `.part` file, `fsync`'d,
//! then renamed over the real path — a crash mid-write leaves the old
//! history intact, never a truncated one. Concurrent camembert instances
//! last-writer-wins on the rename (not merged) — acceptable for a
//! convenience file, not silently corrupting one.
//!
//! A missing or unreadable history file is silently treated as empty
//! (same "absent/broken = empty" convention `config.rs` uses); a write
//! failure (read-only `$HOME`, no space) is logged and otherwise ignored —
//! losing the ability to recall a past query is never worth interrupting
//! the session over.
//!
//! Privacy note (attack finding 13): entries are query text, which can
//! include filenames the user searched for, in plaintext, in a file with
//! default permissions. Worth exactly the one sentence the design doc
//! gives it; no encryption/redaction is attempted.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

/// Bound on stored entries (D6: "bounded ~200").
pub const MAX_ENTRIES: usize = 200;

/// `$XDG_STATE_HOME/camembert/history`, or
/// `~/.local/state/camembert/history` when `XDG_STATE_HOME` is unset or
/// empty. `None` when neither it nor `HOME` is available (same shape as
/// `config::config_path`) — treated exactly like a missing file.
pub fn history_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("camembert/history"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/camembert/history"))
}

/// Load the history file: one entry per (non-empty, trimmed) line, oldest
/// first. Never fails — a missing/unreadable file or one with more than
/// [`MAX_ENTRIES`] lines (an old file from a lower bound, or hand-edited)
/// degrades to "however many lines there are, keep only the newest
/// [`MAX_ENTRIES`]" rather than erroring.
pub fn load(path: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no history file yet");
            return Vec::new();
        }
        Err(err) => {
            warn!(path = %path.display(), %err, "cannot read history file: starting empty");
            return Vec::new();
        }
    };
    let mut entries: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if entries.len() > MAX_ENTRIES {
        entries.drain(0..entries.len() - MAX_ENTRIES);
    }
    entries
}

/// Append one query to the history file at `path`, bounding it to
/// [`MAX_ENTRIES`] (oldest dropped first) and skipping a would-be
/// immediate duplicate of the last entry (repeatedly re-applying the same
/// query while tweaking something else shouldn't spam identical lines).
/// Blank entries are never recorded. Writes atomically: full content to
/// `<path>.part` in the same directory (so the rename is same-filesystem,
/// never a cross-device copy), `fsync`'d, then renamed over `path`.
/// Creates the parent directory (`mkdir -p`) if it doesn't exist yet — the
/// first write this tool ever makes to disk outside of `--output`.
///
/// Never fatal: every failure is logged and swallowed — a history write
/// hiccup must never interrupt browsing.
pub fn append(path: &Path, entry: &str) {
    let entry = entry.trim();
    if entry.is_empty() {
        return;
    }
    let mut entries = load(path);
    if entries.last().map(String::as_str) == Some(entry) {
        return; // immediate duplicate: not worth a second line
    }
    entries.push(entry.to_owned());
    if entries.len() > MAX_ENTRIES {
        entries.drain(0..entries.len() - MAX_ENTRIES);
    }
    if let Err(err) = write_atomic(path, &entries) {
        warn!(path = %path.display(), %err, "cannot write history file: continuing without it");
    }
}

fn write_atomic(path: &Path, entries: &[String]) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other("history path has no parent"));
    };
    std::fs::create_dir_all(dir)?;
    let tmp_path = dir.join(format!(".history-{}.part", std::process::id()));
    let mut contents = String::new();
    for entry in entries {
        contents.push_str(entry);
        contents.push('\n');
    }
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_as_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nope/history");
        assert_eq!(load(&path), Vec::<String>::new());
    }

    #[test]
    fn append_then_load_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state/camembert/history");
        append(&path, "*.log");
        append(&path, "older:6mo");
        assert_eq!(
            load(&path),
            vec!["*.log".to_owned(), "older:6mo".to_owned()]
        );
    }

    #[test]
    fn append_creates_parent_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("a/b/c/history");
        append(&path, "node_modules/");
        assert!(path.exists());
        assert_eq!(load(&path), vec!["node_modules/".to_owned()]);
    }

    #[test]
    fn blank_entries_are_never_recorded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("history");
        append(&path, "");
        append(&path, "   ");
        assert_eq!(load(&path), Vec::<String>::new());
    }

    #[test]
    fn immediate_duplicates_are_not_appended_twice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("history");
        append(&path, "*.log");
        append(&path, "*.log");
        assert_eq!(load(&path), vec!["*.log".to_owned()]);
        // A different entry in between allows the repeat again.
        append(&path, "*.tmp");
        append(&path, "*.log");
        assert_eq!(
            load(&path),
            vec!["*.log".to_owned(), "*.tmp".to_owned(), "*.log".to_owned()]
        );
    }

    #[test]
    fn bounded_to_max_entries_dropping_the_oldest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("history");
        for i in 0..MAX_ENTRIES + 10 {
            append(&path, &format!("query-{i}"));
        }
        let entries = load(&path);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0], format!("query-{}", 10));
        assert_eq!(
            entries[MAX_ENTRIES - 1],
            format!("query-{}", MAX_ENTRIES + 9)
        );
    }

    #[test]
    fn write_is_atomic_no_part_file_left_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("history");
        append(&path, "*.log");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .part file left after a successful write"
        );
    }
}
