# Adversarial review — Option A "JSONL/zstd"

> Produced by an adversarial agent instructed to demolish the proposal.
> Verdict: **VIABLE WITH FIXES**.

## 1. Quantitative claims — mostly honest, two soft spots

Arithmetic rebuilt from the proposal's own example lines (byte-exact):

- **Raw byte/entry**: basic file line = **58 B** (+nl), matches "55–65 B".
  d-line example = **140 B**, below the claimed "150–170 B" — but the
  example path is only 14 chars; real deep paths push it back into range.
  **Raw dump total: 700 MB**, dead center of "650–800 MB". **Survives.**
- **Compressed 100–150 MB (10–15 B/entry)**: low end optimistic — that
  figure is the baseline for **basic** ncdu JSON; this schema is
  extended-mode with per-entry 10-digit `mtime` and distinct `d` sizes,
  both high-entropy. Realistic: 12–18 B/entry → **130–180 MB**. Also: the
  zstd seekable format resets the window at every ~512 KiB frame
  (~5–10 % loss). MINOR.
- **Full load "1–3 s"**: optimistic. Per-line document setup, name
  percent-decoding, arena interning, and parallel-frame reassembly into one
  arena (hand-waved: shared-arena u32 indices need remapping or a lock).
  Realistic **3–8 s**. MINOR→MAJOR.
- **Diff wall 3–6 s single-core**: borderline-optimistic, not dishonest.

## 2. Two-phase writer — not honest about I/O pattern or disk space

- **Finalize is a random-access permutation re-read, not sequential 2× I/O**:
  `.part` is completion-ordered, finalize emits DFS-ordered. Usually fine
  (130 MB compressed fits page cache), degrades badly on RAM-starved boxes.
  MAJOR as stated / MINOR in practice.
- **Nearly-full disk**: finalize needs `.part` + final artifact
  simultaneously → **peak ≈ 2× compressed dump (~260 MB)**. A disk analyzer
  runs precisely when the disk is full; below ~260 MB free the ordered
  artifact cannot be produced locally at all. The pipe escape means no local
  ordered dump. **MAJOR — the most under-sold weakness.**
- **Failure windows**: solid. `.part` intact until atomic rename; `e`-line
  clean-completion marker correct. One nit: a torn zstd frame loses the
  whole in-flight frame (~8k entries), not "the last line". MINOR.

## 3. Path comparator + percent-encoding — one genuine interop trap

- Names containing `/`: non-issue on Unix. Survives.
- Encoding is **injective and reversible** — no collision constructible.
  Survives.
- Component-wise comparator justification is **correct** (verified:
  `/a/foo/x` before `/a/foo.bar`; whole-string bytewise gets it wrong).
  Survives.
- **Encoded-vs-raw sort divergence (CONFIRMED)**: sorting on the encoded
  form disagrees with raw-byte order. Verified: raw `\xff…` vs `&…` —
  raw order `&` < `\xff`, encoded order `%FF` < `&`. Camembert's own
  writer+differ agree internally, but any third-party tool sorting by
  decoded names silently mis-diffs trees with non-UTF-8 names.
  **MAJOR for the interop thesis.**
- `.`/`..` filtering never stated in the spec. NITPICK.
- Byte-wise case-sensitive: fine Linux-first, latent for macOS/Windows.
  MINOR.

## 4. Streaming-diff memory bound — false on hardlink-heavy trees

- **Full delta tree sorted by growth**: survives better than expected —
  top-N-by-growth is a bounded-heap streaming pass; the navigable tree is
  O(changed dirs) with lazy drill-down re-reads. Defensible.
- **Hardlink seen-set (CONFIRMED break)**: "never O(entries)" is **false**
  on hardlink-heavy trees — rsnapshot/BackupPC farms, Nix/Guix stores, pnpm
  stores, where nearly every file has `nlink>1`. 5M inodes × ~32 B ≈
  **160 MB per side, 320 MB both**, unevictable. On a 10M-inode backup farm
  it is O(entries), ~640 MB+. HANDOFF §3 itself warned "ce set grossit
  vite". **MAJOR.**

## 5. Mega-dir spill, pipe story, zstdcat|jq

- Mega-directory spill: textbook, fine for writing. Listing a 5M-entry dir
  is inherently ~350 MB in any format. MINOR.
- **ordered:false over pipe (real catch)**: the streaming merge-join is
  **not available on the live pipe** — the receiver must external-sort the
  whole stream first (O(entries) temp disk ~700 MB + full pass + latency),
  recreating the 2× disk cost on the receiving machine. "Exactly the
  remote-scan topology" quietly means "the topology where you don't get the
  streaming diff". **MAJOR.**
- **`zstdcat | jq` (SURVIVES — empirically tested)**: concatenated
  independent zstd frames + trailing skippable seek-table frame → `zstdcat`
  decodes all content and silently ignores the skippable frame (rc 0);
  `jq` streams; `jq -s length` slurps. **TRUE.**
- **64-bit JSON numbers (new finding, tested)**: `node`'s `JSON.parse` of a
  63-bit inode returns a corrupted value (>2^53 doubles). Inodes routinely
  exceed 2^53 on XFS/btrfs/ZFS; the self-contained HTML exporter (JS) and
  any `jq` arithmetic silently mangle them. serde_json/Python safe; the
  advertised lightweight consumers are not. Fix: emit inodes (and possibly
  sizes) as JSON **strings**. **MAJOR against the interop thesis.**

## 6. What it quietly doesn't serve

- UI-navigable-during-scan: orthogonal, but note two aggregate code paths
  (live rollup + finalize rollup). NITPICK.
- Stale-cache reopen: 3–8 s full load, not "instant"; lazy open is instant
  with per-navigation decompress. MINOR.
- **Incremental re-scan / cache refresh: actively resisted** — a sorted,
  compressed, aggregate-rolled immutable stream cannot be patched; any
  refresh is a full rewrite (SQLite updates in place). MINOR→MAJOR.
- mmap: conceded honestly.

## 7. Where it's strictly dominated

- Bigger-than-RAM persistent browse (duc territory): dominated by
  SQLite/duc. Conceded.
- Incremental cache refresh: dominated by SQLite.
- Fast/mmap reopen: dominated by any mmap-able binary (~10×, conceded).
- **Hardlink-heavy diff memory: dominated by SQLite** (on-disk index dedup,
  `ORDER BY delta DESC` without RAM-resident seen-set): ~30 MB vs ~640 MB.
- **Not dominated (survives)**: crash tolerance, schema evolution, ncdu
  import, and the core §4 diff constraint on normal trees — competitive
  with SQLite, wins on simplicity, debuggability, kill -9 survival.

## Verdict: VIABLE WITH FIXES

Required fixes:

1. **Encode 64-bit inodes as JSON strings** (empirically confirmed
   corruption in JS/jq arithmetic).
2. **Resolve encoded-vs-raw sort divergence** — define the sort key on raw
   bytes, or stamp "conformant tools MUST sort on the encoded form" and
   stop advertising naive-consumer diff interop.
3. **Restate the diff memory bound** as O(changed dirs + distinct
   hardlinked inodes) — O(entries) on hardlink-heavy trees.
4. **Be honest about finalize**: 2× compressed dump in free disk (hazard in
   the tool's own use case), random-access re-read; give the pipe escape
   top billing and note the receiver's external sort.

Recommended: temper 100 MB → 130–180 MB, 1–3 s → 3–8 s; note seekable-frame
compression penalty; note cache refresh = full rewrite.
