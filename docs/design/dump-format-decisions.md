# Dump format — decisions (co-design session, 2026-07-22)

Outcome of the co-design session over the
[options dossier](dump-format-options.md). These decisions are settled;
reopening one requires a new element, not re-litigation.

## D1 — Format family: Option A now, Option C later as cache

The v1 dump/interchange format is **Option A: JSON Lines in a zstd
seekable container**. SQLite (Option C) is deferred to wave 4 as an
**optional local cache/index derived from dumps** — regenerable and
deletable, never the interchange format. Option B (custom binary) is
dropped; its salvageable ideas (CRC-checked frames, PNG-style magic,
capability flags) are folded into A's spec where applicable.

## D2 — Hardlink attribution: deterministic canonical owner

Aggregate attribution of a hardlinked inode `(dev, ino)` is defined **in
the format spec**, not in reader code: the **canonical owner is the link
whose full path is smallest in the format's sort order** (raw-byte,
component-wise) among all links *seen by the scan*. The owner's directory
chain counts the full size; other links count 0 in aggregates but keep
their full per-entry metadata. Both the writer's aggregates and the differ
use this rule, making aggregates reproducible across scans of an identical
tree (kills the phantom-diff class of bugs).

## D3 — Sort key: raw name bytes

All ordering (sibling sort, DFS order, merge-join comparator) is defined on
the **raw filename bytes** (component-wise for paths). The percent-encoding
of non-UTF-8 bytes exists only to produce valid JSON strings; it is not the
sort key. Consequence: third-party tools that decode names and sort
naturally agree with camembert's comparator.

## D4 — u64 fields in JSON: strings above 2^53, inodes always

`ino` and `dev` are **always emitted as JSON strings**. Any other u64
field (sizes, counters) **must** be emitted as a string when its value is
≥ 2^53, and may be either below. Readers **must** accept both number and
string for every u64 field. Rationale: `JSON.parse`/jq-arithmetic silently
corrupt integers above 2^53 (empirically verified with a 63-bit inode).

## D5 — Low-disk behavior: degrade, but keep working

The tool never hard-fails a scan for lack of finalize space. If the
ordered finalize (which needs ~2× the compressed dump transiently) cannot
complete, the writer **keeps the unordered dump** (`"ordered":false` — a
fully valid, diff-upgradeable artifact) and prints a clear warning with
the upgrade path (`camembert dump sort`, possibly on another machine) and
the pipe alternative for next time.

## D6 — Naming and v1.0 scope

- Format name: `camembert-dump` (the `format` field of the header line).
- File extension: **`.cmbt`**.
- v1.0 field scope: the Option A schema as amended by D2–D5 (see the
  [spec](../format/dump-v1.md)). btrfs shared extents, atime, quotas,
  nanosecond mtime are explicitly *not* in v1.0 — they are additive minor
  bumps later.

## Condensed reasoning trail

### Research

Gathered 2026-07-22 (full digest: `dump-format-research.md`, git history).

- **ncdu JSON** (`-o`): nested-array tree, `ino` only emitted since 1.16
  when `nlink>1`, `dev` inherits from parent when absent. Known upstream
  problem: the multithreaded exporter buffers the **whole tree in RAM**
  before writing — directly motivated ncdu's binary format, and warned
  against doing the same in camembert.
- **ncdu binary** (`-O`): zstd-framed CBOR items, index-of-block-offsets
  written **last**. A writer killed mid-scan leaves data blocks but no
  valid index — the tree becomes unreachable. This is the crash-tolerance
  anti-pattern camembert's format must avoid (motivates "every block
  independently interpretable" and "presence of the trailing `e` line is
  the clean-completion marker").
- **duc**: no dump file, a persistent database index (Tokyocabinet
  default; SQLite/LevelDB/LMDB optional), proven past 500M files —
  precedent for Option C as a cache, though weaker than pitched (see
  attack-c).
