//! The freeable **confidence verdict** — one headline above the caveats.
//!
//! Both freeable phases are scrupulously honest about their own limits, and
//! by phase 2 that honesty had accumulated into nine simultaneous caveat
//! lines (process-table coverage, btrfs sibling subvolumes, mmap-only
//! holders, merged shared-outside buckets, an understating ambient floor,
//! RAM-backed inodes split out, compressed mounts, the mapping cap, the
//! kernel gate). Each line is individually correct and individually worth
//! keeping. In aggregate they read as "unreliable number, meticulously
//! annotated" rather than "trustworthy number": the user has to reconstruct
//! a verdict the code could have stated outright.
//!
//! This module states it. A [`Verdict`] is a **headline above** the
//! existing caveat lines, never a replacement for them: one graded word
//! plus a short reason naming at most [`MAX_LIMITERS`] dominant limiters.
//! It invents no measurement — every input is a value the sweep or the
//! oracle already produced.
//!
//! ## The ladder (three rungs, plus the ungraded absence)
//!
//! [`Confidence`] has three *graded* rungs, because three is exactly the
//! number of distinct actions a user can take with a figure:
//!
//! - [`Confidence::Measured`] — nothing this tool can measure is missing.
//!   Act on the number.
//! - [`Confidence::Partial`] — a named part is missing, but the majority
//!   was read. Act on the number, knowing which way it is wrong (always
//!   understating, except on a compressed mount — see below).
//! - [`Confidence::Fragmentary`] — **more than half** the inputs the figure
//!   is built from could not be read, or the figure may err in the
//!   forbidden (overstating) direction. What is shown is still real; it
//!   just does not bound anything useful yet. Close the gap first.
//!
//! Two rungs would be dishonest in the common case: it would have to
//! collapse the everyday unprivileged desktop sweep (which sees the user's
//! own processes — the ones holding the user's own files) into the same
//! word as a sysadmin's 3 %-coverage sweep on a multi-user server, and
//! whichever word were chosen would libel one of them. Four would claim a
//! resolution the signals do not have: the inputs are one ratio and a
//! handful of booleans, so a fourth rung's boundary could only be
//! arbitrary.
//!
//! [`Confidence::NoFigure`] is **not** a fourth rung — it is the absence of
//! anything to grade (the feature is switched off, the pass has not landed
//! yet, `/proc` was unreadable). Folding it into the bottom rung would
//! either dismiss a real sample as "nothing" or dress an absence up as a
//! weak figure, and dressing an absence up as a figure is exactly what the
//! freeable decisions forbid (`--no-fiemap` shows nothing rather than a
//! fallback, `docs/design/freeable2-decisions.md` D3).
//!
//! ## What is deliberately *not* a signal
//!
//! - **Unmeasurable gaps stay unconditional caveat lines.** btrfs sibling
//!   subvolumes outside the scan root and mmap-only holders (no fd, needs
//!   `CAP_SYS_ADMIN`) are invisible *by construction* — the code cannot
//!   tell a run where they matter from one where they do not, so they are
//!   constants, and a constant cannot discriminate between rungs. Grading
//!   on them would move every verdict down by the same amount, which is
//!   the same as not grading on them at all, minus the honesty.
//! - **Scope is not uncertainty.** Deleted-open bytes on other crossed
//!   filesystems and RAM-backed (memfd/shm) inodes are excluded from the
//!   root-filesystem headline on purpose (freeable D2/D3) and listed under
//!   their own labels. The headline is *exact for what it claims*; folding
//!   a scope decision into a confidence grade would make a correct figure
//!   look doubtful.
//! - **Upside uncertainty is not a limiter.** `shared_within` is a ceiling
//!   on bytes that might *additionally* be freed. The decision figure is
//!   the `exclusive` floor beneath it, so a fat ceiling never weakens the
//!   decision.

use crate::fiemap::OracleReport;
use crate::freeable::Coverage;
use crate::size::HumanSize;

/// Dominant limiters named in a reason string. Two, because the whole
/// point is to replace a list of nine with a headline; a third clause
/// starts rebuilding the list.
pub const MAX_LIMITERS: usize = 2;

