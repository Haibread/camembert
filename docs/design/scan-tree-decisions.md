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

## Addendum (2026-07-24) — default flips to crossing filesystem boundaries

User decision, not a re-litigation of D1–D5: the scan's mount-boundary
behavior (the `st_dev` check grafted from D1's engine) now **crosses
filesystem boundaries by default**. `--cross-filesystems` is removed;
`--one-filesystem`/`ONE_FILESYSTEM` is the opt-out, restricting a scan to
the root's own filesystem. Kernel pseudo-filesystems (`/proc`, `/sys`,
cgroups, …) stay excluded by filesystem magic regardless of this flag —
unchanged from before.

**Rationale**: bytes on a `tmpfs` or another disk mounted under the scan
root are real usage of *those* filesystems, not phantom totals invented
by the scanner — stopping at the first mount point silently hid disk
usage a user asked to see. The disk gauge's single-filesystem percentage
is meaningless once a scan spans more than one device, so rather than
show a dishonest number against only the scan root's statvfs, the gauge
now captions multi-filesystem scans as "spans N filesystems · gauge
shows the scan root's" (`ScanOutcome::device_count`) instead of a lying
percentage.

**Accepted caveats** (documented in `--one-filesystem`'s help text and
the README's Honest numbers section, not reopened here):

- **btrfs snapshot subvolumes**: descending into subvolumes also walks
  snapshot subvolumes (e.g. `.snapshots`), which can multiply-count
  snapshotted data. `--one-filesystem` avoids it.
- **bind mounts and multi-mounts**: the `st_dev` check cannot see *why*
  two paths share a device. A bind mount whose source is on the same
  filesystem is descended as an ordinary directory and double-counted
  even under `--one-filesystem` (its `st_dev` never differs from its
  parent's); the same block device mounted at two paths inside the scan
  is descended twice under the default crossing behavior. Hardlink
  deduplication only catches `nlink > 1` files, so `nlink == 1` files and
  directories still double-count in both cases.

A **traversal-dedup dossier** (tracking visited `(st_dev, root inode)`
pairs to collapse both cases) is planned — see HANDOFF's suggested next
steps — but is out of scope for this addendum: the caveats above are
accepted, documented trade-offs of shipping crossing-by-default now
rather than blocking it on a dedup design.
