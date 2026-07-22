# Scan tree — decisions (co-design session, 2026-07-22)

Outcome of the co-design session over the
[options dossier](scan-tree-options.md). Settled; reopening one requires a
new element. Closes HANDOFF open question §7.2 — nothing blocks the engine
implementation anymore.

## D1 — Architecture: single-owner thread (Option A) + Option B's graft

The engine uses the **single-owner-thread architecture**: scan workers
(work-stealing, openat/getdents64/statx) send pre-summed per-directory
batches over one bounded channel to a single owner thread that is the sole
writer of a plain non-concurrent arena; the TUI receives view-scoped
snapshots via arc-swap and is wait-free. Grafted from Option B:
**per-directory batched aggregation** (plain adds up the ancestor chain,
zero per-entry atomics). Option C's frozen-structure substrate is noted
for wave 2–3 (parallel filter/diff folds over the post-scan frozen tree).

The full amendment list from the adversarial review is binding on the
implementation (see the dossier's recommendation §): bounded holding map
for parent-before-child reordering, nav-preemptible integration (check the
nav cell between sections), completion gated on outstanding-statx == 0
(Option B's fatal lesson), honest DRAM-priced budgets in code comments and
benches.

## D2 — Children storage: run lists

A directory's children are stored as a **list of contiguous runs** in the
arena: one run for the ~99 % of directories that fit one batch, N runs for
large directories streamed section by section. This preserves streaming
fill-in for server-scale directories (Maildir, CI artifacts) — the MVP's
headline feel — while keeping slice-like iteration and a well-defined dump
DFS (runs walked in order, merged sort at finalize).

## D3 — Hardlink UX: discreet footer note

Live totals use first-seen attribution; canonical re-attribution (dump
rule D2) runs **off the owner's critical path**, overlapped with finalize.
While uncorrected hardlinks exist, the TUI shows a **status-bar note**
("provisional totals (hardlinks) — corrected at scan end"), shown only if
hardlinks were actually seen. No per-row badge (rejected: extra tracking
for a rare case).

## D4 — Memory target re-baselined: ~450 MB @ 10 M entries

The HANDOFF's ~300 MB figure is superseded: the honest MVP target is
**~450 MB RSS @ 10 M entries** (typical trees; unique-name and
hardlink-heavy worst cases documented, not hidden). The packed 24-byte
node (u40 sizes + escape map, mtime i64) stays on the backlog as a
follow-up behind the same accessors (~380 MB), not in the MVP.

## D5 — UI cadence: 33 ms, degraded 250 ms

View-snapshot publication targets **33 ms** (≈30 fps). Directories with
more than ~20k children degrade to a **250 ms** publish cadence for that
view only, displayed as "updating…"; the render loop itself never blocks
and never drops below full frame rate.
