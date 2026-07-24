# Scan tree — decisions (co-design session, 2026-07-22)

Outcome of the co-design session over the
[options dossier](#condensed-reasoning-trail). Settled; reopening one requires a
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

## Condensed reasoning trail

### Research

Facts gathered 2026-07-22 for HANDOFF §7.2, no recommendations. Every
shipped tool with a live UI converges on the same pattern: a single owner
thread mutates the tree while workers communicate via channel or shared
atomics, and the UI reads the owner's data or polls atomics — ncdu2's
sink abstraction, dua-cli's `integrate_traversal_event()` sole mutator,
gdu's move from channels to atomics+polling under contention. None of the
surveyed tools uses DashMap, arc-swap, im, or per-node locks for the live
tree. The crux number: an uncontended atomic add is ~7 ns but a contended
one is ~110 ns (mutex ~125 ns) and gets worse with more cores — striping
counters recovers ~9× at 4 cores, ~80× at 16 (Travis Downs,
"A Concurrency Cost Hierarchy"), which is the empirical basis for
per-directory (not per-file) aggregation. HANDOFF's ~300 MB @ 10M figure
was found to assume a padded per-node atomic (640 MB of padding alone) or
an unresolved false-sharing tension with a packed one — "no prior art
found resolving this for a `Vec`-arena tree". Ten items were flagged
explicitly as unconfirmed (ncdu2 mid-scan browsing, diskonaut's tree
structure, left-right/`im` maturity, a canonical LongAdder crate,
subtree-completion aggregation as prior art, snapshot-consistency needs,
a suspicious 145 % interner-overhead figure, and the back-of-envelope
contention estimate itself) — flagged, not silently assumed.

### Options

**A — single-owner thread** (dua-cli lineage pushed to scale). Core idea:
one thread is the sole writer of a plain `Vec<Node>` arena; workers ship
pre-summed per-directory batches over a bounded channel; ancestor
aggregation is plain non-atomic `u64 +=`, deleting root contention rather
than mitigating it. Decisive pros: zero per-entry contended atomics, zero
unsafe, direct prior art (dua/ncdu2/diskonaut lineage), best-behaved
memory of the three once re-priced (~460 MB). Decisive cons found by
attack: "contiguous children" contradicted its own batching model, the
holding map was unbounded, and priced at register speed rather than DRAM.
**Won** (D1) because every con was fixable in place and the architecture
had no structural blocker, unlike B (fatal race) and C (freeze-then-pop).

**B — shared arena** (in-place concurrent reads, zero-copy). Core idea:
workers write directly into a pinned chunked arena; the TUI dereferences
the same memory with no owner thread and no channel; aggregation batches
per directory with striped counters at the hot top levels. Decisive pro:
lowest theoretical per-entry structure overhead (~55 ns) and zero-copy
reads. Decisive cons: a **FATAL io_uring termination race** (getdents EOF
could drop the completion token while statx completions were still in
flight, silently corrupting totals and dump blocks), ~400+ lines of
load-bearing unsafe across six under-modeled concurrent protocols, and
real RSS of ~580–620 MB once per-worker chunk fragmentation is counted —
worst of the three. **Lost outright** ("not viable"): the zero-copy win
saves an imperceptible ~48 KB/s of memcpy for a cost of hundreds of MB,
unproven protocols, and UB-in-production risk. Its one surviving
contribution — per-directory batched aggregation with striped top
counters — was grafted onto A (D1).

**C — frozen structure, epoch snapshots** (decoupled CoW snapshots). Core
idea: node/name/child-run structure is append-only and frozen at write
time (whole directory enumerated, sorted, and handed over as one
contiguous run); only a small mutable directory-aggregate table is
owned by a single builder, which arc-swaps copy-on-write snapshots to the
UI roughly every 100 ms. Decisive pros: cheapest read path once frozen,
child runs arrive dump-Tier-1-pre-sorted for free, and it is the design
that scales best to future parallel filter/diff folds over an immutable
structure. Decisive con: its whole-directory-batch premise means a
directory contributes **nothing** to any ancestor total until fully
enumerated — freeze-then-pop on exactly the server-scale directories
(Maildir, CI artifacts) the tool exists to investigate, and the design's
own weaknesses section admits this can't be fixed without abandoning the
frozen-run premise. **Lost for wave 1** (dominated by A on the MVP's
streaming-fill-in requirement) but **kept** — D1 explicitly earmarks its
frozen-structure substrate for wave 2–3, where the post-scan tree freezes
naturally anyway and C's parallel-fold trick applies without its
scan-time cost.

### Attack findings

#### Attack A (Option A — viable with fixes)

1. The 50 ns/entry owner cost was optimistic 2–3×; realistic cost is
   100–180 ns/entry once interner DRAM misses are counted. Adopted: D1's
   binding amendment list requires honest DRAM-priced budgets in code and
   benches instead of the register-speed estimate.
2. "Children contiguous in the arena" structurally contradicted
   per-section batching of large directories. Adopted: D2 replaces flat
   contiguity with a run-list representation (one run per batch section).
3. The parent-before-child holding map was unbounded under work stealing,
   potentially holding hundreds of MB unbudgeted. Adopted: D1 requires a
   bounded holding map (cap + spill), loom/stress-tested.
4. A large batch (e.g. 500k entries) could stall navigation for ~60 ms
   because the owner only checked the nav cell between batches. Adopted:
   D1 requires nav-preemptible integration (check the nav cell between
   sections, not whole batches).
5. The owner is an architectural ceiling on many-core hot-cache scans
   (~8–10M entries/s vs. 20–40M/s possible), throttling wide parallelism
   2–4×. Accepted as an honest, stated trade-off (cold-cache scans are
   the priority regime) rather than re-engineered away — not revisited
   by D1–D5.
6. The end-of-scan hardlink correction pass would freeze the UI for
   2–6 seconds on backup farms, ~1000× worse than the "ms total" claim,
   and briefly breaks the totals-are-monotonic invariant. Adopted: D3
   moves the correction pass off the owner's critical path, overlapped
   with finalize, with a footer note while totals are provisional.
7. The 460 MB memory figure was dishonest on unique-name or
   hardlink-heavy trees (realistic worst 700–900 MB). Adopted: D4
   re-baselines the target to ~450 MB typical and documents worst cases
   instead of hiding them.
8. Live filter chunking degrades on hot, still-growing scans, and the
   watch-mode tombstone leak is in tension with hours-long watch
   sessions (both MINOR). Not addressed by D1–D5: left open as future
   work, not resolved in this decision set.
9. Option A is architecturally dominated on many-core hot-cache
   throughput and multi-view GUIs, but not dominated on memory (vs.
   persistent-snapshot designs) or correctness cost. This comparison is
   the basis for D1's recommendation of A over B and C.

#### Attack B (Option B — not viable)

1. The "false sharing impossible" claim was false for `DirAux`: the
   64 B line mixes TUI-read run descriptors with worker-`fetch_add`
   aggregate fields, bouncing ~1500 lines/s. Moot: B was rejected
   outright, so the fix (split hot/read-mostly fields) was never applied.
2. The 0.3 %/core contention estimate held only for balanced trees; a
   realistic skewed tree (e.g. one dominant subtree) concentrates ~0.3–
   0.45 s of serialized coherence on one unstriped line with no detection
   mechanism. Moot: superseded by A's single-owner model, which has no
   per-entry contended line at all.
3. **FATAL**: the specified completion protocol could drop a directory's
   "complete" token while statx completions were still in flight, giving
   silent, permanent undercounts and wrong dump blocks. Adopted via
   generalization: D1 requires completion gated on outstanding-statx == 0
   for Option A, citing this as "Option B's fatal lesson."
4. "Totals live and exact" was overstated — a subdirectory's contribution
   only lands at its own completion, producing chunky jumps rather than
   the claimed instantaneous feel (MINOR). Moot: B rejected.
5. Memory omitted per-worker chunk fragmentation, pushing real RSS to
   ~580–620 MB, worst of the three designs. Contributed to rejecting B:
   cited directly in D1's memory comparison against A.
6. The unsafe surface was undercounted: six concurrent protocols, only
   two loom-modeled, with a UB-in-the-terminal failure mode. Contributed
   to rejecting B: D1 notes Option A carries zero unsafe surface by
   contrast.
7. Streaming republish across chunk-boundary overflow was underspecified,
   plus an unstated scheduler ordering invariant. Moot: B rejected before
   this needed resolving.
8. The TUI's double-indirection read cost at depth was conceded as
   sub-microsecond and not a real problem (NITPICK). Not a factor in the
   rejection either way.
9. **Strategic kill**: B's zero-copy win saves an imperceptible amount of
   memcpy at the cost of ~400+ lines of unsafe, ~600 MB RSS, and an
   unproven protocol — dominated by A on memory, safety, and watch mode.
   Adopted as the rejection rationale in D1; B's per-directory
   subtree-completion aggregation with striped top counters was grafted
   onto A rather than discarded.

#### Attack C (Option C — viable with fixes, wrong milestone)

1. Whole-directory batches mean a directory contributes nothing to any
   ancestor total until fully enumerated — 5–20 s of nothing for a
   1M-entry Maildir at cold statx rates, unfixable without abandoning the
   frozen-run premise (MAJOR, structural, near-FATAL). Decisive: this is
   the primary reason C lost the MVP slot to A, which streams.
2. The dirty-chunk CoW cost was optimistic 4–10×: work-stealing scatters
   active directories across allocation order, making the "worst case"
   (40 MB/epoch) the steady-state normal. Moot for the MVP since C was
   deferred; relevant if C is revisited at wave 2–3.
3. The watermark-safety race was refuted (builder is the sole node
   writer; Release/Acquire is sound) — one real nit: snapshots must clone
   the chunk-pointer vector, never share a growing one. Carried forward
   as an implementation note for whenever C's substrate is built.
4. The "stall" claim for hardlink owner switches was refuted (~0.7 s
   spread over the whole scan), but the underlying honesty issue stands:
   "smallest path *seen*" makes backup-farm totals swing wildly and be
   provisionally wrong (MAJOR on honesty). This concern is handled
   instead within A/D3 via first-seen attribution plus an explicit
   provisional-totals note.
5. The builder-ceiling estimate (155–225 ns/entry) held under attack but
   is unbenchmarked, and per-directory worker-side sorting (e.g. for
   Maildir) was an unbudgeted 0.3–0.5 core (MINOR). Moot: C deferred.
6. Cold deep descent shows rows before their sizes arrive, compounding
   finding 1's emptiness problem (MINOR). Moot: C deferred.
7. Snapshot retention was refuted as bounded (~80 MB) even under SIGSTOP;
   one nit — cap concurrent snapshot holders (UI, filter, dump each
   pinning a different epoch). Carried forward as an implementation note.
8. The 30 fps global header vs. 10 Hz per-row cadence reads as jank on
   the product's core surface, floor-locked by the CoW budget (MINOR).
   Informed D5's simpler single-cadence (33 ms, degraded 250 ms) design
   in A instead of adopting C's two-speed display.
9. **Strategic**: for the MVP metric, C is dominated by A (KB/frame
   view-scoped copies vs. 40 MB/epoch whole-table CoW; streaming vs.
   freeze-then-pop), but C wins for wave-2/3 features — lock-free
   parallel filter/diff folds over a frozen structure, dump-native
   pre-sorted runs. Adopted as-is: D1 defers C's frozen-structure
   substrate to wave 2–3 rather than rejecting it outright.

---

The eight source documents this section condenses (research digest,
options dossier, three design proposals, three adversarial reviews) are
recoverable from git history prior to the commit that removed them.
