# Option C — "just use SQLite" (off-the-shelf maximalist angle)

> Proposal produced by a design agent instructed to push the reuse-a-proven-
> container angle as hard as it honestly can. Not a decision.

## 1. Pitch

The dump is a plain SQLite database file: one table of entries keyed by
`(parent_id, name-as-BLOB)`, one table of per-directory aggregates, WAL mode
during writing. This buys, for zero container-engineering effort, every hard
requirement at once: a single writer thread fed by the scan threads streams
batched transactions while the scan runs; WAL makes every committed batch a
durable, readable prefix even under SIGKILL; a `(parent_id, name)` covering
index gives lazy per-directory browsing in O(log n) and — because BLOB
comparison is memcmp — a deterministic child ordering that turns diff into
an mtree-style lockstep merge-join with O(tree-depth) memory; `PRAGMA
user_version` + additive `ALTER TABLE` is versioning SQLite has supported
for 20 years; and the artifact is a single self-describing file you can
`scp` off a server and inspect with the `sqlite3` CLI already installed
there. The honest price: ~90–105 bytes/entry (~1 GB at 10M entries, ~2× a
bespoke compact binary, zstd-able to ~300 MB for transfer) and a
single-writer insert ceiling around 0.5–1M rows/s. duc has already proven
the category (SQLite backend option, tested past 500M files).

## 2. Schema DDL

```sql
PRAGMA user_version = 1;          -- format MAJOR version

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value                                  -- flexible typing is deliberate
) WITHOUT ROWID;
-- required keys: format_minor, progname, progver, scan_root (BLOB),
-- scan_start, scan_end, complete (0/1), extended (0/1),
-- size_semantics ('blocks'|'apparent'), hardlink_policy ('dedup_first_seen')

CREATE TABLE dev (                       -- st_dev interning: <10 rows typ.
  dev_id INTEGER PRIMARY KEY,            -- small ints ⇒ 1-byte varints
  st_dev INTEGER NOT NULL UNIQUE
);

CREATE TABLE entry (
  id        INTEGER PRIMARY KEY,         -- rowid; scanner's atomic counter
  parent_id INTEGER NOT NULL,            -- 0 = "above root" sentinel
  name      BLOB    NOT NULL,            -- raw filename bytes, NOT text
  kind      INTEGER NOT NULL,            -- 0=dir 1=regular 2=symlink 3=other
  flags     INTEGER NOT NULL DEFAULT 0,  -- bitmask, see §3
  asize     INTEGER,                     -- st_size; NULL if stat failed
  dsize     INTEGER,                     -- st_blocks * 512; NULL if failed
  mtime     INTEGER,                     -- unix seconds
  dev_id    INTEGER,
  nlink     INTEGER,                     -- NULL means 1
  ino       INTEGER,                     -- only when nlink > 1, else NULL
  uid       INTEGER,                     -- extended mode only
  gid       INTEGER,
  mode      INTEGER
);

CREATE TABLE dir_agg (                   -- one row per *completed* subtree
  entry_id  INTEGER PRIMARY KEY,
  cum_asize INTEGER NOT NULL,            -- hardlink-deduped per meta policy
  cum_dsize INTEGER NOT NULL,
  n_inodes  INTEGER NOT NULL,            -- subtree inode count
  n_files   INTEGER NOT NULL,
  n_dirs    INTEGER NOT NULL,
  n_errors  INTEGER NOT NULL             -- unreadable children in subtree
);

-- Created at FINALIZE, not during scan (see §5, §7):
CREATE INDEX idx_children
  ON entry(parent_id, name, kind, dsize, asize, mtime, flags, nlink);
```

`name` as BLOB is load-bearing twice — arbitrary bytes from `getdents64`,
and memcmp BLOB comparison gives a collation-proof total order per
directory. The covering index means browsing and diffing never touch the
main table; `uid/gid/mode/ino` are fetched by rowid only when displayed.
NULLs cost ~1 byte each.

