# Option B — "CAMB1" (custom binary, performance-first angle)

> Proposal produced by a design agent instructed to push the purpose-built
> binary angle as hard as it honestly can. Not a decision.

## 1. Pitch

CAMB1 is an append-only stream of self-describing, CRC-protected,
zstd-compressed frames, where the atomic unit is the **directory entry block
(DEB)**: one directory's children, statted, name-sorted, front-coded, and
varint-packed at ~25 bytes/entry before compression. Directories get
monotonically-assigned IDs at discovery time, so a parent can reference
children it hasn't written yet, and scan threads emit finished directories in
whatever order they complete — zero coordination beyond an append lock held
once per ~4 MiB. Every frame is independently decodable and length-prefixed,
so a `kill -9` at any byte offset loses at most one torn frame plus unflushed
thread buffers; the tree written so far is fully recoverable by a sequential
header walk that costs milliseconds — precisely the flaw that makes ncdu
2.6's index-written-last format unrecoverable. A clean finish appends a
dir-ID index, a flat mmap-able per-directory summary table, and a sorted
hardlink table — but here they are an *accelerator*, never the *only* way
in. Per-directory sorted children turn diff into a synchronized DFS
merge-join with O(depth) open cursors: two 10M-entry dumps diff in well
under 200 MB RSS, and a 50M-entry dump browses lazily in ~150 MB.

## 2. File layout

```
offset 0
┌────────────────────────────────────────────────────────────────────┐
│ FILE HEADER (uncompressed, 64–512 B)                               │
│   magic "\x89CMB\r\n\x1a\n" (8B, PNG-style: catches ASCII-mode     │
│   transfer mangling — this file gets scp'd around)                 │
│   format_major u16 = 1 | format_minor u16 | min_reader_minor u16   │
│   feature_mask u64  (which optional field groups this file uses)   │
│   meta: CBOR map (root path, tool version, scan timestamp, size    │
│         semantics, cross-fs policy, zstd window cap) — ignore-     │
│         unknown-keys on read                                       │
│   crc32c of header u32                                             │
├────────────────────────────────────────────────────────────────────┤
│ FRAME STREAM (append order = completion order, NOT tree order)     │
│                                                                    │
│  ┌ frame header (20 B, uncompressed) ┐                             │
│  │ fmagic "CmbF" | type u8 | flags u8 | rsvd u16                   │
│  │ clen u32 | ulen u32 | crc32c(payload) u32                       │
│  └ payload: one zstd frame ──────────┘                             │
│                                                                    │
│  type 0  DATA      — 1..n DEBs, ~4 MiB uncompressed target         │
│  type 1  SUMMARY   — dir-completion records, in completion order   │
│  type 2  INDEX-CKPT— incremental dir_id→offset index, every 64 MiB │
│  type ≥8 (future)  — skippable: length-prefixed, unknown = skip    │
├────────────────────────────────────────────────────────────────────┤
│ FINALIZE SECTION (present only after a clean scan end)             │
│  type 3  SUMMARY-TABLE — flat fixed-40B rows by dir_id,            │
│                          stored UNCOMPRESSED for mmap random access│
│  type 4  HARDLINK-TABLE— sorted (dev,ino)→(nlink_seen, blocks)     │
│  type 5  FINAL-INDEX   — dir_id → (frame_offset, offset_in_frame), │
│                          delta-varint, ~5 B/dir                    │
├────────────────────────────────────────────────────────────────────┤
│ TRAILER (fixed 48 B): tmagic | offsets of frames 3/4/5 |           │
│   frame_count | total_entries | CLEAN flag | crc32c                │
└────────────────────────────────────────────────────────────────────┘
```

Key inversion vs ncdu binfmt: the tail structures are a cache of information
that already exists, recoverably, in the frame stream. ncdu's index is
load-bearing; ours is a fast path.

## 3. Record schema

**DEB** (one per directory, contiguous within one frame; a frame may exceed
the 4 MiB target to keep a giant DEB whole — `clen`/`ulen` are u32, so up to
4 GiB):

