# Option B — annotated tree (best-effort directory attribution)

> Proposal by a design agent instructed to push the "users think in
> directories — meet them there" angle as hard as it honestly can. Not a
> decision. Facts referenced from
> [freeable-research.md](freeable-research.md) (§n).

## 1. Pitch

The question a sysadmin actually asks is not "how many freeable bytes
does this filesystem hold" but "**what**, **where**, and **who** — so I
can restart the right daemon." The kernel hands us the where: the
`/proc/[pid]/fd` readlink text is the file's live, rename-tracked path
(research §1), and in the overwhelmingly common case — a log or cache
file deleted in place — it points squarely into the scanned tree.
Option B attaches every attributable deleted inode to the **deepest
still-existing ancestor directory** and gives directories a freeable
side aggregate, surfaced as a post-scan column. The user browsing
`/var/log` sees `+1.8 GiB freeable` on the row where the problem lives,
selects it, and reads the guilty PID in the selection card — no panel
hunt, no gauge squinting.

The structural bet: phase 2 (btrfs shared extents, hardlink siblings)
needs per-directory freeable aggregation, snapshot plumbing, and a
column/segment renderer *anyway*. Build that channel once, now, with
deleted-open files as its first source, and phase 2 becomes "add two
more sources to an existing pipe" instead of a second UI project.

## 2. What phase 1 claims

Identical sweep core to the research findings, non-negotiable in any
option:

- fd-held regular files, **`st_nlink == 0`** (never the `" (deleted)"`
  string — research §1), including `O_TMPFILE`;
- deduped by **`(st_dev, st_ino)`** (research §5);
- **`st_blocks × 512`** sizing (sparse-safe);
- **`st_dev` ∈ scanned-device set is the scope decision** (research
  §3). The path string never decides whether a file counts — only
  *where* it is displayed;
- memfd/shm/anon excluded from every disk figure (research §2);
  mmap-only holders (no fd) out of scope — `map_files` needs
  `CAP_SYS_ADMIN` and the `maps` range length is a guess (research §2);
- loop devices and unlinked directories scoped out (research open
  question 6).

On top of that, B adds the attribution layer — and states its honesty
contract explicitly in the UI and docs: **the filesystem total is
correct; the per-directory split is best-effort** (research §3:
moved-then-deleted files, paths outside the root, truncation, and
non-UTF-8 all degrade to an "unattributed" bucket, never to a wrong
directory silently... with the caveats in §9).

## 3. Attribution algorithm

Input: the deduped deleted-file list; the frozen post-scan arena.

1. Take the readlink bytes; drop the **final path component** entirely.
   Attribution only needs the ancestor chain, which sidesteps most of
   the `" (deleted)"` suffix ambiguity — the suffix decorates the last
   component only.
2. Require the remaining prefix to start with the scan root's raw
   bytes + `/`. Reject `"(unreachable)"`-prefixed paths (mount-
   namespace escapees, research §1).
3. Walk component-wise from the root dir, matching each component
   against child names as **raw bytes** (the arena stores raw names;
   no UTF-8 assumption anywhere). Stop at the first component that is
   missing, tombstoned, or an excluded mount point.
4. The deepest matched directory gets the entry **iff its `dev` equals
   the entry's `dev`** (a bind-mount or crossed-filesystem path text
   must not attribute bytes across devices). Otherwise: unattributed.
5. Unattributed entries land in a per-filesystem residual bucket,
   displayed on the disk gauge line (so the filesystem total is always
   the sum of what the tree shows plus the residual — the books
   balance visibly).

Cost: tens of entries times path depth — microseconds. Recomputed
wholesale on every refresh.

## 4. Data model

Sweep report struct as in the ledger design (files, holders, coverage,
`ram_backed`), plus two side maps next to the arena — deliberately the
`excluded`-reason side-map pattern from `tree.rs` (small, keyed by id,
out of the packed node):

```rust
/// Post-scan only; owned by whoever owns the frozen ScanOutcome.
pub struct FreeableAttribution {
    /// Subtree-aggregated freeable disk bytes per directory
    /// (propagated to all ancestors at build time — plain adds,
    /// single-threaded, the arena is frozen).
    pub per_dir: FxHashMap<DirId, u64>,
    /// Direct drill-down: which deleted files sit at this directory.
    pub entries_at: FxHashMap<DirId, Vec<u32>>, // indices into files
    /// Not attributable to any live directory; per device.
    pub residual: FxHashMap<u64, u64>,
}
```

Nothing in `Node`/`DirMeta` changes; `ta`/`td` are untouched (deleted
files are **not** part of any subtree total — they are extra bytes the
scan could not see as entries). The dump writer never reads these maps.

Snapshot plumbing: `Row` gains `freeable: u64`, `DirTotals` gains
`freeable`, and the post-scan snapshot builder consults `per_dir`. The
**scan-time publish path is untouched** — the sweep runs at completion,
so the owner thread and the 33 ms cadence never see any of this. The
arc-swap contract is unchanged; only the post-scan `build_snapshot`
call site (UI thread, frozen arena) grows a lookup.

