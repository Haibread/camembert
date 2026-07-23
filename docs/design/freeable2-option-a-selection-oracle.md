# Option A — selection oracle only (exact on demand, nothing ambient)

> Proposal by a design agent instructed to push the "freeable-on-delete
> is a property of a selection, not of an entry — so compute it only for
> selections, exactly, and refuse every ambient number" angle as hard as
> it honestly can. Not a decision. Facts referenced from
> [freeable2-research.md](freeable2-research.md) (§n) and phase-1
> decisions ([freeable-decisions.md](freeable-decisions.md), D-n).
> Written to be attacked stand-alone.

## 1. Pitch

The research's central structural finding (§3, §4, open question 2) is
that phase 2's quantity is **a function of which nodes are co-selected**:
a shared extent frees only when its last referencer goes, a hardlinked
inode frees only when its last link goes, and both facts depend on the
rest of the selection, not on the entry alone. That is naturally a
"recompute for this selection, on demand" shape — and Option A takes
the shape at face value: **phase 2 ships exactly one mechanism, the
selection oracle**, and no per-entry number at rest.

What the user gets: the moment freeable actually matters — an entry is
selected, entries are marked, the delete-confirm is open — camembert
computes, fresh, the exact answer for *that* set: "deleting this frees
at least X, up to Y; Z won't be freed, and here is why." No background
pass, no side map, no staleness, no memory bill, no number anywhere in
the tree that could ever be questioned. The reserved in-bar bright
segment (tui-design reservation 2) lights up **only on rows the oracle
has actually measured this epoch** — the selected row and marked rows —
and stays dark elsewhere, which A argues is the honest reading of the
reservation: the bright fraction appears when it is *known*, not when
it is estimated.

The implicit claim to attack: ambient per-entry freeable — even a
mathematically safe lower bound — buys navigation-time comfort at the
price of a number that (a) under-sells the very trees phase 2 exists
for (snapshot farms show "≥ 0" everywhere), (b) goes stale the moment
any process writes anywhere (§7), and (c) costs a full-tree ioctl pass
plus tens of MB of RSS. A says: skip it entirely; exactness-on-demand
is the whole feature.

## 2. Semantics — the oracle's contract

For one fixed selection S (the marked set, or the single selected row
treated as a one-entry selection), every allocated byte of S's files is
placed in exactly one bucket:

1. **exclusive** — extent with `FIEMAP_EXTENT_SHARED` unset: freed,
   guaranteed (§1);
2. **selection-shared** — `SHARED` set, ≥ 2 in-scan referencers, all
   inside S: freed **unless** something outside the scan also
   references it, which is undetectable unprivileged (§3:
   `LOGICAL_INO` is EPERM); reported as "up to", never promised;
3. **held elsewhere in the scan** — a referencer inside the scanned
   tree survives outside S: not freed;
4. **shared outside the scan** — `SHARED` set, sole in-scan referencer:
   a snapshot / excluded subtree / other subvolume holds it: not freed;
5. **unknown** — delalloc/unknown extents (no `FIEMAP_FLAG_SYNC`, §1),
   files vanished or unopenable since the scan: excluded from every
   figure, one honest line.

Hardlinks (§4): a `(dev, ino)` group participates only when S contains
**every link the scan saw** and the scan saw **every link that exists**
(`group.len() == st_nlink` — the registry already holds both facts).
Otherwise the group lands in the "won't be freed" list with its reason
("2 links outside the selection", "1 link outside the scan").

Headline: **"frees at least ⟨1⟩, up to ⟨1+2⟩"**. Buckets 3/4 named with
counts and bytes. Correlation is per-device interval arithmetic at
4 KiB granularity over `(st_dev, fe_physical, fe_length)` — never
across devices, partial overlaps merged (the `btrfs fi du` set-shared
mechanism the research reproduced unprivileged, §3).

