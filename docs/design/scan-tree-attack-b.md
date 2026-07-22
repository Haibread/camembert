# Adversarial review — Option B (shared arena)

> Verdict: **NOT VIABLE as the chosen design** — one FATAL correctness
> race as specified, and strategically dominated even if fixed.

## Findings

1. **"False sharing impossible" is false for DirAux [MAJOR]**: the 64 B
   DirAux line mixes run descriptors the TUI Acquire-reads every frame
   with aggregate fields many workers fetch_add — ~1500 line transfers/s
   bouncing between the TUI and aggregating cores. Trivial fix (split
   read-mostly/hot lines), but a headline claim is false.
2. **0.3 %/core holds only for balanced trees [MAJOR]**: striping covers
   root+depth-1, but the realistic skewed tree (one dominant subtree at
   depth ≥2 — `/var/lib/docker/overlay2`…) concentrates 1.5M fetch_adds
   on ONE unstriped line ≈ 0.3–0.45 s of serialized coherence that all
   32 workers periodically stall behind. No mechanism detects a hot deep
   line.
3. **io_uring termination race = silent data corruption [FATAL as
   specified]**: "getdents EOF drops the self token" while statx
   completions are still in flight → dir marked complete with children
   at size 0 → wrong ancestor totals (permanent undercount) AND wrong
   dump blocks emitted. The two loom models don't cover it. Fix: gate on
   outstanding-statx == 0 — never mentioned.
4. **"Totals live and exact" overstated [MINOR]**: a subdir contributes
   to its parent only at subtree completion → chunky total updates
   (0 B → full size jumps), the opposite of the "instantané" feel; within
   spec but oversold.
5. **Memory omits per-worker chunk fragmentation [MAJOR]**: 32 workers ×
   half-empty 65 536-slot dir-chunks ≈ ~96 MB slack + file-chunk slack →
   real RSS **~580–620 MB**, 2× the 300 MB target — worst of the three.
6. **Unsafe surface undercounted [MAJOR]**: six concurrent protocols
   (publish, pending, overflow lists, name-arena reads, packed-atomic
   RMW, tombstones); loom models cover two. Failure mode is UB in the
   user's terminal on a server.
7. **Streaming republish × chunk-boundary overflow underspecified
   [MAJOR]**: naive `first..first+count` walks off the chunk end; the
   mid-stream switch to the overflow list needs its own ordering spec.
   Plus an unstated load-bearing scheduler invariant (drain one dir's
   getdents before the next).
8. **TUI read cost at depth: attack conceded [NITPICK]** — viewport is
   ~50 rows; double indirection is sub-µs/frame.
9. **Strategic kill [FATAL]**: zero-copy saves ~48 KB/s of memcpy the
   user cannot perceive, paid with 400+ lines of unsafe, ~600 MB, and a
   novel unproven protocol. A single-owner integrator costs ~2 % of a
   core — not a bottleneck. B is dominated by A on memory, safety, and
   watch mode. **B's one real innovation — per-directory
   subtree-completion aggregation with striped top counters — is
   orthogonal to the arena and should be grafted onto Option A.**

## Survived

Node-chunk single-writer publish. Overflow rate estimate (conservative).
Dump-writer sort. Viewport read cost. Balanced-tree aggregation math.
Filter-as-parallel-column.

## Recommendation

Do not build B. Take its aggregation strategy into A's storage model —
same contention win, none of the unsafe surface, memory blowup, or the
io_uring race.