## 5. Sweep timing

Same skeleton as the other options: once at scan completion (background
thread, event-channel delivery), `r`-refresh in the drill-down,
unfiltered `open_file_index` re-run when the delete-confirm modal opens
(research §7 — same walk, no `st_nlink` filter, warning fills in
asynchronously). Never during the scan: attribution needs a frozen
arena (step 3 walks children), which is the one *structural* reason —
beyond the semantic one — that no option should sweep mid-scan.

## 6. UI

- **Freeable column**, post-scan, only when at least one visible row
  has a nonzero value: rendered `+1.8G` in the accent style — the `+`
  is load-bearing, signaling "in addition to the size column, not a
  fraction of it". These bytes must **not** be painted inside the
  proportion bar: the bar shows the directory's scanned total, and
  deleted-open bytes are outside it. If a visual is wanted, it is an
  appended bright tick *beyond* the bar end — never a bright segment
  within (that rendering is reserved for phase 2's genuine fractions,
  per the tui-design reservation).
- **Selection card**: selecting an annotated directory lists the top
  holders — `nginx (PID 1234) holds 1.8 GiB of deleted access.log`.
- **Disk gauge**: filesystem total + residual — `· 2.0 GiB freeable
  (1.8 GiB shown in tree, 0.2 GiB unattributed)`. The explicit split
  is the honesty mechanism: the user can always see how much of the
  claim is directory-resolved.
- **Sort**: `SortKey::Freeable` added — "show me where the freeable
  bytes are" is the whole point of putting them in the table.
- **Coverage**: same one-line footer as the other options, in the
  drill-down view (`f` or Enter on an annotated row): `365 of 505
  processes unreadable — run as root for the full picture`. `/proc`
  absent: no column, no gauge suffix, a debug log line.

## 7. Dump and CLI

- **Dump**: no per-entry or per-directory freeable records — the
  attribution is process state and best-effort, two disqualifiers for
  a filesystem snapshot format. B proposes one concession for the
  monitoring use case: two additive keys on the `e` line (`fb`:
  freeable disk bytes, `fbn`: file count), informational only, ignored
  by diff, minor-bump per dump-v1 §10. A dump reader that shows "at
  scan time, 2 GiB were deleted-but-open" is stating a recorded fact
  about the past, not inventing present state. (The survey may well
  strike this; it is severable.)
- **CLI**: `--no-proc-sweep` (env `NO_PROC_SWEEP`), same rationale as
  the other options — auditable environments and masked-/proc
  containers get a clean off switch. Documented in `--help` + README
  in the same change.

## 8. Phase-2 growth

This is B's strongest card. Phase 2's data (btrfs
`FIEMAP_EXTENT_SHARED` bytes, hardlink siblings outside the marked
set) is per-entry; its display needs per-directory aggregation, `Row`
plumbing, a column/segment, and a sort key. Under B, all of that
exists and is tested when phase 2 starts; the work is: add per-entry
sources, extend `per_dir` from one source to a sum of sources, light
the reserved in-bar bright segment for the fraction-of-total sources.
The deleted-open source keeps its beyond-the-bar rendering. One
pipeline, several honesty-tiered sources, each rendered by its own
semantics.

## 9. Honest weaknesses

1. **A wrong-but-plausible number can reach a directory row.** The
   guarantees in §3 shrink the surface but cannot close it: a file
   renamed *within* the scanned tree after the scan snapshot and then
   deleted attributes to its new (correct on disk, unfamiliar to the
   user) directory; a deleted file whose ancestor chain was replaced
   by a same-named directory since the scan attributes to the
   impostor. Both are rare; neither is impossible; the product thesis
   says numbers must be correct where other tools lie, and this column
   is exactly a number that can lie. This is the weakness the
   adversarial pass should press hardest.
2. **Cognitive cost of "+N outside the total".** A column whose bytes
   are additive to the size column, on rows whose bars they must not
   enter, is a subtler contract than anything else in the table.
   Users *will* add the columns up and compare to `df` and sometimes
   be confused anyway (multiple mounts, residual bucket).
3. **More surface now**: `Row`/`DirTotals` fields, snapshot builder,
   column layout and responsive behavior, a sort key, selection-card
   text, the drill-down — roughly double option A's UI work, all
   before the adversarial review has established that per-directory
   attribution is even wanted for phase 1.
4. **Attribution decays across refreshes**: each `r` re-walk can move
   bytes between directories (holder renamed/moved the file) — a
   column that shifts under the user's eyes on a frozen tree erodes
   trust in the *tree*, not just the column.
5. **Tombstone interaction**: after the user deletes a directory
   in-app, its attributed freeable entries must re-attribute to a live
   ancestor on the next refresh — a small but real invariant to test.
6. The `e`-line keys (§7) invite scope creep: the first request for
   "per-directory freeable in dumps" arrives the week after they land.
