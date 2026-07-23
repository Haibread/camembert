//! Pure logic for freeable phase 1's UI (docs/design/freeable-decisions.md
//! D5/D6): the `f` panel's display grouping, the sweep-completion toast's
//! threshold, and the pre-deletion open-file advisory's aggregation and
//! text. No terminal types, no `/proc` access — every rule here is
//! unit-testable with synthetic data; `ui.rs` wires it to the ratatui
//! rendering and to `camembert_core::freeable`'s live sweep/index.

use camembert_core::freeable::{Coverage, DeletedEntry, Holder};

// ---------------------------------------------------------------------------
// D5: panel grouping — deepest still-existing ancestor, longest-prefix match
// ---------------------------------------------------------------------------

/// One display group in the freeable panel: entries whose evidence path
/// falls under `ancestor`, or `None` for the "(outside the scan / unknown)"
/// catch-all when no candidate ancestor matches (D5). Display-only — never
/// changes the ledger's own data, just how it's nested on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeableGroup {
    pub ancestor: Option<Vec<u8>>,
    /// Indices into the entries slice this was built from — largest first,
    /// inherited unchanged from the input order (D5: entries stay
    /// "largest first"; grouping is a display nesting, not a re-sort).
    pub entries: Vec<usize>,
}

/// Group `entries` (assumed already sorted largest-first, as
/// [`camembert_core::freeable::Ledger::root_fs_entries`] provides) under the
/// deepest still-existing ancestor directory in `ancestors` — a plain
/// longest-byte-prefix match of the evidence path against each candidate
/// (D5: "keep it simple"; unreadable/unmatchable entries land in the
/// `None` catch-all). Groups come back in first-seen order, so whichever
/// group contains the single largest entry is always first; entries within
/// a group keep their incoming order.
pub fn group_by_ancestor(entries: &[DeletedEntry], ancestors: &[Vec<u8>]) -> Vec<FreeableGroup> {
    let mut groups: Vec<FreeableGroup> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let key = longest_prefix_ancestor(&entry.evidence, ancestors).map(<[u8]>::to_vec);
        match groups.iter_mut().find(|group| group.ancestor == key) {
            Some(group) => group.entries.push(index),
            None => groups.push(FreeableGroup {
                ancestor: key,
                entries: vec![index],
            }),
        }
    }
    groups
}

/// The deepest (longest) member of `ancestors` that is a genuine path
/// prefix of `evidence` — the boundary must land on a `/` (or consume all
/// of `evidence`), so `/home/a` never matches `/home/ab/x`.
fn longest_prefix_ancestor<'a>(evidence: &[u8], ancestors: &'a [Vec<u8>]) -> Option<&'a [u8]> {
    ancestors
        .iter()
        .map(Vec::as_slice)
        .filter(|ancestor| is_path_prefix(evidence, ancestor))
        .max_by_key(|ancestor| ancestor.len())
}

/// Whether `evidence` lies at or under the directory `ancestor` — a path
/// prefix, not just a byte prefix: the boundary must land on a `/` (or
/// `evidence` equals `ancestor` exactly), so `/a/b` never matches `/a/bb`.
/// `pub(crate)` (not just module-private): the D6 pre-deletion advisory's
/// marked-*directory* containment check in `ui.rs` reuses this exact rule
/// rather than re-deriving it — one path-boundary implementation for both
/// the panel's ancestor grouping and the confirm modal's warning.
pub(crate) fn is_path_prefix(evidence: &[u8], ancestor: &[u8]) -> bool {
    !ancestor.is_empty()
        && evidence.starts_with(ancestor)
        && (evidence.len() == ancestor.len() || evidence.get(ancestor.len()) == Some(&b'/'))
}

/// "pid (comm)", or just the bare pid when the process name could not be
/// read (or was empty) — the display form shared by the panel's holder
/// list and the pre-deletion warning's "top holders" (D5/D6).
pub fn format_holder(pid: u32, comm: &Option<String>) -> String {
    match comm {
        Some(name) if !name.is_empty() => format!("{pid} ({name})"),
        _ => pid.to_string(),
    }
}

// ---------------------------------------------------------------------------
// D5: sweep-completion toast threshold
// ---------------------------------------------------------------------------

