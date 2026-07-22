# Option C — "frozen structure, epoch aggregates" (decoupled snapshots)

> Proposal by a design agent instructed to push the decoupled-snapshot
> angle as hard as it honestly can. Not a decision.

## 1. Pitch

Split the tree into what never changes and what always changes. The
**structure** (nodes, names, child runs) is append-only and frozen at
write time: a worker enumerates a whole directory before handing it over,
so every directory's children land as one contiguous, pre-sorted,
immutable run. The only mutable state — per-directory subtree aggregates,
completion counters, hardlink ownership — is quarantined in a small
**directory table** (~40 MB @ 10M entries), owned by a single builder
thread using plain non-atomic arithmetic. Every ~100 ms the builder
copy-on-writes the *dirty chunks* of that table and arc-swaps an
immutable `Snapshot` to the UI. The UI never touches scan state; the scan
never waits for the UI; the contended-fetch_add problem is deleted, not
mitigated. Between epochs, striped per-worker atomics feed the global
speedometer so the screen never looks frozen.

## 2. Data layout and snapshot mechanism

**Node arena (frozen, shared read-only).** Chunked SoA arena, 65 536
entries/chunk, chunk pointers stable forever. Columns, 28 B/node: name
u32 | parent u32 | apparent u64 | disk u32 (512-B units, 2 TB saturate +
side map) | mtime u32 | aux u32 (dirs: dir-table idx; files: flags).
Written once, never mutated. Children of a dir occupy one contiguous
run **already sorted by raw name bytes** (workers sort before sending —
also dump Tier-1 order, paid once, off the builder thread).

**Interner**: builder-only FxHashMap over bump arena (~20–40 ns/name).

**Directory table (mutable, builder-private).** 48 B/dir in 4096-row
chunks (192 KiB/chunk): node_idx, first_child, n_children, pending,
state, agg_apparent u64, agg_disk u64, agg_inodes u32, agg_errors u32.

**Side tables**: hardlink registry; sparse node → (ino, nlink) map;
optional `--ext` columnar uid/gid/mode (12 B/node, off by default).

**Snapshot = per-chunk CoW + arc-swap.** Dirty bitmap over dir-table
chunks; per epoch: copy dirty chunks into fresh `Arc<DirChunk>` (clean:
Arc::clone, zero copy), build `Snapshot { node_watermark,
node_chunk_ptrs, dir_chunks, global_stats }`, ArcSwap::store (release).
Why not the alternatives: triple_buffer clones the whole T (40 MB per
publish); left-right keeps 2 full copies + op-log; im/imbl HAMTs cost
3–5× the packed node and turn reads into pointer chasing. Chunk-CoW
gives structural sharing with dense rows and copy cost proportional to
what changed.

## 3. Thread topology

N scan workers (work-stealing dirfds; getdents64; statx via io_uring or
fallback; enumerate a directory fully, sort by raw bytes, pack one batch,
send, bump striped progress counters) → bounded channel (cap ~1024
batches, backpressure = flow control) → **1 builder** (appends nodes,
interns, aggregates, tracks completion, owns hardlinks, publishes epochs,
emits dump events) + 1 dump writer + 1 UI thread (30 fps, ArcSwap load
per frame).

Builder headroom: ~200–300 ns/entry ≈ 0.2–0.3 cores at 1M entries/s;
batch rate ~66k msgs/s at mean fanout ~15 (order of magnitude below
channel degradation). Saturates ~2–3M entries/s (estimate); sharded
builders as documented escape hatch.

## 4. Aggregation

Workers do **zero** aggregation and touch zero shared tree state. The
builder, per batch for dir D: append children, add direct sums to D, walk
parent links to the root adding deltas — plain u64 adds, ~1–2 ns. At 66k
batches/s × ~10 ancestors: ~0.7M adds/s — noise. Single writer → the
root's cache line never leaves one core's L1. Immediate propagation (not
completion-time) is what makes ancestors visibly grow while a subtree
streams in.

## 5. TUI read path

- Per frame: ArcSwap load (~5 ns), render current dir from snapshot
  (dir row → contiguous child run → per-subdir aggregates from dir
  chunks). Navigation pure snapshot reads — instant, including into
  not-yet-scanned dirs (row exists from parent enumeration, spinner).
- Per epoch (100 ms): re-sort current dir's children into a cached index
  vec. Monotonic growth → ranks drift slowly → pdqsort near-linear;
  <10k children well under 1 ms. Pathological 500k-child dirs: 10–20 ms
  once per epoch, capped by 250 ms cadence fallback. Ties broken by name
  (no jitter).
- 100 ms feels continuous (rows reorder up to 10×/s under 30 fps render);
  500 ms would be visible stepping.
- Between epochs: striped atomics (64-B padded per-worker slots) for
  totals + per-worker "currently scanning" path cell → global numbers
  tick at 30 fps even though per-dir numbers tick at 10 Hz.
