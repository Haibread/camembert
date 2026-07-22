# Option B — shared arena, in-place concurrent reads

> Proposal by a design agent instructed to push the shared-arena angle as
> hard as it honestly can. Not a decision.

## 1. Pitch

Workers write nodes directly into a pinned, chunked, append-only arena;
the TUI dereferences the very same memory every frame with zero copies,
zero channels, and zero owner thread. Almost nothing in this tree is
actually contended: a directory's children are enumerated by exactly one
worker (single-writer sibling runs, published with one Release store),
nodes are immutable-once-published except a handful of write-once or
monotonic atomics, and aggregate propagation is batched per *directory*
up the ancestor chain, with striped counters only at root and depth-1.
Result: ≤ ~60 ns structure overhead per entry, sub-50 ms UI staleness,
navigation at pointer-chase cost, and a completion protocol feeding the
dump writer exactly the blocks dump-v1 wants.

## 2. Data layout

**Arena** = global chunk table (65 536 `AtomicPtr` slots, 1 MB) →
fixed chunks of up to 2^16 nodes. `NodeId = chunk_id:16 | offset:16`.
Chunks 64-B aligned, never moved, never freed during scan. Workers own
their current chunks, bump-allocate unsynchronized; a global `AtomicU32`
hands out chunk ids (1 RMW / 65 536 nodes). **Chunks are homogeneous:
dir-chunks vs file-chunks** — a dir-chunk carries a parallel `DirAux`
array at the same offsets, so "is this a dir" is free from the chunk id.

**Node (32 B):** name u32 | parent u32 | apparent AtomicU64 |
disk AtomicU32 (512-B sectors, ≥2 TiB → side map) | mtime AtomicU32 |
packed u32 kind(3)+flags(5)+meta_ref(24) into interned (uid,gid,mode)
table | 4 B pad. Frozen fields written pre-publish; mutable fields are
write-once atomics. False sharing inside chunks impossible by
construction (one writer per chunk).

**DirAux (64 B, dirs only):** file_run AtomicU64 (first:u32,count:u32),
dir_run AtomicU64, overflow AtomicPtr<RunList>, ta AtomicU64,
td AtomicU64, tn_te AtomicU64, pending+state AtomicU32×2, node, dev_ref.

**Names:** per-worker bump byte-arenas behind the same chunk-table trick;
raw bytes, varint length. Dedup via thread-local 64k hot-name map only
(no shared map on the hot path); cross-thread duplicates tolerated.

**Striped top counters:** root + depth-1 (typically <100 dirs) get
min(threads,16) cache-line-padded stripe slots (~100 KB total).

## 3. Thread topology

N scan workers (work-stealing deque; io_uring statx ring per worker,
thread-pool fallback) + TUI thread (reads arena in place) + dump-writer
thread (pops completed-dir ids off an MPSC) + later one watch-mutator
thread. No thread ever waits on another to render or scan.

## 4. Aggregation

**Per directory, not per file.** A worker finishing directory d's
enumeration sums its direct children locally (linear pass over memory it
just wrote), then walks d's ancestor chain doing fetch_add of the delta
(3 RMWs per level; depth-1/root go to stripe slots).

Numbers @ 10M entries, 1M dirs, depth 10, 8 workers, 10 s scan:
- Chain RMWs: 1M × 10 × 3 = 30M, mostly uncontended at 7 ns ≈ 0.21 s
  total across workers → **≤0.3 % of a core each**; ~10 ns amortized
  per entry.
- Root unstriped would be ~45 ms/s of coherence — survivable; striping
  (9× at 4 cores, 80× at 16 threads) makes it vanish.
- Naive per-file design would be 30M root RMWs — the 1–1.5 s meltdown,
  structurally avoided.

One counter set, live and exact: totals accumulate live and are already
exact at completion; completion only marks state.

## 5. TUI read path

Per frame: Acquire-load file_run/dir_run (+overflow), iterate contiguous
child runs (10k children = 320 KB streamed in µs), snapshot each row
(relaxed atomic loads), sort frame-locally.

**Safety argument:** (a) frozen fields fully written before the Release
store publishing the run count; Acquire readers see initialized nodes
(standard reserve→init→publish); (b) mutable-after-publish fields are
atomics, relaxed loads, torn values self-correct next frame; (c) nothing
moves or frees during scan → `&'tree Node` borrows sound without
epoch/GC machinery.