/// Minimum root-fs freeable total, in bytes, for the sweep-completion toast
/// (D5).
pub const TOAST_MIN_BYTES: u64 = 100 * 1024 * 1024;
/// Minimum fraction of filesystem capacity for the toast (D5).
pub const TOAST_MIN_FRACTION: f64 = 0.01;

/// Both bounds must hold (D5): small disks aren't nagged about crumbs, big
/// arrays aren't nagged about rounding noise. A zero-capacity filesystem
/// (statvfs failed, or a degenerate mount) never divides by zero and never
/// toasts.
pub fn should_toast(root_fs_freeable_bytes: u64, fs_capacity_bytes: u64) -> bool {
    if root_fs_freeable_bytes < TOAST_MIN_BYTES || fs_capacity_bytes == 0 {
        return false;
    }
    (root_fs_freeable_bytes as f64 / fs_capacity_bytes as f64) >= TOAST_MIN_FRACTION
}

// ---------------------------------------------------------------------------
// D6: pre-deletion open-file advisory
// ---------------------------------------------------------------------------

/// Top holders named in the advisory line before the rest go uncounted by
/// name (the `holder_count` total still reflects everyone).
const MAX_TOP_HOLDERS: usize = 3;

/// The pre-deletion advisory (D6): how many marked *files* are directly
/// open, how many *more* open files sit *inside* marked directories (the
/// D6 amendment's containment check — the primary real-world case: a
/// marked data directory whose contents are still held open by another
/// process), by how many distinct processes total, with the busiest few
/// named. Advisory only — the caller never lets this block confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWarning {
    /// Marked *file* entries whose own `(dev, ino)` is currently open
    /// (direct match).
    pub entries_open: usize,
    /// Open files found strictly *under* a marked *directory* (path-prefix
    /// containment) — distinct from `entries_open`, which only covers
    /// marked files themselves. A marked directory's own inode being open
    /// (e.g. some shell's cwd) does not count here; this is about files
    /// inside it.
    pub contained_open: usize,
    /// Distinct holder processes across *both* kinds above, deduplicated
    /// by pid.
    pub holder_count: usize,
    /// Busiest holders first (most matched entries/files held open, ties
    /// broken by pid for determinism), capped to [`MAX_TOP_HOLDERS`].
    pub top_holders: Vec<(u32, Option<String>)>,
    /// Set when the index's sweep saw a partial process table (D6/D7):
    /// `(readable, seen)`, so the same caveat the panel shows can repeat
    /// here — an absent warning must never be mistaken for a clean bill of
    /// health on a multi-user machine (attack A serious finding). This same
    /// caveat covers the containment check's own blind spot too: a holder
    /// in a different mount namespace can have an evidence path that
    /// doesn't textually match the marked directory even though the file
    /// is the same one (bind mounts, chroots, containers) — an admitted
    /// false-negative, not something this warning can detect and flag on
    /// its own, so it rides on the same honesty line rather than inventing
    /// a separate (and falsely precise) caveat for it.
    pub partial_coverage: Option<(u32, u32)>,
}

