//! Pure data shaping for the `t` (flat top files) and `b` (pattern
//! breakdown) table modes (D3, `docs/design/flat-view-decisions.md`):
//! turning a [`FlatSummary`] into sorted display rows, independent of the
//! terminal — `ui.rs` only draws what this module computes, the same
//! split as [`super::wheel`] and [`super::state`].
//!
//! # The path gap during a live scan
//!
//! [`camembert_core::flat::TopFile`] now carries a denormalized, lossily
//! decoded basename ([`TopFile::name`]) alongside its `NodeId`, disk size
//! and hardlink flag — cloned only when an entry is actually kept in the
//! top-N heap, so it costs nothing on the hot per-node path. That closes
//! the name half of the original gap: [`flat_rows`] uses it directly,
//! live or post-scan alike, with no `outcome` needed. The **path** half
//! remains post-scan only: a full path requires walking the arena's
//! parent chain, and *during* a live scan the arena lives exclusively on
//! the owner thread (the whole point of the
//! [`camembert_core::view::ViewBus`] design — the UI thread never shares
//! it), so [`FlatRow::path`] (and `apparent`, which `TopFile` never
//! carries at all) stay `None` until `outcome` is `Some`. The table shows
//! the basename alone mid-scan and the full abbreviated path once it
//! completes — never a placeholder standing in for real data.

use std::path::PathBuf;

use camembert_core::flat::{FlatSummary, PatternKind, TopFile};
use camembert_core::scan::ScanOutcome;
use camembert_core::tree::NodeId;

use super::state::{SortKey, SortSpec};

/// One display row of the flat top-files table.
#[derive(Debug, Clone)]
pub struct FlatRow {
    pub node: NodeId,
    /// Lossily-decoded basename (display/sort only), carried straight
    /// from [`TopFile::name`] — available live or post-scan alike (see
    /// the module doc).
    pub name: Box<str>,
    /// Disk (real) bytes — the ranking key `fold`/the accumulator already
    /// sorted by.
    pub disk: u64,
    /// Apparent bytes, when resolvable (post-scan only — `TopFile` itself
    /// carries no apparent size, see the module doc).
    pub apparent: Option<u64>,
    pub hardlink: bool,
    /// Full path, when resolvable (post-scan only — see the module doc).
    pub path: Option<PathBuf>,
}

/// Build one display row per [`FlatSummary::top_files`] entry. `outcome`
/// is `Some` post-scan (the frozen arena is directly readable) and `None`
/// mid-scan (provisional summary, arena not shareable — see the module
/// doc): every row carries its name/disk/hardlink either way; only
/// `apparent`/`path` degrade to `None` without the arena.
pub fn flat_rows(summary: &FlatSummary, outcome: Option<&ScanOutcome>) -> Vec<FlatRow> {
    summary
        .top_files
        .iter()
        .map(|f| enrich(f, outcome))
        .collect()
}

fn enrich(f: &TopFile, outcome: Option<&ScanOutcome>) -> FlatRow {
    let Some(outcome) = outcome else {
        return FlatRow {
            node: f.node,
            name: f.name.clone(),
            disk: f.disk,
            apparent: None,
            hardlink: f.hardlink,
            path: None,
        };
    };
    let size = outcome.node(f.node).size();
    FlatRow {
        node: f.node,
        name: f.name.clone(),
        disk: f.disk,
        apparent: Some(size.apparent),
        hardlink: f.hardlink,
        path: Some(outcome.tree().path_of_node(f.node)),
    }
}

/// Sort keys the flat top-files table can honor: disk (its natural
/// order), apparent size, and name — all meaningful once resolvable.
/// `Items`/`Mtime`/`Errors` describe subtree concepts a single file has
/// none of, so they are refused (the caller flashes "not available in
/// this view", D3).
pub fn flat_supports_sort(key: SortKey) -> bool {
    matches!(key, SortKey::Disk | SortKey::Apparent | SortKey::Name)
}

/// Display order for [`flat_rows`]' output under `sort`. Stable tiebreak
/// by node id ascending, matching [`camembert_core::flat::fold`]'s own
/// deterministic tiebreak so the order never looks arbitrary at the
/// cutoff.
pub fn sort_flat_rows(rows: &[FlatRow], sort: SortSpec) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| {
        let (ra, rb) = (&rows[a], &rows[b]);
        let primary = match sort.key {
            SortKey::Apparent => ra.apparent.unwrap_or(0).cmp(&rb.apparent.unwrap_or(0)),
            SortKey::Name => ra.name.cmp(&rb.name),
            _ => ra.disk.cmp(&rb.disk),
        };
        let primary = if sort.descending {
            primary.reverse()
        } else {
            primary
        };
        primary.then_with(|| ra.node.index().cmp(&rb.node.index()))
    });
    order
}