```
DEB header
  dir_id        varint     ID of this directory (root = 0)
  parent_id     varint     redundant with parent's record — recovery seam
  dflags        u8         bit0 has_extended, bit1 dev_changed,
                           bit2 read_error, bit3 partial, rest rsvd
  dev_idx       varint     only if dev_changed; index into frame dev table
  mtime_base    zigzag-varint  epoch seconds; children delta against this
  self asize    varint     the directory inode's own st_size
  self dsize    varint     st_blocks (512 B units)
  [ext] uid_idx, gid_idx, mode  varints (per-frame uid/gid tables)
  n_children    varint
  n_restarts    varint + restart offsets (varint deltas): a restart
                every 16 children stores the full name (front-coding
                reset) → mid-DEB resume and binary search by name

Child record (sorted bytewise ascending by name), per entry:
  kflags   u8    kind: 3 bits (file / dir / symlink / other / excluded-stub)
                 bit3 has_ext, bit4 hardlink, bit5 error,
                 bit6 has_ext2 (future TLV block), bit7 rsvd
  name     prefix_len varint + suffix_len varint + suffix bytes
                 raw bytes, ≤ 32 KiB, non-UTF-8 fine
  if dir:  child_dir_id  varint delta vs previous child dir in this DEB
  asize    varint                                   (~3 B typical)
  dsize    varint, 512-byte block units             (~2 B typical)
  mtime    zigzag-varint delta vs mtime_base        (~2–3 B typical)
  [has_ext]  uid_idx varint, gid_idx varint, mode varint   (~3–4 B)
  [hardlink] ino varint (~4–5 B), nlink varint (1–2 B)  — only nlink>1
  [error]    reason u8: perm / io / pattern-excluded / otherfs /
             kernfs / frmlink  — superset of ncdu's excluded enum
  [has_ext2] len varint + opaque bytes  — old readers skip; future fields
```

Frame prologue tables (per DATA frame): uid table, gid table, dev table.
Records store 1-byte indices instead of 4–8-byte values.

**SUMMARY record** (stream form, varint; table form, fixed 40 B row indexed
by dir_id):

```
dir_id | sub_asize | sub_dsize | sub_items (subtree inode count) |
sub_dirs | hlnk_dsize (blocks of nlink>1 entries, raw sum — dedup is the
reader's job via the hardlink table) | err_count | flags (complete/partial)
```

Typical cost per child record, extended mode on: ≈ **25–26 B/entry
uncompressed** (kflags 1 + name ~11 front-coded + asize 3 + dsize 2 +
mtime 3 + uid/gid/mode 3–4 + amortized overhead ~2); hardlinked entries
+6–7; dirs +2. Compare the naive fixed record at 44–48 B and ncdu's CBOR
maps at ~35–45 B.

## 4. Ordering guarantees and the multithreaded writer

The format guarantees exactly four things:

- **G1** — within a DEB, children are sorted bytewise ascending by name.
- **G2** — each directory appears as exactly one contiguous DEB.
- **G3** — `dir_id` is assigned at *discovery* (enqueue) time from an atomic
  counter, so every parent's ID is strictly less than its children's. IDs
  are dense: 0..n_dirs.
- **G4** — no ordering whatsoever between frames or DEBs across frames.
  Completion order is nondeterministic and that is fine.