/// Build the advisory from two independent match kinds, decoupled from
/// `OpenFileIndex` itself (its fields are private core-crate internals by
/// design, D8) so this aggregation is unit-testable with synthetic data:
///
/// - `marked_file_lookups`: one entry per marked *file*, `Some(holders)`
///   when its own `(dev, ino)` is presently open, `None` otherwise (D6's
///   original direct match).
/// - `contained_holders`: one entry per open file found strictly *under* a
///   marked *directory* (D6 amendment's path-prefix containment) — each
///   element is that file's holders.
///
/// `None` when neither kind found anything — the caller shows no line at
/// all. Holders are deduplicated by pid across *both* kinds before ranking
/// (the same process can hold a marked file open directly and something
/// else open inside a marked directory; it should count once).
pub fn build_open_warning(
    marked_file_lookups: &[Option<Vec<Holder>>],
    contained_holders: &[Vec<Holder>],
    coverage: Coverage,
) -> Option<OpenWarning> {
    let entries_open = marked_file_lookups
        .iter()
        .filter(|lookup| lookup.is_some())
        .count();
    let contained_open = contained_holders.len();
    if entries_open == 0 && contained_open == 0 {
        return None;
    }
    let mut by_pid: std::collections::BTreeMap<u32, (Option<String>, usize)> =
        std::collections::BTreeMap::new();
    let all_holder_lists = marked_file_lookups
        .iter()
        .flatten()
        .chain(contained_holders);
    for holders in all_holder_lists {
        for holder in holders {
            let slot = by_pid
                .entry(holder.pid)
                .or_insert_with(|| (holder.comm.clone(), 0));
            slot.1 += 1;
        }
    }
    let holder_count = by_pid.len();
    let mut ranked: Vec<(u32, Option<String>, usize)> = by_pid
        .into_iter()
        .map(|(pid, (comm, count))| (pid, comm, count))
        .collect();
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    let top_holders = ranked
        .into_iter()
        .take(MAX_TOP_HOLDERS)
        .map(|(pid, comm, _)| (pid, comm))
        .collect();
    Some(OpenWarning {
        entries_open,
        contained_open,
        holder_count,
        top_holders,
        partial_coverage: coverage
            .is_partial()
            .then_some((coverage.readable, coverage.seen)),
    })
}