Crucially, **there is one important simplification A gets for free**:
because the oracle FIEMAPs *only* S, it cannot distinguish bucket 3
from bucket 4 (both look like "shared, not fully explained within S").
A embraces the merged form — "Z is shared with files or snapshots
outside your selection; deleting the selection will not free it" —
which is true, sufficient for the decision at hand, and requires no
scan-wide extent map. (Option B needs extra machinery to even try the
split; A's honest merge is the simpler contract.)

### The non-additivity answer

Attack B ([freeable-attack-b.md](freeable-attack-b.md)) killed
per-directory freeable scalars because no scalar per entry can express
a selection-dependent quantity. A's answer is the purest available:
**no scalar per entry exists**. Every number the feature ever shows is
(a) scoped to one explicitly-named selection, (b) computed exactly for
it, (c) fresh at computation time, and (d) labeled with what it could
not see. `freeable(A ∪ B) ≠ freeable(A) + freeable(B)` is not a trap A
must engineer around — the user is never shown `freeable(A)` and
`freeable(B)` side by side as comparable static columns in the first
place; they are shown the answer for whatever they actually marked.

## 3. Data model

New module `camembert-core/src/freeable2.rs`. Zero changes to the
32-byte `Node`, `tree.rs`, snapshots, dump, diff (phase-1 D8 pattern).
The only persistent state is a tiny per-session result cache:

```rust
/// Result of one oracle run, kept only for rendering (selection card,
/// basket strip, in-bar segments on measured rows). Invalidated by
/// (selection fingerprint, deletion epoch) — the query-engine D5
/// staleness pattern.
pub struct OracleResult {
    pub selection: SelectionFingerprint,
    pub epoch: u64,
    pub guaranteed: u64,        // bucket 1
    pub selection_shared: u64,  // bucket 2 ("up to" − guaranteed)
    pub held_outside: u64,      // buckets 3+4, merged (§2)
    pub unknown_bytes: u64,
    pub unknown_files: u64,
    /// Per-node guaranteed bytes for the rows in S — feeds the bright
    /// segment on measured rows only. Bounded by |S|.
    pub per_node: FxHashMap<NodeId, u64>,
    pub hardlink_notes: Vec<HardlinkNote>, // "won't be freed" reasons
    pub computed_at: SystemTime,
}
```

Memory: O(|S|), transient. No FloorMap, no 40–80 MB side vec, nothing
scale-dependent at rest.

## 4. Lifecycle

Triggers:

- **selection card**: moving the cursor onto a *file* row runs a
  single-file oracle automatically — one open+FIEMAP+close is 6–15 µs
  (§1), debounced with cursor motion; the card shows
  "excl ≥ 1.2 GiB of 4.5 GiB · shared 3.3 GiB". A *directory* row is
  not auto-computed (its subtree may be millions of files); the card
  shows "press x for exact freeable (~N s estimate)" with the estimate
  derived from `tn` × per-file rate.
- **basket**: any basket change re-runs the oracle for the marked set,
  debounced ~300 ms, auto up to ~50k files (≈ 0.5 s at the measured
  ~10 µs/file rate, §1/§3); above that, explicit (`x`) with a progress
  line.
- **delete-confirm**: always re-runs fresh for the marked set (the D4
  pre-deletion-refresh slot), filling the modal asynchronously —
  **advisory, never blocking confirmation** (D6 precedent).

Execution: the spawn+channel idiom (`ui.rs` filter fold). The thread
takes the post-scan `RwLock` read guard in chunks (extract paths +
`(dev, ino, nlink, disk)` for ~10k files, drop the lock, ioctl
lock-free) so a concurrent deletion's write lock never waits on a long
selection. Results stamped `(fingerprint, epoch)`; stale never renders.
`ENOENT`/`EACCES`/`EOPNOTSUPP` per file → bucket 5.

Freshness is A's quiet superpower: extent sharing is volatile in a
strictly stronger sense than `/proc` state (§7 — any write by any
process, snapshots by backup jobs, `duperemove` runs), and A never
shows a number older than the last debounce window. There is no
staleness display because there is nothing stale to display.

## 5. UI surface

- **Selection card**: the primary home (per-row question, per-row
  answer). File rows: automatic, instant. Dir rows: on demand with a
  cost estimate — the honest way to expose that exactness has a price.
- **In-bar bright segment**: rendered only on rows with a `per_node`
  entry in the current `OracleResult` (marked rows + measured selected
  row), as `guaranteed/disk` of that row. Everywhere else the bar
  stays exactly as today. Presence rule is stable ("segments appear on
  measured rows") — no per-directory flicker (attack-b amendment 6).
- **Basket strip**: "5 marked · 12.3 GiB · frees ≥ 8.1 GiB (up to
  11.9)" / "computing…".
- **Confirm modal**: the range + the "won't be freed" lines (reasons:
  shared outside selection/scan, hardlinks elsewhere) + the unchanged
  phase-1 open-file advisory. This *replaces* phase-1's optimistic
  freed estimate with an honest range — the single biggest user-visible
  win, and it ships in slice 1.
- **No column, no sort key, no gauge line.** "Where can I reclaim?"
  remains answered by the size column + marking; A explicitly declines
  to rank rows by a freeable-flavored number (attack-b finding 2
  honored in the strongest possible form: no such axis exists).
- **Compression caveat**: when a scanned device's mount options carry
  `compress` (`/proc/self/mountinfo`, read once), oracle outputs
  append: "compressed filesystem: figures are allocated (uncompressed)
  bytes — physical reclaim may be smaller; the kernel does not expose
  compressed sizes to unprivileged users" (§2). Same unit as the
  `disk` column, same pre-existing blind spot, worded not fixed.
- **Flat/filter modes**: marks work on real rows in both modes
  already; the oracle follows the basket, so `t`-mode marking gets the
  same figures for free. No per-group or per-filtered-dir floors exist
  to compose — nothing to do.

## 6. Filesystem tiers

Per unique `st_dev`, one post-scan `fstatfs` (the `classify_mount`
idiom; magics verified in §5):

- **btrfs / XFS** (tier F): full oracle. First FIEMAP failing with
  `EOPNOTSUPP` downgrades the device to tier H.
- **ext family and other real filesystems** (tier H): hardlink subset
  check only (registry data, no ioctls); extents reported as "no
  sharing on this filesystem" and the range collapses to a single
  exact figure — on ext4 an `nlink == 1` file's deletion frees its
  blocks, full stop (§5; deleted-but-open is phase 1's separate
  advisory).
- **ZFS** (tier Z, magic unverified live §5): the oracle refuses with
  one line — "ZFS does not expose per-file sharing (block cloning is
  pool-level); no freeable figure rather than a guess" (§5). The
  hardlink-only fallback is deliberately *not* offered on ZFS: block
  cloning can make even an `nlink == 1` file free ~nothing, so a
  tier-H figure could overstate — the settled "show nothing rather
  than invent" stance.