## 3. Field mapping

| Requirement | Where | Notes |
|---|---|---|
| name, possibly non-UTF-8 | `entry.name BLOB` | raw bytes |
| apparent size | `entry.asize` | varint: small files cost 1–3 bytes |
| disk size | `entry.dsize` | `st_blocks * 512` |
| mtime | `entry.mtime` | nanos via added column later |
| uid/gid/mode (optional) | `entry.uid/gid/mode` | NULL without `-e` |
| inode + dev | `entry.ino` + `entry.dev_id`→`dev.st_dev` | ino only when `nlink>1` (ncdu 1.16 trick) |
| nlink | `entry.nlink` | NULL ⇒ 1 |
| error / excluded flags | `entry.flags` bitmask | bit0 read_error, bit1 stat_error, bit2 excl_pattern, bit3 excl_otherfs, bit4 excl_kernfs, bit5 excl_frmlink, bit6 notreg; unknown bits ignored (reserved) |
| per-dir inode counts | `dir_agg.n_inodes` | plus files/dirs/errors split |
| ncdu JSON import | 1:1 | SAX-parse the nested array, push through the identical insert path. The afternoon estimate survives. |

## 4. Ordering guarantees and the streaming merge-join diff

The format guarantees **nothing about physical row order** — that's what
makes it writable under a multithreaded scan. The guarantee is logical, from
`idx_children`: *children of any directory are enumerable in ascending
memcmp(name) order in O(log n + k)*. That's the mtree property generalized
from a flat sorted list to a tree.

Diff = simultaneous preorder descent of both dumps:

```
fn diff_dir(a: DirCursor, b: DirCursor) {   // each = "SELECT ... WHERE
    loop {                                   //  parent_id=? ORDER BY name"
        match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => match memcmp(x.name, y.name) {
                Less    => { emit_removed(x); a.next() }
                Greater => { emit_added(y);   b.next() }
                Equal   => {
                    if x.kind != y.kind { emit_type_changed(x, y) }
                    else if x.is_dir()  { diff_dir(children(x), children(y)) }
                    else if x.dsize != y.dsize || x.mtime != y.mtime
                                        { emit_modified(x, y) }
                    a.next(); b.next();
                }
            },
            (Some(x), None) => { emit_removed(x); a.next() }
            (None, Some(y)) => { emit_added(y);   b.next() }
            (None, None)    => return,
        }
    }
}
```

Memory bound: two B-tree cursors per recursion level (a few KB each) ⇒
**O(max path depth)** cursors, plus two capped page caches
(`PRAGMA cache_size = -32000` ⇒ 32 MB each). Index-only steps; the main
table is never read. Optional heuristic pruning: if both sides' `dir_agg`
rows match on all five counters, a `--fast` mode *may* skip the subtree
(flagged heuristic: a rename inside a dir preserves all five). Exact mode
walks everything: 2×10M index-only cursor steps at ~0.5–1 µs ≈ **10–25 s**
full exact diff.

Pure-SQL bonus for flat questions: `ATTACH 'old.db' AS old;` then set-based
queries (top growth by owner, by extension) work directly — but the *tree*
diff is the cursor merge above (recursive CTEs would blow the memory bound).

## 5. Write path under multithreaded scan

**Single dedicated writer thread**; scan workers never touch SQLite. With
SQLite this is the design, not a concession: one writer eliminates lock
contention, and WAL lets concurrent *readers* (live TUI) see committed
snapshots.

- Scan workers allocate entry IDs from a shared `AtomicU64`. A directory's
  ID is allocated at discovery — before its children are enqueued — so every
  record arrives at the writer already carrying a valid `parent_id`, in any
  completion order. No DB round-trip in the hot path.
- Workers push fixed-size batches (~4096 entries) into a **bounded**
  crossbeam MPSC channel (64 batches ⇒ ~25 MB max). Bounded ⇒ backpressure;
  memory stays flat.