- **mtree** (BSD): spec files kept in filename-sorted order; compare mode
  sorts both sides and streams a `comm(1)`-style diff — primary-source
  confirmation that **sorted emission enables an O(1)-memory merge-join
  diff**. This is the mechanism D3's sort key exists to preserve.
- **Sizing** (10M entries): ncdu-style JSON ≈ 60–70 B/entry raw, ~10 B/entry
  gzipped (≈600–700 MB raw / ~100 MB compressed); a compact binary record
  ≈ 44–48 B/entry (~450 MB). Loading two dumps fully ≈ 1 GB RSS; a
  **sorted streaming merge-join** turns that into a few MB — the direct
  payoff of guaranteeing sort order in the format.
- **Serialization survey**: JSON Lines, MessagePack, CBOR, bincode,
  postcard, Cap'n Proto, FlatBuffers, Parquet, SQLite compared on
  streaming-write, partial-read, schema evolution, crash tolerance.
  Decisive facts: **FlatBuffers requires the whole buffer in memory before
  finalize** (worst crash story); **SQLite WAL is the most battle-tested**
  torn-write recovery; **JSON Lines/CBOR/MessagePack all survive
  truncation** since every complete value up to the cut is valid; Parquet's
  footer-at-end makes a truncated file entirely unreadable (same
  anti-pattern as ncdu binary).
- **64-bit JSON numbers**: `JSON.parse` of values ≥ 2^53 loses precision
  (empirically verified against a real 63-bit inode) — the direct source
  of D4.
- Open gaps (dua-cli's exact schema, restic's sort-order guarantee, borg/
  tmutil internals) were logged but never fed into any decision — dropped
  here.

### Options

Three proposals pushed to their limit, then each adversarially attacked
(`dump-format-options.md`, `-option-{a,b,c}-*.md`, `-attack-{a,b,c}.md`).

- **A — JSONL + zstd seekable.** One JSON entry per line, grouped into
  per-directory blocks, path-sorted by a finalize pass, inside a zstd
  seekable container (`zstdcat | jq` works unmodified). Won on: crash
  tolerance (every complete line valid), interop (verified), schema
  evolution ("add a key"), engineering cost (~2–3 person-weeks), and the
  diff-in-bounded-memory requirement on normal trees. Lost on: fast
  mmap-style reopen (3–8 s parse), lazy browse of bigger-than-RAM trees,
  incremental cache refresh (any update is a full rewrite). Chosen as v1
  because the MVP/wave-2 feature set (dump, diff, ncdu import,
  non-interactive mode) is served best or well by A at a fraction of B/C's
  cost, and nothing in waves 1–3 needs what only C provides.
- **B — "CAMB1" custom binary.** Directory-entry blocks, varint-packed,
  CRC-protected frames, dir IDs assigned at discovery so parents can
  reference not-yet-written children; pitched as a lock-free multithreaded
  writer with an O(200 MB) streaming diff. Lost outright: its headline
  diff cost (frames placed in completion order, walked in name-sorted DFS
  order) was refuted as ~4 orders of magnitude off (minutes, not 0.5 s;
  see attack findings 1–2 below), its kflags bit budget was already
  exhausted at v1, and its engineering cost was ~5–8× Option A's for no
  surviving advantage. Nothing about B survived review that A or C didn't
  already provide, except frame-level crash recovery, which is portable —
  folded into A per D1.
- **C — plain SQLite file.** `entry`/`dir_agg` tables, WAL during writing,
  `(parent_id, name)` covering index for both lazy browse and a
  merge-join diff. Won on: lazy random-access browse, ad-hoc SQL
  aggregation (owner/pattern), and **in-place incremental cache
  refresh** — unique among the three, and exactly wave 4's "honest,
  background-refreshed cache" feature. Lost on: size (~8× A on disk, ~1.1–
  1.25 GB at 10M entries), full-disk finalize behavior (index build
  externally sorts ~450 MB — a landmine on the near-full/tiny-VPS targets
  this tool is built for), and a first-seen hardlink-dedup policy that
  produces nondeterministic aggregates (phantom diffs) unless fixed.
  Deferred to wave 4 as a derived, regenerable/deletable cache rather than
  the interchange format — it wins exactly the jobs A structurally can't
  do, so the two are complementary rather than competing for v1.

