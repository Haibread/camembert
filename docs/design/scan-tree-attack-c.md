# Adversarial review — Option C (frozen structure, epoch snapshots)

> Verdict: **VIABLE WITH FIXES — but do not adopt for the MVP.** "An
> elegant Wave-2/3 architecture wearing a Wave-1 costume."

## Findings

1. **Whole-directory batches break the MVP thesis [MAJOR, structural,
   near-FATAL]**: a directory is invisible until fully enumerated —
   a 1M-entry Maildir at cold statx rates = **5–20 s of nothing**, its
   GBs contributing zero to every ancestor total, then a pop. Partial
   delivery would forfeit the frozen-run premise (the design's own §12
   admits the bet) — so it's unfixable within C. Server-scale single
   directories are the flagship customer. A and B stream; C
   freezes-then-pops exactly where "instantané" matters most.
2. **Dirty-chunk math optimistic 4–10× [MAJOR]**: work-stealing scatters
   active dirs across allocation order; occupancy math →
   P(chunk clean) ≈ 0 in steady state → the "worst case" (40 MB/epoch,
   400 MB/s) is the *normal* sustained cost, plus unaccounted full-L3
   eviction 10×/s on a 4-core VPS.
3. **Watermark race: attack refuted [NITPICK]** — the builder is the
   sole node writer; Release/Acquire via ArcSwap makes below-watermark
   reads race-free. One real nit: snapshots must clone the chunk-ptr
   vector, never share a growing Vec (realloc UAF).
4. **Hardlink owner switches: stall refuted, honesty MAJOR**: total
   switch cost ~0.7 s spread over the scan (ln(k) minima) — no stall.
   But "smallest path *seen*" means backup-farm totals swing wildly and
   are provisionally wrong for much of the scan — against the "honest
   numbers" thesis. Needs a UI "attribution provisional" affordance.
5. **Builder ceiling: "6 cache lines/entry" attack refuted** (sequential
   SoA appends amortize <1 line/entry); 155–225 ns/entry estimate holds,
   but is unbenchmarked, and the worker-side whole-directory sort
   (Maildir!) is a real unbudgeted 0.3–0.5 core [MINOR].
6. **Cold deep descent [MINOR]**: rows appear fast, contents/sizes wait
   for each level's full batch — spinners over emptiness; compounds #1.
7. **Snapshot retention: refuted** — bounded at ~80 MB as claimed even
   under SIGSTOP; nit: cap concurrent snapshot holders (UI + filter +
   dump each pinning different epochs).
8. **Two-speed display [MINOR]**: 30 fps global header vs 10 Hz rows
   reads as jank on the product's core surface, and the cadence is
   floor-locked by the CoW budget (20–30 Hz would be 2–3× the cost).
9. **Strategic [MAJOR]**: for the MVP metric C is dominated by A (KB/frame
   view-scoped copies vs 40 MB/epoch whole-table CoW; streaming
   partial dirs vs freeze-then-pop). C genuinely wins on wave-2/3
   features: lock-free parallel filter/diff folds over the frozen
   structure, and dump-native pre-sorted child runs.

## Survived

Watermark safety. Snapshot retention bound. Builder estimate (order of
magnitude). CoW-over-alternatives reasoning (vs triple_buffer /
left-right / imbl). Dump integration & filter mechanics — the design's
real strengths.

## Recommendation

Ship the MVP on Option A; revisit C's frozen-snapshot substrate at
wave 2–3 when filter/diff/dump-streaming land — where its advantages
earn their cost.
