# Option A — "JSONL/zstd" (text-first angle)

> Proposal produced by a design agent instructed to push the text-first /
> interoperability angle as hard as it honestly can. Not a decision.

## 1. Pitch

One entry per line of JSON, grouped into self-contained per-directory blocks,
emitted through a two-phase writer that turns a chaotic multithreaded scan
into a **path-sorted stream** — the mtree trick that makes diffing two
10M-entry dumps a constant-window merge-join instead of a 1 GB double-load.
The artifact is that text stream inside a standard **zstd seekable-frame**
container, so `zstdcat dump.cmbt | jq` works on any machine with stock tools,
every complete line survives a `kill -9`, schema evolution is "add a key",
and ncdu import is a field-renaming exercise. You pay ~2× parse cost and
~1.5× raw bytes versus a binary format, and in exchange every future consumer
— the diff engine, the HTML exporter, the ssh transfer, a user's five-line
Python script, a bug report — reads the same trivially-debuggable stream.

## 2. File layout

```
dump.cmbt  =  zstd seekable stream (independent ~512 KiB frames + standard
              seek table in a skippable frame at the end; plain `zstd -d`
              and `zstdcat` decode it unchanged)

decompressed content, in order:
  1 header line          {"t":"h", ...}                    (always first)
  N directory blocks     one {"t":"d"} line + optional {"t":"s"} line
                         + child entry lines (sorted by name)
  F index lines          {"t":"x", ...}  one per frame     (optional)
  1 end line             {"t":"e", ...}  (clean-completion marker)
```

Two on-disk states of the same grammar, distinguished by a header flag:

- `"ordered":true` — final artifact: blocks in DFS path-sorted order,
  directory aggregates filled in, index + end line present.
- `"ordered":false` — live stream: blocks in scan-completion order, no
  aggregates, no index. This is what goes over an ssh pipe
  (`camembert -o -`) and what the `.part` temp file contains during a scan.
  Same parser reads both.

Key property: **every block is independently interpretable given only the
header.** Field defaults reset at each `d` line; the `d` line carries the
full path. You can start reading at any frame boundary and resynchronize at
the next `d` line.

## 3. Record schema

All lines are JSON objects. `t` selects the record type; **absent `t` =
file-system entry**, the overwhelmingly common case, saving 8 bytes/line.
Unknown keys and unknown `t` values must be ignored/skipped (see §8).

**Name encoding** (solves non-UTF-8): names are raw filename bytes with any
byte not part of a valid UTF-8 sequence emitted as `%XX`, and literal `%` as
`%25`. Deterministic, reversible, and the sort key is defined on this
encoded form, so writer and differ always agree. Newlines/quotes in
filenames are handled by ordinary JSON string escaping — serde_json never
emits a raw newline, so line integrity is structural, not hoped-for (the
classic TSV failure mode).

Entry line keys:

| key | meaning | presence |
|---|---|---|
| `n` | encoded name | always |
| `a` | apparent size, `st_size` | always |
| `d` | disk size, `st_blocks*512`, bytes | always |
| `m` | mtime, unix seconds | always (ns later, additive `mn`) |
| `u` `g` | uid, gid | extended mode; elided when equal to block default |
| `p` | mode bits (`st_mode & 07777`, decimal) | extended mode; elidable |
| `k` | kind: `l` symlink, `b` `c` block/char dev, `f` fifo, `s` socket | omitted for regular files |
| `i` `l` | inode, nlink | only when `nlink > 1`; header flag `"allino":true` forces `i` everywhere |
| `dev` | device id | only when ≠ block default |
| `err` | `true` = stat/read failed | on error |
| `ex` | `"pattern"` `"otherfs"` `"kernfs"` `"frmlink"` | when excluded (ncdu-compatible enum) |

Record types:

```jsonl
{"t":"h","format":"camembert-dump","v":1,"minor":0,"prog":"camembert","progver":"0.3.0","ts":1753142400,"root":"/var","dev":64769,"sem":"blocks","ext":true,"ordered":true}
{"t":"d","path":"/var/log/nginx","a":4096,"d":4096,"m":1753100000,"u":0,"g":4,"p":493,"nf":12,"nd":1,"ta":104857600,"td":105906176,"tn":14}
{"t":"s","u":33,"g":4,"p":420}
{"n":"access.log","a":9437184,"d":9441280,"m":1753141000}
{"n":"access.log.1","a":52428800,"d":52432896,"m":1753055000}
{"n":"error.log","a":1024,"d":4096,"m":1753141200,"u":0}
{"n":"latest","k":"l","a":11,"d":0,"m":1740000000,"u":0}
{"n":"rotated%FF.gz","a":1048576,"d":1052672,"m":1750000000,"i":393221,"l":2}
{"t":"d","path":"/var/log/private","a":4096,"d":4096,"m":1690000000,"err":true,"nf":0,"nd":0,"ta":4096,"td":4096,"tn":1}
{"t":"x","f":412,"p":"/var/log/nginx"}
{"t":"e","entries":10000000,"dirs":947213,"errors":340,"ta":1893459827345,"td":1912345678901,"elapsed":41.2}
```

