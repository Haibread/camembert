# Research digest: concurrent tree for UI-navigable-during-scan

Factual input for HANDOFF §5 "UI navigable pendant le scan" / open question
§7.2. Facts with sources, no recommendations. Gathered 2026-07-22.

## 1. How existing tools handle it

- **ncdu 2 (Zig)**: multithreaded scan (work-stealing LIFO queue of dirs);
  all tree mutation goes through a **"sink" abstraction** owning the
  locking; `Dir` has `parent` pointer, aggregates incrementally updated
  (saturating arithmetic); explicit mutexes on the device-id and
  hardlink/inode tables; thread 0 pumps the UI event loop during scan.
  **Unconfirmed** whether shipped ncdu2 allows interactive *browsing*
  mid-scan — the author is on record that updating the browser's cache on
  each UI draw "is going to be too expensive, not sure how to handle it."
  (dev.yorhel.nl/doc/ncdu2; model.zig/scan.zig read directly)
- **dua-cli (Rust)**: tree = `petgraph::StableGraph<EntryData, ()>`; a
  dedicated walk thread sends `TraversalEvent`s over a **bounded
  crossbeam channel (cap 100)**; the **UI thread is the sole mutator**
  (`integrate_traversal_event()`). Interactive mode is usable while
  scanning. History note: the redraw loop was once coupled to
  channel-receive → multi-second UI stalls; later decoupled.
  (src/traverse.rs read directly)
- **gdu (Go)**: PR #563 **replaced channel-based progress reporting with
  atomic counters + ticker polling** — channel push from many goroutines
  created contention; polling shared atomics on a timer reduced cost.
  Real-world precedent for "poll atomics, don't push per-update".
  (github.com/dundee/gdu/pull/563 — paraphrase, diff not read)
- **diskonaut (Rust)**: scan thread + UI thread + input thread,
  `mpsc::sync_channel` of `Instruction` events; internal tree structure
  **not confirmed**.
- **WizTree/Everything**: MFT/USN model — bulk parse then build; no
  concurrent scan-vs-UI problem; not applicable to Linux-first no-daemon.
- **Pattern**: every tool with a live UI converges on **single-owner-thread
  mutates the tree; workers communicate via channel or shared atomics; UI
  reads from the owner's data or polls atomics**. None uses DashMap,
  arc-swap, im, or per-node locks for the live tree.

## 2. Rust primitives

| Primitive | Maturity | Notes |
|---|---|---|
| crossbeam-channel | very mature | ~700k msgs/s past ~4 senders in third-party bench (indicative, not authoritative); degrades past ~20 senders |
| dashmap | mature | sharded RwLock map; fine for flat id→data, does nothing for ancestor-chain aggregation |
| papaya / flurry | papaya newer, active; flurry has known perf/memory issues | read-optimized concurrent maps |
| AtomicU64 | std | see §3 |
| arc-swap | mature | lock-free reads; docs deliberately publish **no numbers** (machine-dependent) |
| left-right | **not verified** in this pass | single-writer double-buffer + op-log replay |
| triple_buffer | published | wait-free snapshot hand-off, qualitative claims only |
| im / imbl | im maintenance status **unverified (2026)**; imbl is the maintained fork | persistent structures → cheap snapshots |
| crossbeam-epoch | mature | building block; crossbeam's own advice: prefer pre-built structures |
| LongAdder equivalent | **no canonical crate** — only low-visibility `contatori` (64 padded slots, thread-local) | striped counters are a pattern, not a solved crate |

## 3. Aggregate-propagation cost (the crux)

From Travis Downs, "A Concurrency Cost Hierarchy" (fetched directly):

- Uncontended atomic add: **~7 ns**.
- Contended (2 cores, same line): **~110 ns** add, ~150+ ns CAS;
  `std::mutex` ~125 ns. Worse with more cores. Relaxed ordering does
  **not** help — the RMW still needs exclusive cache-line ownership
  (~70 cycles minimum coherence round-trip).
- **Striped/multi-counter**: ~9× speedup at 4 cores, 80× at 16 threads
  (Graviton) vs a single shared atomic — empirical confirmation of the
  LongAdder pattern.
- Implication (inference, not measured for this workload): naive per-file
  ancestor-chain fetch_add makes the root the maximally-contended line —
  order of ~1–1.5 s of pure coherence overhead per 10M entries at ≥2–4
  threads, before any real work.
- Alternatives documented in the wild: gdu's atomics+poll; per-thread
  deltas merged periodically (perthread crate, Rayon fold/reduce);
  **subtree-completion-time aggregation** (bubble up only when a dir
  finishes) — **no published prior art found as a named pattern**; striped
  counters (confirmed technique, no dominant crate).

## 4. TUI render loop (ratatui)

- Immediate mode: full logical redraw per frame into a cell buffer,
  diffed against the previous frame; only changed cells hit the terminal.
- Typical cadence: 20–30 fps (app-chosen poll timeout, not mandated).
- A redraw needs: ~50 visible rows of (name, size, kind) for the current
  dir + aggregate totals + sort. (Derived from HANDOFF framing, not
  sourced.)
- **Snapshot consistency across siblings: no prior art discusses it.**
  Reasoned (unsourced): sizes are monotonically increasing during a scan;
  a torn read across siblings = some rows one frame staler — self-corrects
  next frame. Progress displays likely need per-value freshness, not
  cross-sibling consistency.

## 5. Memory numbers

- HANDOFF target: arena `Vec<Node>` + u32 indices + interning ≈ 300 MB @
  10M files (project's own figure).
- Padded per-node atomic = 64 B cache line → 10M × 64 B = **640 MB of
  padding alone: catastrophic**. Un-padded packed AtomicU64 = 8/16 B but
  shares cache lines with arena neighbors → false-sharing risk during
  concurrent writes. **No prior art found resolving this tension for a
  Vec-arena tree.**
- Interning: lasso (`ThreadedRodeo` multi-thread), string-interner
  (pluggable backends), or custom bump arena. A "145 % memory overhead"
  figure surfaced attributed to *both* lasso and string-interner —
  suspicious, unverified.

## 6. Watch-mode compatibility (future)

- No researched tool implements inotify point-updates + re-aggregation.
- Single-owner + channel is the most naturally compatible (watcher thread
  feeds the same event channel; the single writer re-aggregates the
  affected chain). Reasoned inference, not built anywhere.
- Arena + u32 indices supports point updates given tombstoning for
  deletions; nothing disk-tool-specific found.

## Not confirmed (explicit flags)

1. ncdu2 interactive browsing mid-scan (contradictory signals).
2. diskonaut's tree structure.
3. left-right numbers/maturity.
4. im crate 2026 maintenance status.
5. Canonical Rust LongAdder crate (none found).
6. Subtree-completion aggregation as documented prior art (may be novel).
7. Snapshot-consistency requirements for progress displays (inferential).
8. The lasso/string-interner 145 % figure.
9. Any scanner-specific benchmark of contended root fetch_add (estimate is
   back-of-envelope).
10. gdu PR #563 exact code (paraphrase only).