- Writer loop: `BEGIN` … prepared `INSERT` per row … `COMMIT` every
  ~50–100k rows or 250 ms. `dir_agg` rows inserted as the scanner reports
  subtree completion (bottom-up).
- Pragmas during scan: `journal_mode=WAL`, `synchronous=NORMAL`,
  `wal_autocheckpoint=4000`, `page_size=8192`, `cache_size=-131072`
  (128 MB), no secondary index yet.
- **Finalize**: `CREATE INDEX idx_children ...` — sorted bulk build over 10M
  rows, roughly 10–25 s, perfectly packed; write `meta.complete=1`;
  `PRAGMA wal_checkpoint(TRUNCATE)`.

Throughput, honestly reasoned: prepared statements + batched transactions +
WAL + no secondary index ⇒ ascending-rowid appends at ~1–2 µs CPU each ⇒
**400k–900k rows/s** modern core (the classic "~1M inserts/s SQLite" config),
maybe 250–400k/s on an old server core. 10M entries = 12–30 s of writer CPU,
overlapped with the scan. Hot-dentry-cache scans on many cores can exceed
1M entries/s and hit backpressure; escape hatches: (a) bounded channel
throttles dump writing without stalling the UI arena, (b) `--dump-at-end`:
scan into the arena first, bulk-dump in ~15–25 s. Cold-cache scans (the case
that matters on servers) run at 50–200k stats/s and never saturate the
writer.

## 6. Read paths and memory bounds

- **Full load**: `SELECT * FROM entry` in rowid order = sequential scan at
  SSD speed; 1 GB file → arena in a few seconds.
- **Lazy browse**: open read-only, `mmap_size=256MB`. One directory level =
  one covering-index range scan, O(log n + k) page touches,
  sub-millisecond warm. Cumulative sizes from `dir_agg` by primary key: an
  ncdu-style listing is k+1 point/range queries. A 100M-entry dump browses
  in ~constant memory. This is the feature ncdu invented its entire binary
  format to get; here it's an index.
- **Browse during scan**: a second read-only WAL connection (even another
  process) sees the latest committed batch — ≤250 ms stale. No index yet,
  but the live TUI reads its own arena anyway; the cross-process trick is a
  freebie.
- **Diff**: §4; peak RSS ≈ 2×32 MB page cache + cursors + output ⇒
  **< 100 MB** for two 10M-entry dumps.

## 7. Crash tolerance

WAL: checksummed, length-prefixed frames; recovery discards torn tails.
Kill -9 ⇒ the database contains exactly the committed batches — a valid
readable prefix losing at most ~100k rows / 250 ms.