`d`-line specifics: `nf`/`nd` = direct file/subdir counts, `ta`/`td`/`tn` =
cumulative apparent/disk/inode-count for the subtree (the per-directory
inode counter the HANDOFF asks for). Subdirectories are **not** repeated as
child lines in the parent block — they exist only as their own `d` line;
parenthood is implied by the path. `s` lines set block-local defaults
(writer typically uses the modal uid/gid/mode of the block; outliers carry
explicit fields, like mtree's `/set`).

## 4. Ordering guarantees

Two tiers, precisely stated:

- **Tier 1 — always true, even in a killed writer's output**: each directory
  block is contiguous, and child lines within a block are sorted by
  encoded-name bytes.
- **Tier 2 — true iff `"ordered":true`**: `d` lines appear in DFS preorder
  with siblings sorted by encoded-name bytes. Equivalently, `d` lines are
  totally ordered by **path compared component-wise** (split on `/`, compare
  components as byte strings). Component-wise is mandatory, not whole-string
  bytewise — `foo.bar` vs `foo/x` would otherwise misorder because `.`
  (0x2E) < `/` (0x2F). The differ uses the same comparator.

How a multithreaded writer gets there: **it doesn't try to during the
scan.** Threads emit blocks in completion order (a child directory can
legitimately complete before its parent). Sorting happens in a finalize pass
(§5). Over a pipe, where no second pass is possible, the stream ships
`"ordered":false` and the *receiver* finalizes — which is exactly the
remote-scan topology anyway.

## 5. Write path and its memory bound

**Phase 1 — scan.** Each worker thread buffers only the *direct children* of
directories it currently has open. When a directory's listing is fully
stat-ed, the thread sorts the children by name (tier 1), serializes the
block, and sends it over a `crossbeam` channel to one writer thread, which
appends it to `dump.cmbt.part` (zstd frames flushed at ≥512 KiB boundaries)
and records `(name, parent_idx, offset, len, agg)` in an in-memory directory
table.

Memory bound during scan:
- In-flight child buffers: O(threads × in-flight-dir width). Pathological
  mega-directory (5M entries ≈ 350 MB of lines): spill threshold at ~100k
  children → sort-run to temp, merge on emission. External-sort classic;
  bounded at a few tens of MB per in-flight dir.
- Directory table: ~1M dirs at 10M entries × ≈40 B ≈ **40 MB**. Entries
  never held; only dirs. (When embedded in the TUI, this table is the scan
  arena the engine keeps anyway.)

**Phase 2 — finalize** (skipped when writing to a pipe). DFS over the
directory table with siblings name-sorted; for each dir, re-emit its block
from `.part` into the final seekable stream: decompress the block, patch the
`d` line with the now-known `ta`/`td`/`tn` aggregates, recompress. Emit `x`
index lines (frame ordinal → first path) and the `e` line, write the seek
table, rename over the target, delete `.part`. Cost: one extra read+write of
the dump (~2×130 MB compressed I/O — a couple of seconds on NVMe), zero
extra per-entry memory.

## 6. Read paths

**Full load** (cache reopen, TUI): frames are independent → decompress and
parse frames **in parallel**, reassemble by frame ordinal into the arena.
10M entries ≈ 700 MB of JSON at ~500 MB/s–1 GB/s per core (simd-json) →
~1–3 s wall on 4+ cores.

**Lazy browse** (tree bigger than RAM): load header + seek table + `x` lines
(~6k entries at 512 KiB frames). Locating any path = binary search over `x`
first-paths (component-wise comparator) → decompress one frame → scan to the
`d` line. Because the file is DFS-sorted, a subtree is a **contiguous frame
range**, and the `d` line's `ta`/`td`/`tn` aggregates mean rendering a
directory listing never requires descending. Memory: O(frames currently
open) ≈ a few MB.

**Streaming diff** (the payoff): two readers yield `d` blocks in the same
total order → merge-join on path (component-wise); within two matched
blocks, child lines are name-sorted on both sides → inner line-level
merge-join, no block buffering needed. Classification per entry:
added/removed/grown/shrunk/touched (mtime), type-changed. Deltas propagate
up a DFS stack (a dir's subtree finishes contiguously); only dirs with
nonzero delta are retained for the output tree. Hardlink correctness:
per-side seen-set of `(dev,ino)` for `nlink>1` entries only.

Memory bound: O(path depth) stack + O(changed dirs) output tree +
O(hardlinked inodes) seen-sets — **never O(entries)**. Typical incident diff:
10–30 MB peak. Absolute worst case (all 10M entries changed, 1M dirs, 1M
hardlinked inodes): ~100–150 MB. Versus ≥1 GB for load-both. Wall time ≈
decompress 2×130 MB + parse 2×700 MB ≈ 3–6 s single-core, parallelizable by
partitioning the path space with the `x` indexes.

An `"ordered":false` dump can't be merge-joined directly;
`camembert dump sort` (the finalize pass run standalone, external-sort if no
dir table) upgrades it first.

## 7. Crash tolerance

- Writer dies mid-scan → `dump.cmbt.part` remains: a valid `"ordered":false`
  stream up to the last flushed frame (zstd frame checksums detect the torn
  tail; drop back to the last complete line). Every fully-listed directory
  is present with tier-1 guarantees. `camembert dump repair` re-sorts it and
  computes aggregates, marking the result incomplete. **Presence of the
  trailing `e` line is the clean-completion marker** — the header can't
  know, it's written first.
- Killed during finalize → `.part` still intact, final written via rename;
  you never lose the scan.
- Strictly better than both ncdu formats: JSON-tree dies on unbalanced
  brackets, binary dies on the missing last-position index.

## 8. Versioning & evolution

Header carries `"v":1,"minor":0`. Rules, in force from day one:

1. Readers **must ignore unknown object keys** (new per-entry fields = minor
   bump: e.g. `mn` mtime-ns, `bt` btrfs shared-extent bytes, `at` atime).
2. Readers **must skip lines with unknown `t`** (new record types = minor
   bump: e.g. a future `q` quota record).
3. Unknown `ex`/`k` enum values are preserved as opaque strings.
4. Major bump only for changes to the comparator, the name encoding, or line
   framing — things that would silently corrupt a diff.
5. Capability booleans in the header (`ext`, `ordered`, `allino`) rather
   than inferring from data.

**ncdu import** stays an afternoon: ncdu's infoblock fields map 1:1
(`asize→a`, `dsize→d`, `ino/nlink→i/l`, `read_error→err`, `excluded→ex`,
extended `uid/gid/mode/mtime→u/g/p/m`), ncdu's nested JSON already arrives
in DFS order — pull-parse it (SAX), sort siblings per dir, run the same
phase-2 writer. Export back to ncdu JSON is the same table reversed, which
also gives gdu interop for free.

## 9. Numbers (10M entries, ~1M dirs)

| quantity | estimate | basis |
|---|---|---|
| file line, raw | ~55–65 B | name ~15 + keys/syntax ~28 + numbers ~18; matches 60–70 B/entry measured for ncdu JSON, extended fields mostly elided via `s` lines |
| `d` line, raw | ~150–170 B | full path ~50 + aggregates |
| **dump, raw** | **≈ 650–800 MB** | 9M × 60 + 1M × 160 |
| **dump, zstd -3 seekable** | **≈ 100–150 MB** (10–15 B/entry) | 10 B/entry gzipped for basic ncdu JSON; extended fields and mtime entropy add a few B |
| writer RAM during scan | ~40 MB dir table + in-flight blocks | §5 |
| finalize cost | one rewrite, ~2×130 MB compressed I/O | §5 |
| full load | 1–3 s (parallel frames + simd-json) | §6 |
| **diff peak RSS** | **10–30 MB typical, ~100–150 MB worst case** | O(changed dirs + hardlinked inodes) |
| diff wall time | 3–6 s single core | decompress + parse dominated |

For scale: the binary strawman is 44–48 B/entry raw (≈450 MB) — raw text is
~1.6× that, but *compressed on disk and on the wire* the gap mostly closes.

## 10. Rust crates

- **Write**: hand-rolled line emitter with `itoa` (+ `memchr` for the name
  encoder); `serde_json` only as the debug/reference path. `zstd` for
  frames; `zeekstd` (pure Rust) or `zstd-seekable` for the seek table.
  `crossbeam-channel` for block hand-off; `tempfile` for `.part` and spill
  runs.
- **Read**: `serde_json` (`&RawValue`/borrowed) baseline, `simd-json` behind
  a feature flag for hot paths; `memchr` for line splitting.
- **ncdu import**: `struson` (streaming pull parser).
- Zero-crate interop check in CI: `zstdcat dump.cmbt | jq -s 'length'` and
  `... | jq -c 'select(.t=="d" and .td>1e9)'` must work.

## 11. Honest weaknesses

1. **Worst-served requirement: lazy browse of bigger-than-RAM trees.** Works,
   but binary search over an index of *frames* plus decompress-and-scan per
   jump; text is the wrong substrate for duc-territory (500M+ persistent
   index).
2. **Parse tax.** ~700 MB of JSON per side per diff/load; a mmap-able binary
   would be ~10× faster to open.
3. **Finalize is a full rewrite.** ~2× write volume, multi-second pause at
   scan end (worse on HDD). Pipes skip it by shipping unordered.
4. **Crashed output is degraded, not just truncated**: unordered, no
   aggregates; diffing requires an explicit repair/sort step.
5. **Names are transformed.** Percent-encoding is deterministic and
   reversible, but the sort key is defined on the encoded form — one page of
   spec others must implement exactly to interoperate on diffs.
6. **Raw size** (~750 MB uncompressed at 10M) makes compression effectively
   mandatory.
7. mtime is seconds-granularity in v1; nanosecond additions are additive but
   cross-version diffs need care with the "touched" classification.