/// One display row of the pattern-breakdown table: a named group, or the
/// trailing uncategorized ("rest") row — `kind` is `None` for it, there is
/// no single glob kind to show.
#[derive(Debug, Clone)]
pub struct BreakdownRow {
    pub label: String,
    pub kind: Option<PatternKind>,
    pub disk: u64,
    pub apparent: u64,
    pub entries: u64,
}

/// Label of the trailing uncategorized row (D1: "rest" — everything
/// matched by no pattern).
pub const UNCATEGORIZED_LABEL: &str = "(uncategorized)";

/// Build one row per pattern group (in [`FlatSummary::groups`] order) plus
/// a trailing uncategorized row for [`FlatSummary::rest`] — always
/// present, even at zero, so the table's row count is stable and the
/// donut's implicit "everything not in any group" fraction has a named
/// counterpart in the list (D1's disjoint-partition invariant means this
/// is always exactly `total - sum(groups)`, never an overlap artifact).
pub fn breakdown_rows(summary: &FlatSummary) -> Vec<BreakdownRow> {
    let mut rows: Vec<BreakdownRow> = summary
        .groups
        .iter()
        .map(|g| BreakdownRow {
            label: g.label.clone(),
            kind: Some(g.kind),
            disk: g.disk,
            apparent: g.apparent,
            entries: g.entries,
        })
        .collect();
    rows.push(BreakdownRow {
        label: UNCATEGORIZED_LABEL.to_owned(),
        kind: None,
        disk: summary.rest.disk,
        apparent: summary.rest.apparent,
        entries: summary.rest.entries,
    });
    rows
}

/// Sort keys the breakdown table can honor: disk/apparent (the group
/// total), name (the label) and item count (the group's entry count,
/// mapped from the `c` key). `Mtime`/`Errors` describe per-entry concepts
/// a group total has none of (attack finding 10): refused, same as flat
/// mode.
pub fn breakdown_supports_sort(key: SortKey) -> bool {
    matches!(
        key,
        SortKey::Disk | SortKey::Apparent | SortKey::Name | SortKey::Items
    )
}

/// Display order for [`breakdown_rows`]' output under `sort`. Stable
/// tiebreak by label ascending.
pub fn sort_breakdown_rows(rows: &[BreakdownRow], sort: SortSpec) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| {
        let (ra, rb) = (&rows[a], &rows[b]);
        let primary = match sort.key {
            SortKey::Apparent => ra.apparent.cmp(&rb.apparent),
            SortKey::Name => ra.label.cmp(&rb.label),
            SortKey::Items => ra.entries.cmp(&rb.entries),
            _ => ra.disk.cmp(&rb.disk),
        };
        let primary = if sort.descending {
            primary.reverse()
        } else {
            primary
        };
        primary.then_with(|| ra.label.cmp(&rb.label))
    });
    order
}

/// Percentage of the breakdown's own total (Σ groups + rest — the root
/// subtree aggregate, D1's invariant) a row accounts for. `0.0` on an
/// empty scan rather than dividing by zero.
pub fn breakdown_percent(row: &BreakdownRow, total_disk: u64) -> f64 {
    if total_disk == 0 {
        0.0
    } else {
        100.0 * row.disk as f64 / total_disk as f64
    }
}

/// Σ groups + rest disk bytes — the breakdown's own total (D1: equal to
/// the root subtree aggregate by construction, asserted in
/// `camembert_core::flat::fold`'s debug invariant).
pub fn breakdown_total_disk(summary: &FlatSummary) -> u64 {
    summary.groups.iter().map(|g| g.disk).sum::<u64>() + summary.rest.disk
}

#[cfg(test)]
mod tests {
    use super::*;
    use camembert_core::flat::{GroupTotal, RestTotal};

    fn sample_summary() -> FlatSummary {
        FlatSummary {
            groups: vec![
                GroupTotal {
                    label: "node_modules".to_owned(),
                    kind: PatternKind::Dir,
                    apparent: 3000,
                    disk: 3000,
                    entries: 5,
                },
                GroupTotal {
                    label: "*.log".to_owned(),
                    kind: PatternKind::File,
                    apparent: 500,
                    disk: 500,
                    entries: 2,
                },
            ],
            rest: RestTotal {
                apparent: 1500,
                disk: 1500,
                entries: 3,
            },
            // Names deliberately diverge from disk order (node 1 is
            // smaller but alphabetically first) so a name-sort test can't
            // pass by accident of matching the disk order.
            top_files: vec![
                TopFile {
                    node: NodeId::from_raw(1),
                    name: "alpha.bin".into(),
                    disk: 1000,
                    hardlink: false,
                },
                TopFile {
                    node: NodeId::from_raw(2),
                    name: "bravo.log".into(),
                    disk: 2000,
                    hardlink: true,
                },
            ],
            truncated: false,
            provisional: false,
            epoch: 0,
        }
    }