/// Assemble the advisory line's text (D6): how many marked files are open
/// and/or how many open files sit inside marked directories, by how many
/// processes total, the busiest named, and — when the sweep's coverage was
/// partial — the same caveat the panel carries.
pub fn warning_text(warning: &OpenWarning) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if warning.entries_open > 0 {
        let noun = if warning.entries_open == 1 {
            "marked entry"
        } else {
            "marked entries"
        };
        clauses.push(format!("{} {noun}", warning.entries_open));
    }
    if warning.contained_open > 0 {
        let noun = if warning.contained_open == 1 {
            "file"
        } else {
            "files"
        };
        clauses.push(format!(
            "{} {noun} inside marked directories",
            warning.contained_open
        ));
    }
    let subject = clauses.join(" and ");
    let verb = if warning.entries_open + warning.contained_open == 1 {
        "is"
    } else {
        "are"
    };
    let process_word = if warning.holder_count == 1 {
        "process"
    } else {
        "processes"
    };
    let mut text = format!(
        "{subject} {verb} open in {} {process_word}",
        warning.holder_count
    );
    if !warning.top_holders.is_empty() {
        let names: Vec<String> = warning
            .top_holders
            .iter()
            .map(|(pid, comm)| format_holder(*pid, comm))
            .collect();
        text.push_str(&format!(" (top holders: {})", names.join(", ")));
    }
    if let Some((readable, seen)) = warning.partial_coverage {
        text.push_str(&format!(
            " (open-file check saw {readable} of {seen} processes)"
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(evidence: &[u8], bytes: u64) -> DeletedEntry {
        DeletedEntry {
            dev: 1,
            ino: 1,
            bytes,
            evidence: evidence.to_vec(),
            holders: Vec::new(),
        }
    }

    fn holder(pid: u32, comm: Option<&str>) -> Holder {
        Holder {
            pid,
            comm: comm.map(str::to_owned),
        }
    }

    // ---- group_by_ancestor ----

    #[test]
    fn groups_by_deepest_existing_ancestor_longest_prefix_wins() {
        let entries = vec![
            entry(b"/home/user/proj/build/big.o (deleted)", 300),
            entry(b"/home/user/proj/small.log (deleted)", 100),
            entry(b"/var/log/other.log (deleted)", 50),
        ];
        let ancestors = vec![b"/home/user".to_vec(), b"/home/user/proj".to_vec()];
        let groups = group_by_ancestor(&entries, &ancestors);
        assert_eq!(
            groups.len(),
            2,
            "one matched group, one unmatched catch-all"
        );
        assert_eq!(
            groups[0].ancestor,
            Some(b"/home/user/proj".to_vec()),
            "deepest (longest) candidate wins over the shallower /home/user"
        );
        assert_eq!(groups[0].entries, vec![0, 1]);
        assert_eq!(
            groups[1].ancestor, None,
            "unmatched entry: the catch-all group"
        );
        assert_eq!(groups[1].entries, vec![2]);
    }

    #[test]
    fn ancestor_prefix_respects_path_boundaries_not_just_bytes() {
        let entries = vec![entry(b"/home/ab/x (deleted)", 10)];
        let ancestors = vec![b"/home/a".to_vec()];
        let groups = group_by_ancestor(&entries, &ancestors);
        assert_eq!(
            groups[0].ancestor, None,
            "byte-prefix only, not a real path component prefix: no match"
        );
    }

    /// The exact boundary-collision case flagged by review: a byte prefix
    /// that is *not* a path prefix must never match, in either direction
    /// this rule is consumed (panel grouping here, D6 containment in
    /// `ui.rs`).
    #[test]
    fn is_path_prefix_rejects_byte_prefix_that_is_not_a_path_boundary() {
        assert!(
            !is_path_prefix(b"/a/bb", b"/a/b"),
            "/a/b must not match /a/bb"
        );
        assert!(
            is_path_prefix(b"/a/b/c", b"/a/b"),
            "/a/b/c is genuinely under /a/b"
        );
        assert!(is_path_prefix(b"/a/b", b"/a/b"), "exact match counts");
        assert!(
            !is_path_prefix(b"/a/b", b""),
            "an empty ancestor matches nothing"
        );
    }

    #[test]
    fn exact_ancestor_match_and_group_order_follows_first_seen_largest_entry() {
        let entries = vec![
            entry(b"/a/big (deleted)", 1000),
            entry(b"/b/small (deleted)", 1),
        ];
        let ancestors = vec![b"/a".to_vec(), b"/b".to_vec()];
        let groups = group_by_ancestor(&entries, &ancestors);
        assert_eq!(
            groups[0].ancestor,
            Some(b"/a".to_vec()),
            "group containing the largest (first-seen) entry sorts first"
        );
        assert_eq!(groups[1].ancestor, Some(b"/b".to_vec()));
    }

    #[test]
    fn no_ancestors_puts_everything_in_the_catch_all() {
        let entries = vec![entry(b"/anything (deleted)", 5)];
        let groups = group_by_ancestor(&entries, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].ancestor, None);
    }

    // ---- format_holder ----

    #[test]
    fn format_holder_falls_back_to_bare_pid() {
        assert_eq!(format_holder(42, &Some("bash".to_owned())), "42 (bash)");
        assert_eq!(format_holder(42, &None), "42");
        assert_eq!(
            format_holder(42, &Some(String::new())),
            "42",
            "empty comm treated like missing"
        );
    }

    // ---- should_toast ----

    #[test]
    fn toast_requires_both_bounds() {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;
        assert!(!should_toast(50 * MIB, GIB), "under the 100 MiB floor");
        assert!(
            !should_toast(150 * MIB, 1000 * GIB),
            "over the floor but under 1% of a huge filesystem"
        );
        assert!(should_toast(150 * MIB, 10 * GIB), "over both bounds");
        assert!(!should_toast(0, 10 * GIB), "nothing freeable");
        assert!(
            !should_toast(200 * MIB, 0),
            "degenerate zero-capacity filesystem never divides by zero"
        );
    }

    // ---- build_open_warning / warning_text ----

    #[test]
    fn no_warning_when_nothing_matched_either_kind() {
        let lookups = vec![None, None];
        assert!(
            build_open_warning(
                &lookups,
                &[],
                Coverage {
                    seen: 5,
                    readable: 5
                }
            )
            .is_none()
        );
    }

    #[test]
    fn warning_aggregates_distinct_holders_ranked_by_entries_held() {
        let lookups = vec![
            Some(vec![holder(10, Some("nginx"))]),
            Some(vec![holder(10, Some("nginx")), holder(20, Some("sidecar"))]),
            None,
        ];
        let warning = build_open_warning(
            &lookups,
            &[],
            Coverage {
                seen: 5,
                readable: 5,
            },
        )
        .expect("two entries open");
        assert_eq!(warning.entries_open, 2);
        assert_eq!(warning.contained_open, 0);
        assert_eq!(warning.holder_count, 2);
        assert_eq!(
            warning.top_holders[0].0, 10,
            "pid 10 holds more marked entries open, ranks first"
        );
        assert!(
            warning.partial_coverage.is_none(),
            "full coverage: no caveat"
        );
    }

    /// D6 amendment: a marked *directory* with no direct `(dev, ino)` match
    /// of its own still produces a warning when files are found open
    /// underneath it (the containment channel), and holders are
    /// deduplicated across it and the direct-file channel.
    #[test]
    fn containment_matches_alone_produce_a_warning_and_dedupe_holders_with_direct_matches() {
        let marked_file_lookups = vec![Some(vec![holder(10, Some("nginx"))]), None];
        let contained_holders = vec![
            vec![holder(10, Some("nginx"))], // same pid as a direct match
            vec![holder(30, Some("backup"))],
        ];
        let warning = build_open_warning(
            &marked_file_lookups,
            &contained_holders,
            Coverage {
                seen: 4,
                readable: 4,
            },
        )
        .expect("both channels contributed");
        assert_eq!(warning.entries_open, 1, "one marked file directly open");
        assert_eq!(
            warning.contained_open, 2,
            "two open files found inside marked directories"
        );
        assert_eq!(
            warning.holder_count, 2,
            "pid 10 counted once despite appearing in both channels"
        );
        assert_eq!(
            warning.top_holders[0].0, 10,
            "pid 10 touches two matches (direct + contained), ranks first"
        );
    }

    /// Containment alone (no marked file directly open) still warns — the
    /// scenario review flagged as the primary real-world case: marking a
    /// directory whose *contents* are held open.
    #[test]
    fn containment_only_still_warns_with_no_marked_files_open() {
        let marked_file_lookups: Vec<Option<Vec<Holder>>> = vec![None, None];
        let contained_holders = vec![vec![holder(99, Some("postgres"))]];
        let warning = build_open_warning(
            &marked_file_lookups,
            &contained_holders,
            Coverage {
                seen: 1,
                readable: 1,
            },
        )
        .expect("containment alone is enough to warn");
        assert_eq!(warning.entries_open, 0);
        assert_eq!(warning.contained_open, 1);
        assert_eq!(
            warning_text(&warning),
            "1 file inside marked directories is open in 1 process (top holders: 99 (postgres))"
        );
    }

    #[test]
    fn warning_carries_the_partial_coverage_caveat() {
        let lookups = vec![Some(vec![holder(1, None)])];
        let warning = build_open_warning(
            &lookups,
            &[],
            Coverage {
                seen: 10,
                readable: 4,
            },
        )
        .expect("one entry open");
        assert_eq!(warning.partial_coverage, Some((4, 10)));
        let text = warning_text(&warning);
        assert!(
            text.contains("open-file check saw 4 of 10 processes"),
            "text: {text}"
        );
    }

    #[test]
    fn warning_text_pluralizes_and_lists_top_holders() {
        let warning = OpenWarning {
            entries_open: 3,
            contained_open: 0,
            holder_count: 2,
            top_holders: vec![(10, Some("nginx".to_owned())), (20, None)],
            partial_coverage: None,
        };
        assert_eq!(
            warning_text(&warning),
            "3 marked entries are open in 2 processes (top holders: 10 (nginx), 20)"
        );
    }

    #[test]
    fn warning_text_singular_entry_and_process() {
        let warning = OpenWarning {
            entries_open: 1,
            contained_open: 0,
            holder_count: 1,
            top_holders: vec![(7, None)],
            partial_coverage: None,
        };
        assert_eq!(
            warning_text(&warning),
            "1 marked entry is open in 1 process (top holders: 7)"
        );
    }

    /// Both channels contribute non-zero counts: the text joins both
    /// clauses and pluralizes the shared verb from their combined total.
    #[test]
    fn warning_text_combines_both_channels() {
        let warning = OpenWarning {
            entries_open: 1,
            contained_open: 2,
            holder_count: 2,
            top_holders: vec![(10, Some("nginx".to_owned())), (30, None)],
            partial_coverage: None,
        };
        assert_eq!(
            warning_text(&warning),
            "1 marked entry and 2 files inside marked directories are open in 2 processes \
             (top holders: 10 (nginx), 30)"
        );
    }
}