Salvage on open when `meta.complete` missing: (1) `CREATE INDEX` if absent
(~10–25 s at 10M, one-time), (2) directories lacking a `dir_agg` row render
as "incomplete — sizes unknown" — the honest display (their subtrees
genuinely weren't finished).

`synchronous` choice: in WAL, `NORMAL` fsyncs only at checkpoints
(near-zero cost), guarantees app-crash consistency, only risks losing recent
commits (not corruption) on power loss. `OFF` can corrupt on power loss;
defensible for a regenerable dump, but NORMAL is so cheap that OFF should be
a `--unsafe-fast` flag, not the default. We use WAL for concurrent readers
and torn-write recovery, not durability.

One real gotcha: a live WAL database is *two* files (`.db` + `.db-wal`).
Ship only after finalize (checkpoint TRUNCATE + close), or `VACUUM INTO`
to snapshot (which also compacts). Document it; it has bitten everyone once.

## 8. Versioning & evolution

- `PRAGMA user_version` = format **major**; reader refuses mismatch.
- `meta.format_minor` = additive level; readers ignore unknown meta keys,
  unknown flag bits, extra columns (old readers never SELECT them), extra
  tables.
- Additions are `ALTER TABLE entry ADD COLUMN ...` — O(1) in SQLite (old
  rows return the default). New per-entry data (btrfs shared-extents, atime)
  = new nullable column or side table, minor bump.
- Semantics changes (redefining `dsize`) = major bump; avoid by policy.
- Strictly stronger than hand-rolled TLV evolution because it's *queryable*:
  `pragma table_info(entry)` tells any tool exactly what a file contains.

## 9. Size & memory numbers (10M entries)

| Component | Bytes/entry | 10M total |
|---|---|---|
| `entry` table cell | ~55–62 | ~580 MB |
| `idx_children` covering index | ~38–45 | ~420 MB |
| `dir_agg` (~1 dir per 8–10 entries) | ~4–5 | ~45 MB |
| **Total on disk** | **~95–110** | **~0.95–1.1 GB** |

Reference: bespoke compact binary ~44–48 B/entry (~480 MB); ncdu JSON basic
~600–700 MB, extended ~1.2–1.7 GB. Dropping the covering columns saves
~150 MB if it matters. Transfer artifact: `VACUUM INTO` + `zstd -3` ⇒
**~300–380 MB over the wire**; decompress before opening (SQLite has no
transparent compression).

Diff peak RSS: **< 100 MB**, runtime 10–25 s exact, seconds with aggregate
pruning. Write-side overhead: ~25 MB channel + 128 MB page cache.

## 10. Rust crates

- **`rusqlite`** with **`bundled`** — pins the exact SQLite version into the
  static musl binary (+~0.9 MB). Mainstay crate since 2014.
- **`crossbeam-channel`** — bounded MPSC scan→writer.
- **`zstd`** — outer compression for the transfer artifact only.
- ncdu import: `serde_json` `StreamDeserializer` or a hand-rolled SAX loop.
- Nothing else. That's the point.

**Inner comparison — second-best off-the-shelf: Cap'n Proto message stream**
(`capnp`). Framed message sequence: streamable writes, crash-tolerant at
message boundaries, native additive evolution, zero-copy reads, ~45–60
B/entry. But it gives you a *log*, not a *database*: no random access
without building your own index block (you've re-derived ncdu's binfmt,
including its index-written-last crash hole), no sorted-child guarantee
without a finalize re-sort you write yourself, so the bounded-memory diff
needs an external sort or full load. Every future feature (lazy browse,
stale cache, HTML export queries, owner/pattern aggregation) re-implements
machinery SQLite ships. Cap'n Proto is the right answer if the dump were
only a transfer pipe; it isn't.

## 11. Honest weaknesses

1. **Size — worst-served requirement: the scp transfer artifact.** ~1 GB raw
   at 10M entries, ~2× a compact binary; no internal compression, and a
   zstd-wrapped dump loses random access until decompressed.
2. **Single-writer insert ceiling** (~0.5–1M rows/s). Irrelevant on
   cold-cache server scans; a hot-cache many-core benchmark will bottleneck
   on the writer — a benchmark-marketing liability. Mitigated by
   backpressure/`--dump-at-end`.
3. **Finalize step is load-bearing**: index + `dir_agg` completeness +
   checkpoint. The crash prefix is real but degraded (salvage rebuild,
   "incomplete" dirs).
4. **WAL sidecar gotcha** — copying a live `.db` without its `-wal` silently
   loses recent data. `VACUUM INTO` mitigates; footguns don't disappear.
5. **You inherit SQLite's rules**: page format, C dependency in an
   otherwise-pure-Rust static binary (bundled build makes it a non-issue in
   practice, an aesthetic issue forever).
6. **Read amplification without care**: children have scattered rowids, so
   tree reads *must* go through the covering index — a constraint every
   future reader must respect.
7. **Schema evolution is additive-only in practice** — renaming/retyping a
   column is a major-version rewrite. SQL makes it tempting to "just
   migrate", which for a dump *file* exchanged between versions you must
   not do.
