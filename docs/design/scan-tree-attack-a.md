# Adversarial review — Option A (single-owner thread)

> Verdict: **VIABLE WITH FIXES** — "the architecture is right; the spec
> sheet is fiction."

## The through-line flaw

Every owner-side cost is priced at L1/register speed on a structure that
is 460 MB and therefore DRAM-bound (interner ~75 MB, DirMeta 38 MB, arena
320 MB — none fits a 4-core VPS's 4–8 MB L3). The machine the doc names
as its best case is where the budget is most wrong.

## Findings

1. **50 ns/entry is optimistic 2–3× [MAJOR]**: the interner probe alone
   costs 80–150 ns (2 DRAM misses); realistic **100–180 ns/entry →
   10–18 % of a core at 1M/s**, not 5 %. Unbudgeted: `Vec` doubling
   copies 160 MB (~8–16 ms owner stall) mid-scan. Aggregation
   per-directory survives structurally but at 24–72 ms/s, not 2–3.
2. **"Children contiguous" contradicts batching [MAJOR]**: sections of a
   huge dir interleave with other dirs' batches in the append-only arena
   → not contiguous → slice views and dump DFS break; or a dir is always
   one message → a 1M-child dir is a 32 MB message, blowing the 10 MB
   in-flight cap. Contiguity + cap + sections: pick two. Unreconciled.
3. **Holding map unbounded under work stealing [MAJOR]**: stolen child
   batches routinely arrive before their parent's (concrete scenario
   given); on a deep fast tree the map holds a large tree fraction —
   hundreds of MB, absent from the budget; the in-flight cap does not
   bound it.
4. **Nav latency [MAJOR]**: the owner checks the nav cell between
   batches; a 500k-entry batch = ~60 ms integration → nav waits.
   Prefetch under fast scroll ≈ 5–10 % of a core of speculative work —
   needs debounce.
5. **Owner is the ceiling on many-core hot cache [MAJOR on "scales up"]**:
   honest ceiling ~8–10M entries/s; a 32-core hot-cache scan (20–40M/s
   possible) throttles 2–4× on the owner. Defensible given HANDOFF's
   cold-cache priority — but must be stated, not hidden.
6. **Hardlink correction pass = multi-second UI freeze [MAJOR→FATAL on
   backup farms]**: 5M re-attributed inodes × 2 chains × depth 20 ≈ 200M
   scattered DirMeta visits ≈ **2–6 s** with the owner 100 % busy — no
   snapshots, no nav — at the exact moment the user wants to explore.
   "ms total" is off ~1000×. Also breaks the monotonicity invariant the
   read path leans on (totals drop at completion).
7. **460 MB dishonest on target workloads [MAJOR]**: unique-name trees →
   interner 180–250 MB; backup farm → hardlink map 200–300 MB; holding
   map unbudgeted. Realistic worst **700–900 MB**. Packed-node mtime u32
   silently corrupts the age feature (pre-1970/post-2106, no ns).
8. **Live filter chunking degrades on hot scans; single pass over a
   still-growing arena misses concurrent arrivals [MINOR]** — needs
   re-pass semantics. Watch-mode leak vs hours-long watch sessions in
   tension [MINOR].
9. **Dominated**: many-core hot-cache throughput (vs per-thread
   accumulation designs, architecturally); multi-view GUI. **Not
   dominated**: memory vs persistent-snapshot designs; correctness cost.

## Survived

Zero per-entry contended atomics (the central claim) — TRUE. Wait-free
render loop. Completion cascade. Tier-1 streaming dump. Per-directory
aggregation principle.

## Required fixes

1. Re-price all budgets at DRAM latency; state the owner ceiling.
2. Resolve contiguity vs sections vs in-flight cap (pick a mechanism).
3. Bound the holding map (cap + spill policy), loom/stress-test reorder.
4. Make big-batch integration nav-preemptible.
5. Move the hardlink correction off the owner's critical path
   (background/finalize-parallel); re-scope registry memory at 5M inodes.
6. Re-baseline memory honestly (unique-name/hardlink-heavy worst case).
7. Debounce prefetch; mtime i64 (or document age corruption); filter
   re-pass semantics.