Root-only precision (`TREE_SEARCH_V2` compressed truth,
`LOGICAL_INO` referencer lists): not in A; carried in the dossier as a
session decision with a reject recommendation (the privilege wall is
total on the reference machine, §3).

## 7. Deletion integration

The confirm modal is A's flagship surface (§5). After a confirmed
deletion, `apply_removal` runs exactly as today; the oracle result is
invalidated by the epoch bump and the basket is empty — nothing to keep
consistent, because nothing ambient exists (attack-b amendment 3 is
satisfied vacuously). Surviving reflink siblings' exclusivity changes
on disk within microseconds (§1) and the next oracle run sees it fresh.

## 8. Dump / diff / CLI

Nothing in dumps (extent state is more volatile than `/proc` state, §7;
D7's capability argument applies verbatim), nothing in diff. CLI
surface: **zero new flags** — there is no background pass to disable;
an environment that dislikes ioctls simply never marks anything.
(`--no-ui` prints nothing new; a non-interactive selection semantics
would be wave-3 query-language territory, named and deferred.)

## 9. Costs

| operation | cost |
|---|---|
| file-row card figure | 6–15 µs (§1) — imperceptible |
| 1k-file selection | ~10 ms |
| 50k-file auto cap | ~0.5 s (background) |
| 1 M-file marked dir (explicit) | ~10 s with progress (§3 field rate ≈ 13.5 µs/file) |
| memory at rest | 0 |
| scan-time / scan-end impact | 0 |

Pathological tail: one 801-extent file ≈ 1.2 ms (§1); bounded by the
pagination loop, no `SYNC` stalls ever.

## 10. Phasing

A **is** the smallest honest slice — it is literally slice 1 of the
other options:

1. Oracle engine + confirm-modal range + basket figure.
2. Selection-card auto figure for file rows + `x` for dirs + measured-
   row segments.

If a later session decides ambient floors are wanted after all, A's
engine is the substrate both B and C build on — nothing is unbuilt.

## 11. Where to press (self-identified)

1. **Reservation 2 stays mostly dark.** tui-design reserved the in-bar
   bright segment as "the UI home of the libérable ≠ taille thesis";
   A's measured-rows-only reading arguably under-delivers the design's
   intent — most bars never show a segment. Is a thesis whose UI home
   is empty until the user marks something actually surfaced?
2. **"Where can I reclaim?" is unanswered.** A user staring at a
   snapshot-heavy tree has no ambient signal that 90 % of a subtree is
   shared; they must suspect it, mark, and ask. B's floor at least
   shows suspiciously dark segments. A trades discovery for purity —
   attack whether the trade is right for the product's audience.
3. **Auto-oracle on cursor motion** (file rows) reintroduces per-
   keystroke ioctls on the UI's hot path (debounced, off-thread, but
   still a novel per-navigation side effect — cf. the idle-quiescent
   polling design). Press on jitter and on network filesystems where
   a single open+ioctl might not be 10 µs.
4. **The 50k auto-cap and the cost estimate** are magic numbers derived
   from one machine's measurements (§1, §3). A slow HDD or a
   fragmented tree can blow the "~0.5 s" promise by an order of
   magnitude; the estimate line (`tn × rate`) needs honest error bars
   or adaptive calibration.
5. **Merged buckets 3/4** ("shared with files or snapshots outside
   your selection") is honest but coarse: a user deciding between
   "mark the sibling too" (bucket 3 — actionable) and "give up, it's a
   snapshot" (bucket 4 — not actionable) gets no help. That
   distinction is exactly what the eager options can offer; press on
   whether A's contract is *too* minimal at the decision moment.
6. **Repeated work**: every basket change re-FIEMAPs the whole
   selection (no incremental reuse across debounce windows). For a
   user toggling marks inside a 50k-file dir, that is ~0.5 s of ioctls
   per toggle. An incremental per-(dev,ino) cache within one epoch is
   an obvious amendment — but then staleness (A's superpower is having
   none) creeps back in. Attack the purity/efficiency boundary.