/// How much of a freeable figure's inputs were actually readable — the
/// graded rungs plus the ungraded absence. See the module docs for why the
/// ladder has exactly these members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Nothing measurable is missing.
    Measured,
    /// Something named is missing; the majority was read.
    Partial,
    /// More than half the inputs were unreadable, or the figure may
    /// overstate. Real, but not yet a bound worth acting on.
    Fragmentary,
    /// Not a rung: there is no figure to grade at all.
    NoFigure,
}

impl Confidence {
    /// The word rendered to the user. Load-bearing product copy: it is the
    /// only carrier of the level on a monochrome terminal, so it must read
    /// correctly with no color at all. "fragmentary" describes *our view*,
    /// not the number — a fragmentary figure is still the only signal the
    /// user has, and the copy must never invite dismissing it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Partial => "partial",
            Self::Fragmentary => "fragmentary",
            Self::NoFigure => "no figure",
        }
    }
}

/// One rendered verdict: a graded [`Confidence`] and the short reason
/// naming its dominant limiters. Build it with [`Verdict::sweep`] (the
/// phase-1 deleted-open ledger) or [`Verdict::reclaim`] (the phase-2
/// selection oracle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    level: Confidence,
    reason: String,
}

impl Verdict {
    fn new(level: Confidence, reason: impl Into<String>) -> Self {
        Self {
            level,
            reason: reason.into(),
        }
    }

    pub fn level(&self) -> Confidence {
        self.level
    }

