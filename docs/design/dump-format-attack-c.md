# Adversarial review — Option C "just use SQLite"

> Produced by an adversarial agent instructed to demolish the proposal.
> Verdict: **VIABLE WITH FIXES** — but the doc systematically undersells
> failure modes on exactly the machines the HANDOFF targets.

## Quantitative claims

- **Entry-table cell ~58 B / ~580 MB: SURVIVES** (redone from SQLite serial
  types; sensitive to name length — 620–680 MB at realistic 18–20 B names).
- **Covering index understated**: ~49 B/entry → **~470–490 MB**, not
  420 MB (the appended 4-byte rowid was under-weighted). MINOR.
- **Total on disk**: realistically **1.12–1.25 GB**, not "~1 GB"; §1 and §9
  also quote inconsistent per-entry figures. MINOR.
- **Insert throughput**: the 400–900k rows/s headline is for pure ascending
  appends; `dir_agg` (~1M rows) arrives bottom-up in non-sequential order →
  random B-tree inserts into a second hot tree → effective **300–600k/s**.
  Not fatal (cold-cache scans never saturate the writer — that part is
  honest). MINOR→MAJOR.
- **Index build 10–25 s: DISK/RAM LANDMINE.** `CREATE INDEX` externally
  sorts ~450 MB. Near-full disk → temp files hit disk-full → finalize fails.
  `TMPDIR` on tmpfs (common) → ~450 MB lands in RAM → **OOM on a 256 MB
  VPS**. HDD server: realistically **30–90 s**. **MAJOR — the most
  under-disclosed risk.**
- **Diff cursor step 0.5–1 µs: OVERSOLD.** Real cost through rusqlite:
  step + ~8 column FFI calls + BLOB name copy ≈ **1.5–3 µs**; and because
  each recursion level holds a live cursor, `prepare_cached` can't be
  reused across levels → ~2M re-prepares. Exact diff realistically
  **30–60 s**, not 10–25 s — tolerable for incident response, not the
  implied instant answer. MAJOR.

## The hardlink blow: dir_agg first-seen dedup is order-dependent [MAJOR]

`hardlink_policy='dedup_first_seen'` + a format that explicitly guarantees
no traversal order ⇒ **two scans of a byte-identical tree can produce
different `dir_agg.cum_*`** for directories sharing hardlinks. The diff
then reports **phantom growth/shrink where nothing changed**, and `--fast`
pruning can be defeated the same way. Directly undermines "qu'est-ce qui a
grossi" — the HANDOFF's best-value feature. Fix requires a canonical dedup
owner (e.g. lowest inode / lowest (parent_id, name)) — a design change,
not a footnote. Not addressed anywhere.

- Covering index lacks `ino`/`uid`: the specified tree diff stays
  index-only (conceded), but the "libérable" feature needs (dev,ino) →
  rowid lookups, and owner breakdown needs a main-table scan. MINOR, but
  the "index-only" framing oversells.
- **Diff RSS < 100 MB: SURVIVES** — dedup is precomputed at scan time into
  `dir_agg`; the diff never rebuilds an inode set.

## Recursion, cursors, musl

- **O(depth) open cursors: SURVIVES** (SQLite tolerates thousands of open
  statements; real depth is tens).
- **Native recursion SIGSEGVs on musl [MAJOR]**: `diff_dir` recurses on the
  call stack; **musl's default thread stack is 128 KB** (glibc 8 MB). At
  ~1 KB/frame, overflow around ~128 levels — reachable with deep
  `node_modules`/backup snapshots — on the exact primary build target.
  Needs an explicit heap stack or a spawned big-stack thread. Undisclosed.

## Write path / WAL

- **WAL unbounded growth with a live reader [MAJOR]**: the advertised
  "browse during scan via a second read-only connection" pins a snapshot;
  the checkpointer can't reclaim frames past it, and during a 1 GB one-pass
  write the `-wal` file can grow to ~DB size → **disk-full blowout
  mid-scan** on the near-full machines the tool targets. SQLite documents
  this hazard; the doc presents the feature as pure upside. (The primary
  TUI reads its own arena — the *advertised* cross-process feature is the
  liability.)
- **Crash salvage honest but incomplete [MINOR→MAJOR]**: dir_agg is written
  bottom-up, so at any crash the root and upper dirs systematically have
  **no aggregates** — the first thing a user opens. Recovering correct
  top-level numbers from a partial tree isn't possible (hardlink dedup).
  "Valid readable prefix" is true for rows, oversold for aggregates.

## Single-file, VACUUM INTO, CLI availability

- **VACUUM INTO needs ~1 GB free [MAJOR]**: unavailable exactly when disk
  is tight. Default transfer path should be
  `wal_checkpoint(TRUNCATE)` + copy.
- "sqlite3 CLI already on the server": **false** on distroless/scratch/
  minimal-Alpine — the very environments the HANDOFF names. Lost bonus,
  not blocker. MINOR.
- `mmap_size=256 MB` unconditional: reckless on 32-bit / 256 MB VPS. MINOR.
- **duc precedent weaker than pitched**: duc defaults to Tokyocabinet
  (SQLite is a build option), stores per-directory rows (orders of
  magnitude fewer), and the ">500M files" test isn't attributed to the
  SQLite backend. The precedent doesn't cover this design. MINOR.

## Versioning — mostly SURVIVES

`user_version` gate fine; `ALTER TABLE ADD COLUMN` appends, so positional
reads stay stable — additive evolution is safe. Residual risk is semantic
redefinition sneaking under a minor bump (policy, not enforcement — true of
every format). MINOR.

## Cross-compare

For the MVP's constrained-target dump-writing (near-full disk, tiny VPS,
minimal containers), **JSONL+zstd strictly dominates**: ~100 MB artifact vs
~1.1 GB + WAL sidecar + ~450 MB finalize temp; no finalize cliff; O(1)
writer memory. SQLite wins decisively on **random-access lazy browse,
ad-hoc SQL aggregation (owner/pattern), and in-place stale-cache update** —
genuine and unmatched by JSONL — but those are *later waves* in the
HANDOFF, not the MVP. The sorted streaming diff is not unique to SQLite.

## Verdict: VIABLE WITH FIXES

1. Make dir_agg hardlink dedup **deterministic** (canonical owner).
2. Rewrite the recursive diff with an **explicit heap stack** (musl).
3. Disclose and bound the **finalize temp-space** demand (~450 MB);
   handle tmpfs TMPDIR and disk-full.
4. Fix or drop **browse-during-scan** (WAL growth hazard).
5. Default transfer = checkpoint+copy, not VACUUM INTO.
6. Re-baseline numbers (1.1–1.2 GB total, ~470–490 MB index, 30–60 s exact
   diff, throughput with dir_agg interleaved).
7. Add the ino/uid story for freeable/owner views; correct the sqlite3-CLI
   and duc-precedent claims.

Strategic note: SQLite earns its keep only if lazy browse, in-place cache
updates, and ad-hoc SQL become first-class requirements — HANDOFF lists
them as later waves.
