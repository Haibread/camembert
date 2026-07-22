# Scan tree (UI-during-scan) — options dossier for the co-design session

**Status: options for discussion, nothing decided.** Synthesizes one
research pass ([research](scan-tree-research.md)), three design proposals
([A: single-owner](scan-tree-option-a-owner.md),
[B: shared arena](scan-tree-option-b-shared.md),
[C: frozen structure + epoch snapshots](scan-tree-option-c-snapshot.md)),
and one adversarial review each ([attack A](scan-tree-attack-a.md),
[attack B](scan-tree-attack-b.md), [attack C](scan-tree-attack-c.md)).
Answers HANDOFF open question §7.2, the last blocker before the engine.

## TL;DR

| | A — single-owner thread | B — shared arena | C — frozen + epoch CoW |
|---|---|---|---|
| Adversarial verdict | **Viable with fixes** | **Not viable** (1 FATAL race + strategically dominated) | Viable with fixes, **wrong milestone** |
| Concurrency model | workers → batches → 1 owner; UI gets view snapshots | workers write arena in place; UI reads in place | workers → batches → 1 builder; UI gets whole-table CoW snapshots |
| Per-entry contended atomics | zero | fetch_add chains (hot deep lines unsolved) | zero |
| Big-directory live fill | streams (once contiguity issue fixed) | streams | **freeze-then-pop (structural, unfixable)** |
| Honest RSS @ 10M | ~460 MB typ / 700–900 MB worst-case workloads | ~580–620 MB | ~430–500 MB (+ 400 MB/s sustained CoW traffic) |
| Unsafe surface | none | ~400+ lines, 6 protocols, UB failure mode | none |
| Correctness landmines | hardlink pass = 2–6 s UI freeze (fixable); holding map unbounded (fixable) | **io_uring termination race → silent corruption** (fixable but symptomatic) | hardlink live totals swing wildly on backup farms |
| Prior art | every shipped tool (dua, ncdu2, diskonaut lineage) | none | none |

## What the adversarial pass established

1. **All three proposals converged on the same core insight, and it
   survived every attack: delete per-entry contention instead of
   mitigating it.** Aggregate per *directory* (not per file), and either
   single-ownership (A, C) or completion-time bubbling (B) makes the
   root-cache-line meltdown structurally absent. This is settled.
2. **Two attackers independently reached the same recommendation**:
   Option A's architecture, with B's one real innovation grafted on
   (per-directory completion aggregation; striped counters if ever
   needed) — and C's frozen-structure substrate revisited at wave 2–3,
   where its lock-free parallel filter/diff folds and dump-native sorted
   runs earn their cost.
3. **Streaming partial directories is the MVP's hill to die on.** C's
   whole-directory batches — elegant everywhere else — freeze-then-pop
   exactly the server-scale directories (Maildir, CI artifacts) the tool
   exists to investigate. Any chosen design must stream large
   directories incrementally. This kills C for wave 1 and forces A to
   fix its contiguity-vs-sections contradiction properly.
4. **Every proposal's numbers were priced at cache speed on DRAM-sized
   structures.** Honest owner/builder cost is ~100–200 ns/entry
   (interner DRAM probes dominate), giving a single-thread ceiling of
   ~8–10M entries/s — fine under HANDOFF's cold-cache priority, but it
   must be stated. The 300 MB @ 10M HANDOFF target is not reachable by
   any of the three as written (realistic: ~400–500 MB typical; worse on
   unique-name or hardlink-heavy trees).
5. **Hardlinks strike again** (cf. the dump dossier): A's end-of-scan
   correction pass would freeze the UI for seconds on backup farms; C's
   live canonical switching makes totals swing wildly mid-scan. The fix
   direction: first-seen attribution live + correction *off the critical
   path* + an explicit UI "attribution provisional" state. The dump's D2
   rule stays satisfied at emission time.
6. **B's fatal lesson generalizes**: completion detection must gate on
   *outstanding statx results*, not getdents EOF — whatever design wins,
   under io_uring the "directory done" event fires only when every
   in-flight statx for it has landed.

## Recommendation to challenge in session

**Option A (single-owner thread) as the MVP architecture**, amended with
its attacker's fix list plus B's graft:

1. Children storage that supports **streaming large directories** without
   breaking dump DFS: contiguity guaranteed only per *section*; a
   directory's children = a small list of contiguous runs (1 run for the
   ~99 % of dirs that fit one batch; N runs for giants). Slice views
   become run-list iterations; dump finalize walks runs in order.
2. **Batched, per-directory aggregation** (B's graft, already in A's
   §4): owner adds pre-summed section deltas up the chain — plain adds.
3. **Bounded holding map** for parent-before-child reordering (cap +
   spill), loom/stress-tested.
4. **Nav-preemptible integration** (check the nav cell between sections,
   not batches).
5. **Hardlink correction off the owner's critical path**: first-seen live
   + background re-attribution overlapped with ordered-dump finalize +
   "provisional" UI state until done.
6. **Completion gated on outstanding-statx == 0** (B's lesson).
7. **Honest budgets in the ADR**: ~100–180 ns/entry owner cost,
   ~8–10M entries/s ceiling (accepted: cold-cache is the priority
   regime), RSS ~450 MB typical @ 10M with worst-case workloads called
   out; the packed-node diet (24 B, mtime i64) as a tracked follow-up,
   not a promise.

**And keep C's frozen-structure idea in the back pocket for wave 2–3**
(filter/diff parallel folds over an immutable post-scan structure) — the
post-scan tree naturally freezes anyway, so C's best trick applies to the
frozen phase without its scan-time costs.

## Decisions needed in the co-design session

1. Adopt the A + B-graft recommendation? (Or challenge: is the
   many-core hot-cache ceiling worth more architecture?)
2. **Run-list children representation** — accept the extra indirection
   for streaming giants, or cap "streamed" dirs at a threshold?
3. **Hardlink UX**: provisional-attribution marker in the TUI — subtle
   (footer note) or explicit (per-row badge)?
4. **Memory target**: re-baseline HANDOFF's 300 MB to ~450 MB typical,
   or commit the packed-node work into the MVP?
5. **UI staleness budget**: 33 ms publish cadence (A) is the target;
   confirm 250 ms degraded cadence for >20k-child dirs is acceptable.