    /// The graded word — see [`Confidence::word`].
    pub fn word(&self) -> &'static str {
        self.level.word()
    }

    /// The short reason: the dominant limiters, or why the figure is
    /// trustworthy / absent. Never a list of every caveat — those keep
    /// their own lines below the verdict.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// `"<word> — <reason>"`. The TUI splits this into styled spans (the
    /// word carries the level's color *and* bold, never color alone); this
    /// is the plain-text form used by tests and any non-styled surface.
    pub fn line(&self) -> String {
        format!("{} — {}", self.word(), self.reason)
    }

    /// Verdict for the phase-1 `/proc` sweep's deleted-but-open ledger.
    ///
    /// `coverage` is [`crate::freeable::Ledger::coverage`] — `None` when no
    /// ledger exists yet. `disabled` is `--no-proc-sweep`/`NO_PROC_SWEEP`,
    /// which distinguishes "switched off" from "not finished yet".
    ///
    /// The rule, in order (`readable`/`seen` are the coverage counts):
    ///
    /// 1. no ledger → [`Confidence::NoFigure`] (off, or not landed yet);
    /// 2. `seen == 0` → [`Confidence::NoFigure`]: `/proc` itself was
    ///    unreadable, so the empty ledger means nothing (freeable D7's
    ///    silent degradation, said out loud);
    /// 3. `readable == seen` → [`Confidence::Measured`];
    /// 4. `readable * 2 <= seen` (**at most half** the process table was
    ///    readable) → [`Confidence::Fragmentary`];
    /// 5. otherwise → [`Confidence::Partial`].
    ///
    /// Coverage is the *only* signal here on purpose: it is the one thing
    /// about this sweep the code can actually measure (see the module docs
    /// on unmeasurable gaps and on scope).
    pub fn sweep(coverage: Option<Coverage>, disabled: bool) -> Self {
        let Some(coverage) = coverage else {
            return Self::new(
                Confidence::NoFigure,
                if disabled {
                    "the /proc sweep is off (--no-proc-sweep)"
                } else {
                    "the /proc sweep has not finished yet"
                },
            );
        };
        if coverage.seen == 0 {
            return Self::new(Confidence::NoFigure, "/proc could not be read");
        }
        if coverage.readable == coverage.seen {
            return Self::new(
                Confidence::Measured,
                format!("all {} processes readable", coverage.seen),
            );
        }
        let level = if u64::from(coverage.readable) * 2 <= u64::from(coverage.seen) {
            Confidence::Fragmentary
        } else {
            Confidence::Partial
        };
        Self::new(
            level,
            format!(
                "{} of {} processes readable",
                coverage.readable, coverage.seen
            ),
        )
    }

    /// Verdict for the phase-2 selection oracle's reclaim estimate.
    ///
    /// `facts` is `None` while the oracle is still mapping (a pending
    /// oracle is a confidence signal — "we do not know yet" — not an
    /// error); `disabled` is `--no-fiemap`/`NO_FIEMAP`.
    ///
    /// With a report in hand, let
    ///
    /// ```text
    /// unaccounted = unknown + zfs_bytes + truncated_disk
    /// total       = exclusive + shared_within + shared_outside + unaccounted
    /// ```
    ///
    /// (`shared_outside` is an exact "will not be freed" statement and
    /// `shared_within` an upside ceiling, so neither is unaccounted — see
    /// the module docs.) Then:
    ///
    /// 1. `old_kernel && report.mapped > 0` → [`Confidence::Fragmentary`]:
    ///    below 6.1 the `SHARED` bit can be falsely unset, so `exclusive`
    ///    may **overstate** — the one forbidden direction. Gated on
    ///    `mapped > 0` because a selection that never used FIEMAP (the
    ///    ext-family hardlink tier) is unaffected by the kernel's extent
    ///    bookkeeping.
    /// 2. `unaccounted * 2 > total` (more than half the selection's bytes
    ///    could not be estimated) → [`Confidence::Fragmentary`].
    /// 3. `unaccounted > 0 || compressed` → [`Confidence::Partial`]. A
    ///    `compress`-mounted device can never reach `Measured`: the figures
    ///    are allocated-**logical** bytes (freeable2 D2) and the physical
    ///    reclaim can be smaller by the whole compression ratio — again the
    ///    overstating direction, just a bounded and universal one rather
    ///    than a correctness bug, which is why it caps at `Partial` instead
    ///    of dropping to `Fragmentary` on every zstd-mounted machine.
    /// 4. otherwise → [`Confidence::Measured`] (an all-zero-byte selection
    ///    lands here: "frees nothing" is a complete answer).
    pub fn reclaim(facts: Option<&ReclaimFacts>, disabled: bool) -> Self {
        let Some(facts) = facts else {
            return Self::new(
                Confidence::NoFigure,
                if disabled {
                    "extent mapping is off (--no-fiemap)"
                } else {
                    "still mapping the selection"
                },
            );
        };
        let report = &facts.report;
        let unaccounted = report
            .unknown
            .saturating_add(report.zfs_bytes)
            .saturating_add(facts.truncated_disk);
        let total = report
            .exclusive
            .saturating_add(report.shared_within)
            .saturating_add(report.shared_outside)
            .saturating_add(unaccounted);

        let overstates = facts.old_kernel && report.mapped > 0;
        let mostly_unaccounted = total > 0 && unaccounted.saturating_mul(2) > total;
        let level = if overstates || mostly_unaccounted {
            Confidence::Fragmentary
        } else if unaccounted > 0 || facts.compressed {
            Confidence::Partial
        } else {
            Confidence::Measured
        };
        if level == Confidence::Measured {
            return Self::new(Confidence::Measured, "every marked file accounted for");
        }
        Self::new(level, reclaim_reason(facts, unaccounted, overstates))
    }
}

/// Everything the oracle already knows about one assembled selection —
/// the UI's `OracleView` restated as plain core data, so the rule can be
/// unit-tested without a terminal or a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimFacts {
    /// The oracle's bucketed answer.
    pub report: OracleReport,
    /// Any selected file sits on a `compress`-mounted device.
    pub compressed: bool,
    /// Kernel < 6.1: `FIEMAP_EXTENT_SHARED` is not trustworthy.
    pub old_kernel: bool,
    /// Files past the 50k-per-mark cap, or vanished before their stat.
    pub truncated_files: u64,
    /// Scan-time `disk` sum of those files.
    pub truncated_disk: u64,
}

