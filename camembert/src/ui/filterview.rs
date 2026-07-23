//! Pure composition helpers for what an active filter's [`FilterResult`]
//! makes the pill say (D4/D6, `docs/design/query-decisions.md`;
//! amendments `docs/design/query-attack-a.md` findings 7 (the residual
//! must be shown, not just documented) and 12 (the "Esc clears" scope)).
//!
//! The other half of D4 composition — which tree rows survive an active
//! filter (attack finding 10: a directory is kept only when its filtered
//! subtree still has a match; a leaf only when it is itself matched,
//! hardlink extras included per attack finding 1) — lives directly in
//! [`super::state::UiState::ensure_sorted`], because that decision has to
//! share exactly one row-index space with the cursor, mouse hit-testing
//! and mark toggling that already live there; duplicating it into a
//! second, index-space-agnostic function here would only invite the two
//! to drift. `ui.rs`'s `draw_table` renders the resulting (already
//! filtered) rows with their directory totals swapped for
//! [`FilterResult::dir_total`] and a hardlink-extra badge, right where the
//! rest of the table's per-row formatting already lives.
//!
//! Same split as [`super::flatview`]/[`super::wheel`] otherwise:
//! terminal-free, unit-testable, `ui.rs` only draws what this computes.

use camembert_core::query::FilterResult;
use camembert_core::size::HumanSize;

/// The filter pill's one line of text: query, matched entries + bytes,
/// the dir-inode residual explanation when nonzero (attack finding 7 —
/// shown continuously, not just documented once), a hardlink-extra count
/// when nonzero, and the Esc hint (whose scope attack finding 12 pins
/// down precisely: clearing the filter, not closing a modal — see
/// `ui.rs`'s Esc ladder).
pub fn pill_text(query_text: &str, result: &FilterResult, show_apparent: bool) -> String {
    let bytes = if show_apparent {
        result.matched_apparent
    } else {
        result.matched_disk
    };
    let mut text = format!(
        "/ {query_text}  ·  {} matched, {}",
        result.matched_entries,
        HumanSize(bytes)
    );
    let residual_bytes = if show_apparent {
        result.residual.apparent
    } else {
        result.residual.disk
    };
    if let Some(line) = residual_line(residual_bytes, result.residual.dirs) {
        text.push_str("  ·  ");
        text.push_str(&line);
    }
    if result.matched_extra_links > 0 {
        text.push_str(&format!(
            "  ·  {} \u{26d3} hardlink row(s) counted at their canonical path",
            result.matched_extra_links
        ));
    }
    text.push_str("  ·  Esc clears");
    text
}

/// The dir-inode residual explanation (attack finding 7): `None` when
/// there is nothing to explain (an empty tree, or a hypothetical filter
/// engine result with no scanned directories at all) — extracted as a
/// pure function so the "only shown when nonzero" rule is testable
/// without needing a real [`FilterResult`] (most of its fields are
/// private to `camembert-core`, by design; only [`apply`] can build one).
fn residual_line(residual_bytes: u64, dirs: u64) -> Option<String> {
    if residual_bytes == 0 {
        return None;
    }
    Some(format!(
        "+{} in {dirs} directory inode(s) not counted",
        HumanSize(residual_bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camembert_core::query::{ApplyOptions, HardlinkIndex, apply, parse};
    use camembert_core::scan::{ScanOptions, Scanner};

    fn scan(path: &std::path::Path) -> camembert_core::scan::ScanOutcome {
        let mut outcome = Scanner::new(ScanOptions {
            statx_engine: Default::default(),
            threads: 1,
            cross_filesystems: false,
        })
        .scan(path)
        .expect("scan");
        outcome.finalize_hardlinks();
        outcome
    }

    #[test]
    fn residual_line_only_appears_when_nonzero() {
        assert_eq!(residual_line(0, 0), None);
        let line = residual_line(4096, 2).expect("nonzero residual explains itself");
        assert!(line.contains("2 directory inode"));
    }

    #[test]
    fn pill_text_over_a_real_scan_matches_and_reports_the_residual() {
        // A directory's *apparent* size (st_size) is essentially always
        // nonzero on a real filesystem (unlike st_blocks/disk usage, which
        // some filesystems — notably tmpfs — can legitimately report as 0
        // for a small directory), so this checks the residual line in
        // apparent-size mode: portable across whatever filesystem the test
        // happens to run on, while still exercising the pill's full
        // composition (query text, matched count/bytes, the residual
        // line, the Esc hint) against genuine `apply()` output.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.log"), b"hello").unwrap();
        let outcome = scan(tmp.path());
        let parsed = parse("*.log");
        let hardlinks = HardlinkIndex::build(&outcome, 0);
        let result = apply(
            outcome.tree(),
            &parsed.query,
            &camembert_core::flat::PatternSet::default(),
            &hardlinks,
            &ApplyOptions {
                cap: 100,
                epoch: 0,
                now_unix: 0,
                threads: 1,
            },
        );
        let text = pill_text("*.log", &result, true);
        assert!(text.contains("/ *.log"));
        assert!(text.contains("1 matched"));
        assert!(text.contains("directory inode"));
        assert!(text.ends_with("Esc clears"));
    }
}