    #[test]
    fn flat_rows_without_outcome_carry_the_name_but_not_the_path() {
        let rows = flat_rows(&sample_summary(), None);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(row.path.is_none(), "no path without the frozen arena");
            assert!(row.apparent.is_none(), "no apparent size on TopFile itself");
        }
        // The name is denormalized onto TopFile now, so it's available
        // live, unlike path/apparent (which need the frozen arena).
        assert_eq!(&*rows[0].name, "alpha.bin");
        assert_eq!(&*rows[1].name, "bravo.log");
        assert!(rows[1].hardlink);
    }

    #[test]
    fn sort_flat_rows_by_name_diverges_from_disk_order() {
        let rows = flat_rows(&sample_summary(), None);
        let ascending = sort_flat_rows(
            &rows,
            SortSpec {
                key: SortKey::Name,
                descending: false,
            },
        );
        // "alpha.bin" (node 1, smaller disk) sorts before "bravo.log"
        // (node 2, bigger disk) — the opposite of the default disk-desc
        // order, proving this exercises the name key and not disk.
        assert_eq!(ascending, vec![0, 1]);

        let descending = sort_flat_rows(
            &rows,
            SortSpec {
                key: SortKey::Name,
                descending: true,
            },
        );
        assert_eq!(descending, vec![1, 0]);
    }

    #[test]
    fn sort_flat_rows_by_disk_matches_fold_order() {
        let rows = flat_rows(&sample_summary(), None);
        let sort = SortSpec {
            key: SortKey::Disk,
            descending: true,
        };
        let order = sort_flat_rows(&rows, sort);
        assert_eq!(order, vec![1, 0], "2000 before 1000");
    }

    #[test]
    fn sort_flat_rows_ascending_toggle() {
        let rows = flat_rows(&sample_summary(), None);
        let sort = SortSpec {
            key: SortKey::Disk,
            descending: false,
        };
        assert_eq!(sort_flat_rows(&rows, sort), vec![0, 1]);
    }

    #[test]
    fn flat_supports_sort_matrix() {
        assert!(flat_supports_sort(SortKey::Disk));
        assert!(flat_supports_sort(SortKey::Apparent));
        assert!(flat_supports_sort(SortKey::Name));
        assert!(!flat_supports_sort(SortKey::Items));
        assert!(!flat_supports_sort(SortKey::Mtime));
        assert!(!flat_supports_sort(SortKey::Errors));
    }

    #[test]
    fn breakdown_rows_always_include_a_trailing_rest_row() {
        let rows = breakdown_rows(&sample_summary());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].label, UNCATEGORIZED_LABEL);
        assert!(rows[2].kind.is_none());
        assert_eq!(rows[2].disk, 1500);
    }

    #[test]
    fn breakdown_total_and_percent_match_the_disjoint_invariant() {
        let summary = sample_summary();
        let total = breakdown_total_disk(&summary);
        assert_eq!(total, 5000, "3000 + 500 + 1500");
        let rows = breakdown_rows(&summary);
        assert!((breakdown_percent(&rows[0], total) - 60.0).abs() < 1e-9);
        assert!((breakdown_percent(&rows[2], total) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn breakdown_percent_on_empty_scan_is_zero_not_nan() {
        let row = BreakdownRow {
            label: "x".to_owned(),
            kind: None,
            disk: 0,
            apparent: 0,
            entries: 0,
        };
        assert_eq!(breakdown_percent(&row, 0), 0.0);
    }

    #[test]
    fn sort_breakdown_rows_by_name_and_items() {
        let rows = breakdown_rows(&sample_summary());
        let by_name = sort_breakdown_rows(
            &rows,
            SortSpec {
                key: SortKey::Name,
                descending: false,
            },
        );
        let labels: Vec<&str> = by_name.iter().map(|&i| rows[i].label.as_str()).collect();
        // Plain byte-order ascending: '(' (0x28) sorts before '*' (0x2A).
        assert_eq!(labels, ["(uncategorized)", "*.log", "node_modules"]);

        let by_items = sort_breakdown_rows(
            &rows,
            SortSpec {
                key: SortKey::Items,
                descending: true,
            },
        );
        assert_eq!(rows[by_items[0]].entries, 5, "node_modules has most items");
    }

    #[test]
    fn breakdown_supports_sort_matrix() {
        assert!(breakdown_supports_sort(SortKey::Disk));
        assert!(breakdown_supports_sort(SortKey::Apparent));
        assert!(breakdown_supports_sort(SortKey::Name));
        assert!(breakdown_supports_sort(SortKey::Items));
        assert!(!breakdown_supports_sort(SortKey::Mtime));
        assert!(!breakdown_supports_sort(SortKey::Errors));
    }
}