Why not mtree-style global path-sorted emission? Because it's impossible for
a streaming multithreaded writer — you'd need the whole tree before the
first record (ncdu's JSON-exporter disease). Global sort is not needed:
G1+G3 give the diff the same O(1)-per-level merge-join, organized as a
synchronized DFS instead of a flat `comm(1)` pass.

G1 is free-ish: a thread that owns a directory has the complete child list
(one `getdents64` sweep + statx batch) before anything is emitted; one
`sort_unstable` on the name bytes and the DEB serializes in a single pass
out of the scan arena. Child directory IDs are known at that moment. No
thread ever waits on another thread's data.

## 5. Write path and its memory bound

```
worker thread (×N)                       writer thread (×1)
──────────────────                       ──────────────────
getdents64 + statx children              recv compressed frames
sort children by name                    append under one lock/queue
serialize DEB → thread-local buf         record (frame_off) per dir_id
buf ≥ 4 MiB? → zstd -3 compress          every 64 MiB: emit INDEX-CKPT
send frame ──────────────────────────►   batch summary msgs → SUMMARY frames
subtree complete? send summary ───────►  on scan end: finalize + trailer
```

Contention: one channel send per ~4 MiB of output — a few dozen messages/s
at 16 threads.

Memory bound (dump writer's overhead on top of the scan engine):
- N × (4 MiB serialize buffer + ~2 MiB zstd context) → 16 threads ≈ **96 MB**
- in-flight index: 8 B/dir → 1M dirs ≈ **8 MB**
- summary batching ≈ 1 MB

Total ≈ **~110 MB at 16 threads**, constant in entry count (linear only in
directories). Aggregates bubble up through the scan engine's pending-children
counters; the tree is never buffered.

## 6. Read paths

**Full load**: sequential frame walk, zstd-decompress-bound (~1–2 GB/s
logical); a 10M-entry dump loads in a few hundred ms of CPU.

**Lazy browse — 50M entries, 200 MB budget**: open trailer → load
FINAL-INDEX (~5 B/dir → 5M dirs ≈ 25–30 MB) → `mmap` the SUMMARY-TABLE
(200 MB on disk, only touched pages resident). Rendering a directory = index
lookup → decompress its frame (≤ 4 MiB typical) → walk the DEB; each child
dir's totals are one O(1) mmap read. Restart points give binary search by
name inside huge DEBs. Decompressed-DEB LRU capped at 64 MB. Peak RSS ≈
**~130–150 MB**. Only the index scales with size; 200 MB budget fails around
~300M directories.

**Streaming diff** (dumps A, B):

```
push (root_A, root_B)
loop: merge-join the two sorted child lists by name bytes:
  only in A → emit "deleted subtree" (stream A's subtree via its index)
  only in B → emit "added subtree"
  in both, both dirs → recurse: push cursor pair
  in both, leaf      → compare sizes/mtime/mode → Modified/Touched
```

G1 makes this a textbook per-level merge-join — one child record in flight
per side. Open cursors: O(tree depth) per side. Cursors over big DEBs use
streaming zstd decompression; paused ancestor cursors can be evicted and
resumed from the nearest restart point (re-decompression ~1 GB/s). Hardlink
dedup for "real freed bytes" is a separate merge-join of the two sorted
HARDLINK-TABLEs, O(1) memory.

Peak RSS, two 10M-entry dumps: 2 indexes ≈ 12 MB + cursor/DEB cache capped
at 96 MB + hardlink streams → **< 150 MB** (the cap is a config knob). Full
diff reads both files once: ~0.5 s of zstd CPU + I/O.

## 7. Crash tolerance — kill -9 at an arbitrary byte offset

On disk at all times: valid header + K complete frames + at most one torn
frame.

- **During header write**: no valid magic/CRC → file rejected; nothing lost.
- **Mid-frame**: trailer absent → recovery mode: sequential walk of 20 B
  frame headers, `seek(clen)` skips payloads — ~100 header reads per 200 MB,
  milliseconds; paranoid full-CRC verify ~0.2 s. Torn last frame fails
  length/CRC and is dropped. Garbage header → scan forward for `"CmbF"` +
  plausible lengths + CRC match to resync. Every complete DATA frame yields
  its DEBs; the last INDEX-CKPT covers all but the final ≤ 64 MiB.
- **Loss bound**: one torn frame (≤ 16 MiB worst, ~4 MiB typical) + unflushed
  thread buffers (≤ N × 4 MiB). Everything else survives with full fidelity.
- **Dangling references are detectable, not corrupting**: index miss → UI
  shows "unscanned (interrupted)"; ancestor summaries absent or
  `partial`-flagged → totals display "≥ X, incomplete" (fits the
  honest-stale-cache stance).
- **`camembert dump repair`** replays the walk once and appends the finalize
  section.
- **Power loss**: page-cache tearing caught by CRCs, same bound. Opt-in
  `--sync` fdatasyncs per index checkpoint.

JSONL-grade prefix validity at frame granularity, plus ncdu binfmt's
lazy-browse and multithreaded-write virtues.

## 8. Versioning & evolution

- `format_major` (breaking, refuse), `format_minor` (additive),
  `min_reader_minor` (writer declares the floor needed).
- Four additive mechanisms, cheapest first: (1) new header CBOR keys —
  ignore-unknown; (2) new frame types ≥ 8 — skip-unknown; (3) reserved
  `kflags`/`dflags` bits gated by `feature_mask` (reader knows *before
  parsing* whether it can decode); (4) per-record `has_ext2` TLV for new
  per-entry fields — old readers skip by length. Future additions costing a
  minor bump only: btrfs shared-extent bytes, atime, per-frame trained zstd
  dictionary.
- Golden-file corpus per minor version + structure-aware fuzzing of the
  reader in CI. A custom format's spec *is* the test suite; this is where
  the engineering budget goes.

## 9. Numbers

10M entries, ~1M directories, extended mode on:

| | uncompressed logical | on disk |
|---|---|---|
| DATA frames | ~255 MB (25.5 B/e) | ~150–165 MB (zstd -3) |
| SUMMARY stream | ~30 MB | ~15 MB |
| SUMMARY-TABLE | 40 MB | 40 MB (uncompressed, mmap) |
| FINAL-INDEX + ckpts | ~10 MB | ~8 MB |
| HARDLINK-TABLE | ~2–10 MB | few MB |
| **Total** | **~340 MB** | **~215–230 MB** |

Basic mode: ~21 B/e → ~175–185 MB on disk. Reference: ncdu JSON at 10M is
600–700 MB raw / ~100 MB gzipped *without* extended fields and with zero
random access. ~2.6× smaller raw than ncdu JSON with more fields; ~2× larger
than gzipped basic JSON, buying the index, the mmap table, and O(frame) lazy
access.

Diff of two 10M dumps: **peak RSS < 150 MB**, ~0.5 s zstd CPU + I/O. Browse
of 50M entries: **~130–150 MB RSS**.

## 10. Rust crates

- `zstd` (libzstd bindings; streaming contexts, one per thread) — `ruzstd`
  as pure-Rust read-only fallback.
- `crc32c` (hardware Castagnoli — checksumming is free).
- `memmap2` (summary table).
- `minicbor` (header metadata only).
- `crossbeam-channel` (workers → writer).
- Hand-rolled LEB128/zigzag (~30 lines).
- Deliberately absent: serde/bincode/postcard on the record path (no schema
  evolution, no layout control), capnp/flatbuffers (finalize-whole-buffer or
  padding tax; their evolution machinery duplicates frames+flags).
- Tests: `cargo-fuzz` structure-aware reader fuzzing, `proptest`
  round-trips, `insta` golden files.
- Tooling ships with v1: `camembert dump verify | cat | repair | stat` —
  `dump cat` streams ncdu-compatible JSON back out (the jq-replacement a
  binary format owes its users, and the HTML-export/third-party path).

## 11. Honest weaknesses

1. **Implementation cost is the real price.** Reader, writer, recovery walk,
   repair, restart points, fuzzing, a written spec: 3–5× the effort of
   JSONL+zstd. Format bugs are data-loss bugs.
2. **Requirement served worst: additive schema evolution.** It works, but
   every new field is a design decision (flag bit? TLV? new frame?) with
   hand-written encode/decode. Evolution is *engineered*, never *free*.
3. **Opaque without our tooling.** No `jq`, no `grep`. `dump cat` mitigates.
4. **Pathological directories.** A 10M-children directory makes a ~250 MB
   DEB; diff/browse fall back to streaming cursors and restart points —
   correct, bounded, nobody's favorite code path.
5. **Two-tier fast-open.** A clean dump opens O(1); a crashed one needs a
   recovery walk or `repair` before mmap-grade browsing.
6. **Summary table is uncompressed by design** (mmap): 40 MB per 1M dirs; at
   50M entries, 200 MB of table — mildly annoying to scp. A future
   compressed-table variant could serve the transfer case.
7. **Compressed size loses to gzipped ncdu JSON by ~2×** on basic fields
   (bytes spent on structure). A `--transfer` mode omitting the finalize
   section (regenerable by `repair` on arrival) claws most of it back.

Bottom line: beats ncdu 2.6's binary format on its own turf (smaller
records, same lock-free write, same lazy browse) while fixing its fatal
crash flaw, and is the only candidate family that makes the 10M×10M diff a
sub-200 MB streaming operation *by construction*. The price is engineering
effort and opacity — paid knowingly, because HANDOFF §4 makes this format
the piece everything else sits on.