### Attack findings

Each proposal received one dedicated adversarial pass. Numbering below
follows each source document's own structure (attack-b labels its findings
explicitly `FINDING 1`–`9`; attack-a and attack-c don't use the word
"finding" but do number their sections/fix-list 1–7 each, preserved as-is).

**Attack A** (verdict: viable with fixes) — `dump-format-attack-a.md`:

1. Quantitative claims: raw bytes/entry and total (~700 MB/10M) confirmed
   accurate; compressed size (100–150 MB) and full-load time (1–3 s) were
   optimistic for extended-mode fields — realistic figures are 130–180 MB
   and 3–8 s. Not a binding decision, just a documentation correction to
   carry into the spec/estimates.
2. The two-phase writer's finalize pass needs `.part` + the final artifact
   simultaneously, peaking at ~2× the compressed dump (~260 MB) — a hazard
   on the near-full disks this tool targets. Resolved by **D5**: degrade to
   the unordered dump rather than fail, with a documented upgrade path.
3. Sorting on the percent-encoded form disagrees with raw-byte order
   (confirmed with a constructed non-UTF-8 example) — any third-party tool
   that decodes names before sorting silently mis-diffs. Resolved by
   **D3**: the sort key is the raw name bytes; encoding exists only for
   JSON string validity.
4. The streaming-diff memory bound ("never O(entries)") is false on
   hardlink-heavy trees (backup farms, Nix/pnpm stores): the per-side seen
   set of `(dev,ino)` is unevictable and can reach hundreds of MB.
   Rejected as an algorithmic fix — no bound was found — and instead
   absorbed by restating the bound honestly in the spec as O(changed dirs
   + distinct hardlinked inodes), which is O(entries) on that class of
   tree. **D2**'s canonical-owner rule fixes the separate problem of
   nondeterministic aggregates on the same trees, not this RAM cost.
5. Mixed: mega-directory spill is fine as designed (no change);
   `"ordered":false` piped dumps genuinely lose the streaming-diff property
   (the receiver must externally sort first) — accepted as a documented
   limitation of the pipe escape hatch in **D5**, not eliminated; the
   `zstdcat | jq` interop claim was empirically confirmed true; 64-bit JSON
   number corruption in `node`'s `JSON.parse` on a real 63-bit inode was
   confirmed and is the direct basis for **D4**.
6. Stale-cache reopen (3–8 s, not instant) and incremental re-scan (a
   sorted/compressed/aggregate-rolled stream can't be patched in place,
   any refresh is a full rewrite) are real, un-fixed weaknesses of A.
   Absorbed by **D1**: these are exactly the jobs handed to Option C in
   wave 4 instead of being solved inside A.
7. Axes where A is strictly dominated (bigger-than-RAM persistent browse,
   incremental refresh, mmap-fast reopen, hardlink-heavy diff RAM — all
   dominated by SQLite) directly informed **D1**'s A-now/C-later split:
   nothing in this list is a v1 requirement.

**Attack B** (verdict: not viable as pitched) — `dump-format-attack-b.md`:

1. FATAL: the claimed 0.5 s diff is actually tens of minutes to ~70
   minutes — DEBs are placed in completion order but the diff walks
   name-sorted DFS order, so frame-cache thrash produces ~6.4 TB of
   decompression, not one pass. Central to **D1**'s decision to drop B.
2. Streaming a deleted subtree via the index hits the same blowup at
   smaller scale (a 100k-dir deletion needs tens of thousands of frame
   decompressions). Same disposition as finding 1 — dropped with B.
3. The claimed 96 MB writer memory bound is false for large directories:
   a DEB can't flush until its (up-to-4-GiB) parent directory is fully
   serialized, so worst-case per-thread memory is unbounded in directory
   size. Dropped with B; no fix pursued since B itself was dropped.