/// The dominant limiters, worst first, capped at [`MAX_LIMITERS`].
///
/// Severity order is fixed and total, so the same facts always produce the
/// same string:
///
/// 1. the overstating kernel (wrong *direction* beats any quantity);
/// 2. the byte-quantified gaps — unestimated, ZFS, past-the-cap — ranked
///    by bytes at stake, ties broken in that listed order;
/// 3. the compressed mount (a qualifier on the whole figure, no bytes of
///    its own, and the caveat line below already spells it out).
fn reclaim_reason(facts: &ReclaimFacts, unaccounted: u64, overstates: bool) -> String {
    let mut limiters: Vec<String> = Vec::new();
    if overstates {
        limiters.push("kernel < 6.1 may overstate".to_owned());
    }

    let report = &facts.report;
    let mut quantified: Vec<(u64, usize, String)> = Vec::new();
    if report.unknown > 0 {
        quantified.push((
            report.unknown,
            0,
            format!("{} not estimated", HumanSize(report.unknown)),
        ));
    }
    if report.zfs_files > 0 {
        quantified.push((
            report.zfs_bytes,
            1,
            format!(
                "{} on ZFS, no figure possible",
                plural_files(report.zfs_files)
            ),
        ));
    }
    if facts.truncated_files > 0 {
        quantified.push((
            facts.truncated_disk,
            2,
            format!(
                "{} past the mapping cap",
                plural_files(facts.truncated_files)
            ),
        ));
    }
    quantified.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    limiters.extend(quantified.into_iter().map(|(_, _, text)| text));

    if facts.compressed {
        limiters.push("compressed mount".to_owned());
    }
    limiters.truncate(MAX_LIMITERS);
    if limiters.is_empty() {
        // Unreachable through `Verdict::reclaim` (a non-Measured level
        // always has at least one limiter), but a verdict with an empty
        // reason would be worse than a slightly generic one.
        return format!("{} not estimated", HumanSize(unaccounted));
    }
    limiters.join(", ")
}