**Streaming giant dirs**: the enumerating worker republishes (first,
count) with growing count per getdents batch (~1000 entries) — a 1M-entry
dir fills in live. **Sort**: <10k children full sort <1 ms/frame;
pathological 1M-child dir: select_nth top-50 O(n) ≈ 5–10 ms with per-dir
sort cache, resort every ~200 ms. **Staleness worst ~50 ms.**

## 6. Subtree completion

Weighted termination detection, race-free under stealing: pending = 1
self token at creation; parent fetch_add(1) per child dir *before*
pushing to the steal queue; completing child flushes final deltas
(Release) then fetch_sub(1) on parent; parent's enumeration end drops the
self token. Whoever hits 0 marks complete, recurses upward, pushes to the
dump queue. Release/Acquire on pending guarantees totals visible before
"complete" is observed. Exactly-one-trigger and totals-before-complete
are the two loom models.

## 7. Hardlink registry

256-way sharded map `(dev, ino) → {first_seen, links: SmallVec}`, touched
only when nlink>1. Live: first-seen attribution. At finalize: canonical
owner per dump §8 (smallest path, raw-byte component-wise, chains
walked — groups tiny), corrective delta subtract/add up to the LCA.
Global totals unchanged; only per-directory splits shift, once, during
the visible "finalizing" state. Registry stores (dev,ino) for dump
emission so the 32 B node needn't carry an inode.

## 8. Dump-writer integration

During scan: writer consumes the completion queue, emits each completed
dir as a block — file-run entries sorted by raw name bytes *on the writer
thread* (index vector sort; arena untouched). Tier-1 `ordered:false`,
crash-tolerant. At finalize: hardlink canonicalization, then DFS preorder
walk emitting ordered dump with ta/td/tn/te from DirAux, `x` lines, seek
table, `e`.

## 9. Watch mode & filter

- **Watch**: post-scan there are no workers → one watch-mutator thread
  inherits the single-writer role. Size change = same chain-fetch_add
  with signed delta. Create = allocate in mutator's chunk, append via
  overflow-run (same reader contract). Delete = tombstone + negative
  deltas; slots leak until rescan (session-scoped, honest).
- **Filter**: tree frozen post-scan → filtered view is a fresh parallel
  column `Vec<FilteredAgg>` indexed like DirAux, rayon bottom-up pass:
  320 MB streamed ≈ 100–300 ms. "Another aggregate universe" is a
  column, not a redesign.

## 10. Quantified budget

| item | @ 10M entries |
|---|---|
| nodes 32 B × 10M | 320 MB |
| DirAux 64 B × 1M dirs | 64 MB |
| names (bump, hot-name dedup) | ~80–120 MB |
| tables, stripes, registry, queues | ~10 MB |
| **total RSS** | **~480–520 MB** |

Per-entry cost: ~55 ns (<10 % of the ≥500 ns syscall floor). Staleness
~50 ms worst. Navigation = one pointer chase. Works at 4 and 32 cores.

## 11. Crates and unsafe

`crossbeam-deque`, `crossbeam-queue`, `parking_lot` (registry shards),
`io-uring` + `rustix`, `ratatui`, `zstd`, `hashbrown`, `rayon` (filter
only). Absent: dashmap/arc-swap/im/petgraph.

Unsafe in two modules, ~400 lines: `arena` (MaybeUninit init pre-publish;
bounds-checked NodeId → &Node) + packed-atomic publish helpers. Tested:
**loom** (publish vs reader; pending protocol), **miri** on arena unit
tests, **ThreadSanitizer** stress in CI.

## 12. Honest weaknesses

1. **Memory worst-served**: ~500 MB vs 300 MB target; 32 B/node is the
   floor with exact st_size + mtime + live atomics; trim levers cost dump
   fidelity.
2. **The publish protocol is load-bearing unsafe** — a bug is UB in the
   TUI. Loom covers the model, not the code.
3. **Off the beaten path**: no shipped tool validates in-place concurrent
   reads for this workload (counterpoint: dua's stalls and gdu's retreat
   from channels are evidence against the beaten path at this
   throughput) — but no prior art inherited.
4. Hardlink live totals transiently mis-attributed until the finalize
   correction — the one non-monotonic moment.
5. Million-child dirs need select-nth + sort-cache.
6. Watch-mode deletions leak until rescan.
7. Contention numbers from one microbenchmark source; striping thresholds
   must be re-benched on the VPS and ARM before being trusted.