- **Worst-case staleness of any per-dir number ≈ 105 ms** (adaptive
  worst ~260 ms).

## 6. Subtree completion

pending = 1 (own enumeration) + # incomplete direct subdirs. Batch
carries "enumeration complete" flag → builder decrements self-bit; child
reaching 0 decrements parent recursively. Single-threaded → no
lost-decrement races by construction. Complete state visible next
snapshot (spinner → checkmark, total final). Root complete = scan done.

## 7. Hardlink registry

Builder-owned FxHashMap<(dev, ino), {owner_node, nlink, seen, apparent,
disk}> (~48 B/entry). First sighting: provisional owner, counted. Later
sightings: compare paths (component-wise raw bytes — the dump §4/§8
comparator) by walking parent chains; if newcomer smaller: subtract up
old owner's chain, add up new owner's, swap. The live tree satisfies
dump rule D2 **at every epoch** — finalize needs no fixup pass. Honest
consequence: an owner switch makes a total *decrease* — the sole
exception to monotonic fill-in.

## 8. Dump-writer integration

The frozen arena is nearly the dump's native shape. Streaming during
scan: on "directory enumerated", the dump thread reads the frozen child
run — already contiguous and raw-byte-sorted (Tier 1 by construction) —
and emits the block into ~512 KiB zstd frames; completion order →
`ordered:false`, killed mid-scan leaves valid frames. Ordered finalize:
DFS preorder over the frozen structure (siblings pre-sorted → preorder is
just following runs), `d` lines with ta/td/tn/te straight from the final
dir table — already hardlink-attributed — plus `x` index, seek table,
`e`. No re-sort, no re-aggregation, no second stat pass.

## 9. Watch mode & filter

- **Watch**: inotify events enter the same builder channel:
  "re-enumerate D" → fresh sorted batch → builder appends a **new** child
  run (old tombstoned), computes signed deltas (subtree totals are in the
  dir table), applies up the chain, re-checks hardlink ownership,
  publishes next epoch. Same code path, deltas merely signed. Garbage
  bounded by churn × session length.
- **Filter**: the killer argument — a filter is a rayon fold over the
  frozen columnar arena producing an *alternate dir-aggregate table*
  (~0.2 s single-threaded, <100 ms on 8 cores), published as a snapshot
  over the *same* structure. Filtered views, diff view, per-owner views
  are all alternate 40 MB aggregate generations over one shared 300 MB
  structure.

## 10. Quantified budget (10M entries, ~800k dirs)

| component | size |
|---|---|
| node arena 28 B SoA | 280 MB |
| names (avg 13 B, ~2× dedup) | ~70 MB |
| dir table 48 B × 800k | ~40 MB |
| hardlink registry | ~5 MB |
| nlink side-map, channels, buffers | ~20 MB |
| snapshot CoW overhead (≤2 dirty-sets) | 10–30 MB typ, 80 MB hard worst |
| **peak RSS** | **~430 MB typ, ~500 MB worst** |

Snapshot cost: typically 20–60 of ~200 chunks dirty per epoch = 4–12 MB
copied, 1–3 ms (worst all-dirty 40 MB ≈ 5–8 ms) — ≤8 % of the builder,
≤400 MB/s traffic at 10 Hz. `--no-ui` mode: zero snapshot overhead.
Honest miss vs 300 MB target: ~130 MB, of which only ~30–80 MB is this
design's overhead — the rest is metadata any design carries. Shave:
drop in-RAM mtime when age features off (−40 MB).

## 11. Crates

`crossbeam-deque`, `crossbeam-channel`, `arc-swap`, `rustix`, `io-uring`
(runtime-detected), `rustc-hash`, `rayon` (re-aggregation only), `zstd`
(+ hand-rolled seekable framing), `ratatui`/`crossterm`, `notify`/raw
inotify later. Absent: dashmap, im/imbl, left-right, triple_buffer,
tokio.

## 12. Honest weaknesses

1. **Single-builder ceiling** (~2–3M entries/s, unbenchmarked) — worst-
   served requirement at the extremes, comfortably met at 1M/s; sharded
   builders forfeit simplicity.
2. **~130 MB over target** — mostly intrinsic metadata cost.
3. **10 Hz aggregate granularity** — per-dir numbers tick 10×/s (masked
   globally by the striped side channel).
4. **Pathological wide dirs**: 10–20 ms re-sort per epoch, mitigated not
   eliminated.
5. **Hardlink owner switches break monotonicity** — correct per dump
   rule, mildly surprising on screen.
6. **Watch-mode garbage** in the append-only arena under heavy churn —
   session-scoped, aligned with the no-daemon roadmap.

Quietly load-bearing decision: **whole-directory batches** — they make
child runs contiguous and immutable (cheap snapshots), pre-sortable
off-thread (dump Tier 1 free), completion trivial, and message rate low.
Incremental partial-directory delivery would rework several properties
at once — that is the joint this design bets on.
