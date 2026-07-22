# Option A — single-owner thread (dua-cli lineage, pushed to scale)

> Proposal by a design agent instructed to push the single-owner-thread
> angle as hard as it honestly can. Not a decision.

## 1. Pitch

One thread owns a plain, non-concurrent `Vec<Node>` arena; nothing else
ever writes it. Scan workers ship pre-summed, per-directory batches over
one bounded MPSC channel; the owner integrates ~4k entries per message, so
the "700k msgs/s channel ceiling" becomes a ~250 msgs/s reality. Because
the owner is the sole writer, ancestor aggregation is plain `u64 +=` at
~1 ns — the contended-fetch_add problem is not mitigated, it is *deleted*.
The TUI never touches the arena: the owner publishes a copy-out snapshot
of exactly the directory being viewed (plus a prefetch of the highlighted
child) via `arc-swap` every 33 ms, so the render loop is wait-free and
navigation lands in under one frame. The whole owner budget at 1M
entries/s is ~5 % of one core — scales *down* to a 4-core VPS as well
as up.

## 2. Data layout

All state lives on the owner thread: no atomics, no padding.

```rust
// One per entry (files AND dirs). #[repr(C)], 32 bytes exactly.
struct Node {
    apparent: u64,        // st_size
    disk:     u64,        // st_blocks * 512
    name:     u32,        // interner id (28b) | kind (3b) | err flag (1b)
    parent:   u32,        // arena index; u32::MAX for root
    mtime:    u32,        // unix seconds, saturating
    aux:      u32,        // dirs: index into dirs[]; files: hardlink id
}
// Side table, one per directory (~5–10% of entries). 48 bytes.
struct DirMeta {
    children_start: u32, children_count: u32, // children CONTIGUOUS
    ta: u64, td: u64,                          // subtree totals (live)
    tn: u32, te: u32,                          // inodes / errors
    pending: u32,       // outstanding child dirs + 1 self token
    node: u32,
    state: u8, _pad: [u8; 7],
}
```

Key trick: the owner integrates a directory's whole listing as one batch →
**children are contiguous in the arena** — no sibling pointers (8 B/node
saved); a directory view is a slice. Watch-mode creations go to a small
per-dir overflow `Vec`.

Interning: owner-local bump arena + `hashbrown::RawTable` keyed by
(worker-precomputed FxHash, bytes) → u32. Single-threaded, no locking tax.

Budget @ 10M entries: nodes 320 MB + DirMeta 38 MB + interner ~75 MB +
hardlinks ~10 MB + buffers ~16 MB ≈ **~460 MB**. Packed 24-byte node
variant (u40 sizes + escape map, same accessor API) → **~380 MB**. Honest
position: 300 MB is reachable only with the packed node.

## 3. Thread/channel topology

```
        crossbeam-deque (work stealing: dirfd + parent ref)
   [worker 1..N]  N = 2–4× cores NVMe, 1–2 HDD
        │  openat/getdents64/statx; pre-sum batch totals, pre-hash names
        └──► bounded MPSC (cap 32 batches) ──► [OWNER] — sole arena writer
        ◄── buffer-recycle channel ─────────────┘ │ │
                                                  │ └─► [dump writer]
   [TUI] ◄── arc-swap<Arc<ViewSnapshot>> (≤33 ms) ┘
   [TUI] ──► nav-request cell (capacity-1, latest-wins) ──► [OWNER]
   [watcher (later)] ──► same MPSC as workers
```

Batching: coalesce small dirs, flush at 4096 entries or 4 ms → **~250
msgs/s** at 1M entries/s. Backpressure: 32-batch bound (~10 MB in
flight); if the owner lags, workers block on send — the UI cannot starve
because snapshot publication is clocked inside the owner loop.
Parent-before-child resolution via a dense batch-id → arena-base table;
an out-of-order holding map as tested safety net.

## 4. Aggregation

- **Live, per batch section**: workers pre-sum each dir section; the owner
  adds four sums up the whole ancestor chain — plain non-atomic adds.
  Cost O(depth) per *directory*: ~50k dirs/s × depth 12 × 4 fields ≈
  **2–3 ms CPU per wall-second**. Every ancestor has a monotonically
  growing, renderable total at all times.
- **Completion cascade** flips state bits only — totals already exact.

Root contention: **none exists**. The only shared atomics are batch-id
counter, deque and channel internals — per-directory frequency, never
per-entry. Owner integration ~50 ns/entry → **≤5 % of one core at 1M
entries/s**.

## 5. TUI read path