4. Bytes/entry were optimistic (~27–31 B realistic vs. claimed 25–26) —
   minor, but one more data point against B's numbers; dropped with B.
5. On-disk size loses to gzipped ncdu JSON once compared honestly (vs.
   uncompressed JSON, not the real competitor), and the browse-ceiling
   arithmetic had a unit slip. Dropped with B.
6. The `kflags` bit budget is exhausted at v1 with HANDOFF's already-known
   future fields (btrfs extents, atime, quotas, per-owner stats), forcing
   a TLV labyrinth within one release cycle. This is the direct reason
   **D1** folds B's *ideas* (CRC-checked frames, PNG-style magic,
   capability flags) into A rather than adopting a flag-bit scheme.
7. Crash recovery is B's one genuinely strong section (survives, with
   caveats about adversarial corruption and needing `repair` before
   mmap-grade browse) — this is the part **D1** explicitly salvages into
   A's spec.
8. The ~2–3 person-week-vs-B engineering estimate was itself ~2× optimistic
   (realistic ~13–19 person-weeks, 5–8× A) — the cost argument that,
   combined with finding 1, made **D1**'s "drop B" call straightforward.
9. B is strictly dominated on nearly every claimed axis (diff, schema
   evolution, tooling, crash simplicity, cost, compressed size) by either
   A or SQLite, with only mmap O(1) browse uncontested — and SQLite
   matches that too. This overall verdict is **D1**'s stated rationale for
   dropping B outright.

**Attack C** (verdict: viable with fixes) — `dump-format-attack-c.md`;
numbering follows the verdict's own fix list, which restates each finding
from the body:

1. `dir_agg`'s first-seen hardlink dedup is traversal-order-dependent: two
   scans of an identical tree can produce different cumulative sizes for
   directories sharing hardlinks, causing phantom diffs. Resolved by
   **D2**: a deterministic canonical-owner rule, applied uniformly to
   whichever format carries aggregates (A now; C if/when built in wave 4).
2. The recursive diff walk can SIGSEGV on musl (128 KiB default thread
   stack vs. glibc's 8 MiB) at realistic tree depths. Not yet applicable
   since C isn't implemented — flagged as a requirement for whenever C is
   built as the wave-4 cache (explicit heap stack or a big-stack thread).
3. `CREATE INDEX` finalize externally sorts ~450 MB; on `TMPDIR`-on-tmpfs
   or near-full disks this is a disk-full/OOM landmine, the single most
   under-disclosed risk in the proposal. Deferred alongside C to wave 4 —
   **D5**'s "never hard-fail for lack of finalize space" principle is
   expected to extend to C's finalize when it's built, but no C-specific
   mechanism was designed since C isn't v1.
4. "Browse during scan" via a second WAL reader pins a snapshot, so the
   checkpointer can't reclaim frames during a long write — the `-wal` file
   can grow to roughly the size of the whole DB mid-scan. Deferred with C;
   noted as a feature to fix-or-drop when C is actually built.
5. `VACUUM INTO` needs ~1 GB free to produce a transfer snapshot —
   unavailable exactly when disk is tight; the fix (checkpoint+copy as the
   default transfer path) is deferred with C.
6. Several numbers needed re-baselining (total on disk ~1.12–1.25 GB not
   ~1 GB, covering index ~470–490 MB, exact diff 30–60 s not 10–25 s,
   insert throughput ~300–600k rows/s once `dir_agg`'s non-sequential
   inserts are counted). Recorded here for whenever C's numbers are next
   used; no decision needed since C isn't being built now.
7. The covering index lacks `ino`/`uid`, so the "freeable"/owner-breakdown
   views need extra main-table lookups the index-only framing undersells;
   the "`sqlite3` CLI is already on the server" and "duc already proved
   this" claims don't hold on minimal containers / for duc's actual
   (non-SQLite-by-default, per-directory-row) precedent. Recorded as
   corrections for C's future write-up; not resolved now since C is
   deferred.

The full text of all eight superseded documents remains available from
git history — see the commit that deleted them in favor of this file.