/// "1 file" / "3 files" — the reason strings are prose, not debug output.
fn plural_files(count: u64) -> String {
    if count == 1 {
        "1 file".to_owned()
    } else {
        format!("{count} files")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn facts(report: OracleReport) -> ReclaimFacts {
        ReclaimFacts {
            report,
            compressed: false,
            old_kernel: false,
            truncated_files: 0,
            truncated_disk: 0,
        }
    }

    // ---- Verdict::sweep -------------------------------------------------

    #[test]
    fn sweep_without_a_ledger_distinguishes_off_from_not_yet() {
        let off = Verdict::sweep(None, true);
        assert_eq!(off.level(), Confidence::NoFigure);
        assert!(off.reason().contains("--no-proc-sweep"), "{off:?}");

        let waiting = Verdict::sweep(None, false);
        assert_eq!(waiting.level(), Confidence::NoFigure);
        assert!(waiting.reason().contains("not finished"), "{waiting:?}");
    }

    #[test]
    fn sweep_with_unreadable_proc_is_not_a_graded_zero() {
        // freeable D7: a missing /proc degrades to an empty ledger. That
        // emptiness must never read as "nothing is open".
        let verdict = Verdict::sweep(
            Some(Coverage {
                seen: 0,
                readable: 0,
            }),
            false,
        );
        assert_eq!(verdict.level(), Confidence::NoFigure);
        assert_eq!(verdict.line(), "no figure — /proc could not be read");
    }

    #[test]
    fn sweep_full_coverage_is_measured() {
        let verdict = Verdict::sweep(
            Some(Coverage {
                seen: 505,
                readable: 505,
            }),
            false,
        );
        assert_eq!(verdict.level(), Confidence::Measured);
        assert_eq!(verdict.line(), "measured — all 505 processes readable");
    }

    #[test]
    fn sweep_majority_readable_is_partial() {
        let verdict = Verdict::sweep(
            Some(Coverage {
                seen: 505,
                readable: 480,
            }),
            false,
        );
        assert_eq!(verdict.level(), Confidence::Partial);
        assert_eq!(verdict.line(), "partial — 480 of 505 processes readable");
    }

    /// The measured unprivileged-desktop case from the research digest:
    /// 140 of 505 processes readable.
    #[test]
    fn sweep_minority_readable_is_fragmentary() {
        let verdict = Verdict::sweep(
            Some(Coverage {
                seen: 505,
                readable: 140,
            }),
            false,
        );
        assert_eq!(verdict.level(), Confidence::Fragmentary);
        assert_eq!(
            verdict.line(),
            "fragmentary — 140 of 505 processes readable"
        );
    }

    /// The documented boundary: exactly half readable is *at most* half,
    /// so it falls on the fragmentary side; one more process flips it.
    #[test]
    fn sweep_half_readable_is_the_fragmentary_boundary() {
        let half = Verdict::sweep(
            Some(Coverage {
                seen: 10,
                readable: 5,
            }),
            false,
        );
        assert_eq!(half.level(), Confidence::Fragmentary);
        let one_more = Verdict::sweep(
            Some(Coverage {
                seen: 10,
                readable: 6,
            }),
            false,
        );
        assert_eq!(one_more.level(), Confidence::Partial);
    }

    // ---- Verdict::reclaim -----------------------------------------------

    #[test]
    fn reclaim_without_facts_distinguishes_off_from_pending() {
        let off = Verdict::reclaim(None, true);
        assert_eq!(off.level(), Confidence::NoFigure);
        assert_eq!(
            off.line(),
            "no figure — extent mapping is off (--no-fiemap)"
        );

        let pending = Verdict::reclaim(None, false);
        assert_eq!(pending.level(), Confidence::NoFigure);
        assert_eq!(pending.line(), "no figure — still mapping the selection");
    }

    #[test]
    fn reclaim_everything_bucketed_is_measured() {
        let verdict = Verdict::reclaim(
            Some(&facts(OracleReport {
                exclusive: 900 * MIB,
                shared_outside: 100 * MIB,
                inodes: 4,
                mapped: 4,
                ..OracleReport::default()
            })),
            false,
        );
        assert_eq!(verdict.level(), Confidence::Measured);
        assert_eq!(verdict.line(), "measured — every marked file accounted for");
    }

    /// An exact "will not be freed" statement and an upside ceiling are
    /// not limiters: a selection that is mostly shared is still fully
    /// *measured*.
    #[test]
    fn shared_buckets_alone_never_lower_the_level() {
        let verdict = Verdict::reclaim(
            Some(&facts(OracleReport {
                exclusive: 1,
                shared_within: 500 * MIB,
                shared_outside: 500 * MIB,
                hardlink_outside: 3,
                inodes: 9,
                mapped: 9,
                ..OracleReport::default()
            })),
            false,
        );
        assert_eq!(verdict.level(), Confidence::Measured);
    }

    #[test]
    fn reclaim_empty_selection_is_measured_not_absent() {
        let verdict = Verdict::reclaim(Some(&facts(OracleReport::default())), false);
        assert_eq!(verdict.level(), Confidence::Measured);
    }

    #[test]
    fn reclaim_minority_unknown_is_partial_and_names_the_bytes() {
        let verdict = Verdict::reclaim(
            Some(&facts(OracleReport {
                exclusive: 900 * MIB,
                unknown: 100 * MIB,
                inodes: 5,
                mapped: 4,
                ..OracleReport::default()
            })),
            false,
        );
        assert_eq!(verdict.level(), Confidence::Partial);
        assert_eq!(verdict.line(), "partial — 100.0 MiB not estimated");
    }

    #[test]
    fn reclaim_majority_unaccounted_is_fragmentary() {
        let verdict = Verdict::reclaim(
            Some(&facts(OracleReport {
                exclusive: 100 * MIB,
                unknown: 900 * MIB,
                inodes: 5,
                mapped: 1,
                ..OracleReport::default()
            })),
            false,
        );
        assert_eq!(verdict.level(), Confidence::Fragmentary);
        assert_eq!(verdict.line(), "fragmentary — 900.0 MiB not estimated");
    }

    /// The half boundary, from both sides.
    #[test]
    fn reclaim_exactly_half_unaccounted_is_still_partial() {
        let half = Verdict::reclaim(
            Some(&facts(OracleReport {
                exclusive: 500 * MIB,
                unknown: 500 * MIB,
                ..OracleReport::default()
            })),
            false,
        );
        assert_eq!(half.level(), Confidence::Partial);
        let over_half = Verdict::reclaim(
            Some(&facts(OracleReport {
                exclusive: 500 * MIB - 1,
                unknown: 500 * MIB + 1,
                ..OracleReport::default()
            })),
            false,
        );
        assert_eq!(over_half.level(), Confidence::Fragmentary);
    }

    /// A compressed mount can never read as fully measured (the figures
    /// are logical bytes), but it never drops a whole machine to the
    /// bottom rung either.
    #[test]
    fn compressed_mount_caps_at_partial() {
        let mut f = facts(OracleReport {
            exclusive: 1000 * MIB,
            inodes: 2,
            mapped: 2,
            ..OracleReport::default()
        });
        f.compressed = true;
        let verdict = Verdict::reclaim(Some(&f), false);
        assert_eq!(verdict.level(), Confidence::Partial);
        assert_eq!(verdict.line(), "partial — compressed mount");
    }

    /// Overstating beats every quantity: an otherwise-perfect report on a
    /// pre-6.1 kernel is fragmentary, and the kernel leads the reason.
    #[test]
    fn old_kernel_with_mapped_files_is_fragmentary_and_leads_the_reason() {
        let mut f = facts(OracleReport {
            exclusive: 1000 * MIB,
            inodes: 2,
            mapped: 2,
            ..OracleReport::default()
        });
        f.old_kernel = true;
        f.compressed = true;
        let verdict = Verdict::reclaim(Some(&f), false);
        assert_eq!(verdict.level(), Confidence::Fragmentary);
        assert_eq!(
            verdict.line(),
            "fragmentary — kernel < 6.1 may overstate, compressed mount"
        );
    }

    /// ...but a selection that never touched FIEMAP (ext-family hardlink
    /// tier) is unaffected by the kernel's extent bookkeeping.
    #[test]
    fn old_kernel_without_mapped_files_does_not_lower_the_level() {
        let mut f = facts(OracleReport {
            exclusive: 1000 * MIB,
            inodes: 2,
            mapped: 0,
            ..OracleReport::default()
        });
        f.old_kernel = true;
        let verdict = Verdict::reclaim(Some(&f), false);
        assert_eq!(verdict.level(), Confidence::Measured);
    }

    #[test]
    fn reason_ranks_limiters_by_bytes_and_stops_at_two() {
        let mut f = facts(OracleReport {
            exclusive: 4000 * MIB,
            unknown: 10 * MIB,
            zfs_files: 2,
            zfs_bytes: 300 * MIB,
            ..OracleReport::default()
        });
        f.compressed = true;
        f.truncated_files = 7;
        f.truncated_disk = 50 * MIB;
        let verdict = Verdict::reclaim(Some(&f), false);
        assert_eq!(verdict.level(), Confidence::Partial);
        // ZFS (300 MiB) > cap (50 MiB) > unknown (10 MiB); the compressed
        // mount is last and falls off the two-limiter cap.
        assert_eq!(
            verdict.line(),
            "partial — 2 files on ZFS, no figure possible, 7 files past the mapping cap"
        );
    }

    #[test]
    fn reason_singularizes_file_counts() {
        let mut f = facts(OracleReport {
            exclusive: 1000 * MIB,
            ..OracleReport::default()
        });
        f.truncated_files = 1;
        f.truncated_disk = MIB;
        let verdict = Verdict::reclaim(Some(&f), false);
        assert_eq!(verdict.line(), "partial — 1 file past the mapping cap");
    }

    #[test]
    fn every_level_has_a_distinct_word_and_no_empty_reason() {
        for level in [
            Confidence::Measured,
            Confidence::Partial,
            Confidence::Fragmentary,
            Confidence::NoFigure,
        ] {
            assert!(!level.word().is_empty());
        }
        let words: Vec<&str> = [
            Confidence::Measured,
            Confidence::Partial,
            Confidence::Fragmentary,
            Confidence::NoFigure,
        ]
        .iter()
        .map(|l| l.word())
        .collect();
        let mut deduped = words.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), words.len(), "words must be distinguishable");
    }
}