- **Owner**: every loop iteration (~1–2 ms bound), on 33 ms tick or nav
  request, builds `ViewSnapshot { generation, path, rows, dir_totals,
  scan_stats, prefetch }` by copying the viewed dir's contiguous slice
  (~48 B/row; 10k children → ~0.5 MB → 100–200 µs). Swap via arc-swap.
  `prefetch` = highlighted row's children (top ~200 by disk) so Enter
  renders instantly.
- **TUI (20–30 fps)**: wait-free load; if generation changed, re-sort a
  cached index permutation (10k rows ≈ 0.5 ms); render ~50 rows. Column
  changes and scrolling reuse cached rows. Navigation: capacity-1
  latest-wins cell, optimistic prefetch render.
- **Staleness**: typical ~35 ms, worst ~50 ms. Fanout >20k: that dir's
  publish degrades to 4 Hz (250 ms, displayed as "updating…"); the UI
  itself never drops frames — the render loop performs no blocking
  operation (the structural fix for dua-cli's historical stalls).

## 6. Subtree completion

`pending = (# child dirs discovered) + 1 self token`; self token drops
when the final listing section integrates; child completion decrements
parent; 0 → Complete, fire hook, cascade. Exact, O(1) per completion.
Root completion = scan done.

## 7. Hardlink registry

Owner-local `HashMap<(dev_idx, ino), HardlinkRec>` — only nlink>1
(~10 MB @ 200k inodes). Live totals use **first-seen attribution**
(monotonic, no flicker); at scan end a **correction pass** re-attributes
inodes whose canonical owner (smallest path, raw-byte comparator —
dump §8) ≠ first-seen (subtract old chain, add new — ms total). Yields
exact dump-v1 semantics before ordered totals are written. `links_seen <
nlink` retained per inode — feeds the future "libérable" column.

## 8. Dump-writer integration

- **Streaming** (`-o`, pipes): on dir completion the owner copies the
  block to the dump thread, which sorts children by raw bytes and emits
  Tier-1 `ordered:false` blocks. One writer thread absorbs 1M entries/s;
  a second compression thread is the escape hatch.
- **Ordered finalize**: after root completion + hardlink correction, DFS
  over the arena (children contiguous; sort child slices, sub-second for
  10M), emit `d` preorder with exact totals, `x` index, `e`, seek table,
  `.part` + rename.

## 9. Watch mode & filter

- **Watch**: watcher feeds `PointUpdate/Removed/Created` into the same
  MPSC. Owner applies signed deltas up the chain — identical machinery.
  Removals tombstone (arena/interner leak until compaction — acceptable
  for a session). Zero new mechanism.
- **Filter**: sequential arena pass into a second totals slot; glob
  evaluated once per unique interned name (memoized bitmap) → ~100 ms
  for 10M, chunked at 4 ms slices interleaved with publication.

## 10. Quantified budget

| Metric | Value |
|---|---|
| Bytes/entry | ~43 B (MVP) / ~35 B packed |
| RSS @ 10M | ~460 MB / ~380 MB packed vs 300 MB target — honest miss |
| Owner CPU @ 1M/s | ≤5 % core integration + ~3 ms/s aggregation + <1 % snapshots |
| Channel | ~250 msgs/s, 10 MB in flight |
| UI staleness | 35 ms typ / 50 ms worst; >20k-child dirs 250 ms |
| Contended atomics per-entry | **zero** |

## 11. Crates

`crossbeam-deque`, `crossbeam-channel`, `arc-swap`, `hashbrown` +
`rustc-hash`, `rustix`, `io-uring` (runtime-detected), `ratatui` +
`crossterm`, `zstd` (seekable framing hand-rolled), `smallvec`, `memchr`.
No tokio, no DashMap, no per-node locks.

## 12. Honest weaknesses

1. **Every byte is copied twice** (worker buffer → channel → arena).
   ~40 MB/s at peak — cheap but structural.
2. **Snapshot cost scales with viewed fanout**; 4 Hz degradation is a
   policy, not a solution; million-child dirs want an incremental
   row-diff protocol (deferred).
3. **Memory misses 300 MB** until the packed node lands — worst-served
   requirement.
4. **Cross-producer FIFO assumption** (parent batch before child) must be
   loom/stress-tested, not trusted.
5. **The owner serializes everything** — filter passes and finalize must
   be chunked; a multi-view GUI multiplies the snapshot protocol.
6. Live hardlink attribution is first-seen until scan end — totals shift
   slightly at the correction pass (≪1 % typically).
