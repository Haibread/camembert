# Adversarial review — Option C (ghost rows)

> Target: [freeable-option-c-ghosts.md](freeable-option-c-ghosts.md).
> Method: every load-bearing claim checked against the real
> `camembert-core`/`camembert` code (`tree.rs`, `view.rs`, `dump.rs`,
> `delete.rs`, `ui.rs`, `ui/state.rs`, `ui/theme.rs`, `ui/wheel.rs`), not
> against the option's own prose.
>
> **Verdict: does NOT survive as pitched. The "almost nothing new"
> framing is false — the option needs a new public core mutation API, a
> per-row ghost flag threaded through the snapshot, and a skip-ghost
> clause in five current consumers plus every future one. Survivable only
> by becoming a substantially larger feature than advertised, at which
> point its one selling point over the gauge/annotation options — "reuses
> the only concept the tool has, for free" — is gone. Recommend: do not
> adopt for phase 1.** Amendments that would make it *correct* (not
> *cheap*) are listed at the end.

The single sharpest fact: the option's whole mechanism ("append a run to
a completed directory — the watch-mode insertion path arriving early",
§3) rests on arena primitives that **do not exist as public API**, and
the row it inserts must be filtered back out by every consumer that isn't
the freeable UI. It buys one thing (discoverability) by taxing everything
else, and the tax is larger and more error-prone than §6/§9 admit.

## Findings (severity-ranked)

### FATAL-1 — the insertion path the pitch depends on is not reachable from where the pitch says it runs

§3/§4: ghosts are appended "on the thread that owns the frozen outcome,
i.e. the UI thread, after the sweep thread delivers", using "the run-list
representation (D2) [which] supports appending a run to a completed
directory."

The three arena mutators that would do this —
`Tree::push_node` (`tree.rs:583`), `Tree::add_dir` (`tree.rs:619`) and
`Tree::push_run` (`tree.rs:643`) — are all `pub(crate)`. The UI thread
lives in the **`camembert` crate**; the arena lives in
**`camembert-core`**. The frontend physically cannot append a node or a
run. `NodeId::from_raw` exists but is documented "intended for frontends
building synthetic rows/snapshots **in tests**" (`tree.rs:56-62`), and a
`from_raw` id points at nothing in the real arena, so `build_snapshot`
(`view.rs:177`, which iterates `tree.children`) would never yield it.

So the ghost cannot be a frontend-only construct. It must be a **real
arena node**, inserted by **new public core API** (something like
`ScanOutcome::insert_ghost(ancestor, name, size, GhostInfo) -> NodeId`),
which must: intern a unique name, `push_node` under the deepest-live
ancestor, `push_run` a (1-length) run onto that ancestor's `DirMeta`,
insert into a new `ghosts: FxHashSet<NodeId>`, and populate a
`NodeId -> GhostInfo` side map. That is a second public mutation of the
"frozen" arena with an insertion shape the scan engine itself never uses
(§9-5 concedes this but files it under "honest weakness", not "the
mechanism doesn't compile as described"). The pitch's "almost nothing
new, which is the pitch" (§5) is contradicted by its own §3.

Failure scenario: an implementer takes §3 at face value, tries to build
ghosts in `ui.rs` after `LiveScan::join`, and discovers there is no way
to get a row into a `ViewSnapshot` without new core surface — the design
has to be reopened at implementation time, which is exactly what a design
doc is supposed to prevent.

### FATAL-2 — a ghost is markable and deletable today, and a name-reuse collision turns that into deletion of an unrelated real file

§5 promises "marking a ghost for deletion is refused with a toast." The
actual mark path does not refuse it. `UiState::toggle_mark`
(`state.rs:550-580`) has exactly two guards: `ScanRunning`, and
`MountPoint` which fires only for `row.is_dir && row.dir.is_none()`
(`state.rs:559`). A ghost is a `File`-kind row with `dir == None` and
`is_dir == false`, so **neither guard fires** — `toggle_mark` inserts it
into `marked_set` (`HashSet<NodeId>`, `state.rs:256`) keyed by the
ghost's real `NodeId` and returns `Ok(())`. The refusal in §5 is
unimplemented, and `MarkRefusal` (`state.rs:86-93`) has no variant for
it. To honour §5 you need: a new `Row.is_ghost` field (the `Row` struct,
`view.rs:60-83`, has none), plumbed from a new `Tree::is_ghost` through
`build_snapshot`; a new `MarkRefusal::Ghost`; and a new branch in
`toggle_mark`. Nothing in the current stack short-circuits a ghost mark.

Now the amplifier. `path_of_node` (`tree.rs:428`) builds a path from the
parent chain + name. A ghost's parent chain is its deepest-live ancestor,
so `path_of_node(ghost)` = `<ancestor>/<ghost-name>`. Research §1/§3
establish that the last-known name can be **reused**: delete an open
file `/scan/dir/victim`, then a *different* real file is created at
`/scan/dir/victim`. The attribution walk places the ghost under
`/scan/dir` with name `victim`; `path_of_node(ghost)` now equals the path
of a **live, unrelated inode**.

`delete_one` (`delete.rs:173`) does `fs::symlink_metadata(path)`
(`delete.rs:187`). §6 claims the delete path skips ghosts "by accident"
via ENOENT. In the collision case it is **not** ENOENT: the real
`victim` exists, `kind_matches` passes (both `File`), and
`fs::remove_file(path)` (`delete.rs:207`) **deletes the real file**. The
only thing preventing this is the mark-time refusal — which, per the
paragraph above, does not currently exist and is guarded by must-be-
perfect new code. A single missed branch is silent data loss of a file
the user never saw as deleted. Option B's worst case is a wrong *number*;
this is a wrong *unlink*.

### SERIOUS-3 — zero-aggregate ghosts make the per-row `%`, the bar, the wheel and the identity palette individually wrong unless each is special-cased

§3: "Ghosts contribute zero to every aggregate." Achievable (just don't
call `apply_delta`), but the consequence lands in four independent render
paths, none of which know what a ghost is:

- **`%` column** (`ui.rs:1274-1283`): `frac = row.disk / parent_disk`
  where `parent_disk = snapshot.totals.disk` = the viewed dir's `td`,
  which excludes the ghost. A 2 GiB ghost inside a 1 GiB directory prints
  `213.7%`. The bar clamps (`proportion_bar`, `wheel.rs:304`), but the
  text does not. §9-2 waves this off as "there is no rendering that makes
  both read honestly"; the concrete artefact is a literal >100% cell.
- **identity ranks** (`ui.rs:926-927`): `assign_identity(&disks, 9)`
  (`theme.rs:297`) ranks the top-9 rows by `disk`, in snapshot order,
  with no ghost awareness. A large ghost takes **rank 0**, so (a) it is
  painted an identity colour, directly contradicting §5's "dimmed coral,
  not identity colour", and (b) it **displaces a real sibling** out of
  the 9-colour identity set, changing how the real directory is coloured.
- **wheel** (`ui.rs:1446-1450`): `slice_rows` is built from
  `rows_indexed()` — every row, ghosts included — and fed to
  `build_slices` (`wheel.rs:247`). §5 says "the wheel excludes them"; the
  code includes them. Excluding requires ghost-awareness in *both*
  `assign_identity` and the `slice_rows` construction, or the ghost eats
  a slice and every real slice is misstated (the exact failure §5 says it
  is avoiding).
- **selection card** (`ui.rs:1360-1416`): fixed two-line layout —
  size, `% of parent`, `modified <age>`, `N items`, errors. §1/§5 say the
  card "shows holders and the truncate-through-fd recipe." It shows none
  of that and has no access to `GhostInfo`; `Row` doesn't carry holders,
  `state.rs` holds a `ViewSnapshot`, not the `Tree`. The card's `% of
  parent` line also reprints the >100% number. "The selection card
  already shows details for any row" (§1) is true only for the details a
  real row has.

Each of these is a separate edit in a separate module; none is covered by
"almost nothing new."

### SERIOUS-4 — deleting a ghost's containing directory tombstones the ghost, silently dropping freeable that was not actually freed

§3: "Freeable totals are computed from the ghost set." §6: `delete_nodes`
tombstones a removed directory's whole subtree — `apply_removal`'s subtree
loop (`tree.rs:520-532`) walks `self.children(d)` and tombstones every
descendant. Ghosts are appended as children of a live directory, so they
**are** in that subtree and **will** be tombstoned when an ancestor is
deleted.

But the deleted-open file the ghost represents is held by a running
process; removing its last-known *directory* frees nothing about it (the
inode stays pinned by the fd). So the freeable figure must **not** drop
when the ghost's ancestor is deleted. If freeable is derived from "live
(non-tombstoned) ghosts" — the natural reading of "computed from the
ghost set" — it drops incorrectly the moment a user deletes the
containing directory. Correct behaviour needs freeable to track the
`GhostInfo`/`(dev,ino)` set *independently* of tombstone state, i.e. the
ghost set is decoupled from the arena it was inserted into — which
further undercuts the "it's just a row in the tree" thesis.

### SERIOUS-5 — the "every other consumer must filter ghosts" list is real, cannot be centralised, and the dump writer emits ghosts as-is until it is

§6 is honest that each consumer needs a skip clause; the code confirms the
trap and sharpens it:

- `Tree::children` (`tree.rs:397`) is the **single** filter point for
  tombstones, precisely so consumers don't each reinvent it. Ghosts
  cannot join that filter, because the freeable UI needs the *unfiltered*
  view — so you get two iterators and every call site picks one, "a
  silent wrong choice compiles fine" (§6, correct).
- **Dump writer**: `write_records` collects `tree.children(dir)`
  (`dump.rs:190`) and emits every non-dir child as an entry line. With no
  ghost filter it writes ghost entries into the `.cmbt`, corrupting
  self-diff-is-zero and re-import (§6-bullet-1). This is load-bearing:
  the dump is the interchange format; a ghost line is a lie about a file
  that does not exist.
- `hardlink_files_in` (`delete.rs:247`) happens to be *safe by accident*
  — it only counts nodes where `is_hardlink` is true, and a ghost
  (`st_nlink == 0`, no `HARDLINK_EXTRA`, not in `hardlink_firsts`) is
  never a hardlink. Fine today; fragile as a pattern ("safe by accident"
  is how FATAL-2 also nearly passes).
- **flat view** (the next milestone in HANDOFF value order): a global,
  size-sorted list is the single most hostile consumer. A 2 GiB ghost
  sorts near the top of the *whole scan*, stripped of the directory
  context that made the dimming legible, and any `Σ node.size()` total
  over the flat list double-counts it. This is the strongest instance of
  §9-1's "every future feature paying the ghost tax", and it lands
  immediately after phase 1.

### ANNOYING-6 — the refresh leak is worse than "bounded in practice"

§3/§9-4: refresh tombstones current ghosts and re-appends; "bounded in
practice, unbounded in principle." Three compounding costs the option
doesn't quantify:

- **Interner**: ghost names are effectively unique (`comm` + `PID` +
  path), so every refresh mints new interned names. The interner caps at
  **2^26 unique names** and panics past it (`tree.rs:30`, `42-43`,
  `175-176`) — the documented Maildir worst case, now driven by a
  user-repeatable key press on a churny host.
- **Run-list bloat**: each refresh appends new runs to the ancestor's
  `DirMeta.runs` and tombstones the old ghost nodes, but the dead runs
  **stay** in `runs`. `Tree::children` (`tree.rs:397`) flat-maps every
  run and filters tombstones on every call. After N refreshes an ancestor
  carries N generations of dead ghost runs, and every post-scan
  `build_snapshot` of that directory pays O(dead ghosts) to filter them
  back out — a per-navigation cost that grows on a user loop.
- **Node arena**: monotonic growth toward the 2^32 cap
  (`tree.rs:30-31`), never reclaimed.

"The first structure in the codebase that grows on a user-triggered loop"
(§9-4) is right; the magnitude (interner cap + per-nav run-scan) is
undersold.

### ANNOYING-7 — §1 and §5 contradict each other on the disk gauge

§1 sells discoverability as free precisely because there is "no gauge
suffix to notice." §2/§5 then require unattributable residue to land on
"the disk gauge: filesystem freeable total including unattributed
residue." So Option C needs the gauge freeable figure **as well as** the
ghost rows — it does not avoid the gauge work the annotation/gauge options
need, it adds rows on top of it. The "for free / no gauge" pitch is
internally inconsistent with its own fallback.

### COSMETIC-8 — name/suffix storage and default cursor

- `Row.name` is raw bytes (`view.rs:63`) and the interned node name is
  the sort key (`ensure_sorted`, `state.rs:736-741`; dump sibling order,
  `dump.rs:191`). Baking `" (deleted — nginx, PID 1234)"` into the name
  corrupts sort order and, worse, `path_of_node` (feeding FATAL-2). Keeping
  the suffix out of the name needs yet another `Row` field and render
  branch. Either way, sort-by-name places ghosts arbitrarily.
- Sort is disk-descending by default (`SortKey::Disk`, `state.rs:45`). If
  the largest child is a ghost, the **default cursor and selection card
  land on a dimmed "deleted" row** on entering the directory — the first
  thing the user reasons about is a non-existent file.

## On the honesty question the brief poses

"A dimmed 2 GiB row inside a directory whose total does NOT include it —
more or less honest than not showing it?" Less. A row is the tool's unit
of *existence-shaped locality*: it is sorted by size, carries a size, sits
among real siblings, and drives the same selection card. Dimming + a name
suffix is a weak counter-signal against that strong frame, and §3's
zero-aggregate rule forces the contradiction into the user's face as a
`213.7%` cell (SERIOUS-3). Option B's wrong-number-in-a-real-column is a
smaller lie than Option C's confidently-placed row — and Option B's lie
can't get an unrelated file unlinked (FATAL-2).

## Survived

- **The sweep + attribution core** (§2) is sound and identical across all
  three proposals — not a Option-C advantage, as §8 itself admits.
- **Zero-aggregate is achievable** (don't call `apply_delta`); the
  totals-match-`du`-and-dump invariant is preserved *at the aggregate
  level*. The cost is that four render paths and every consumer must now
  special-case a row that contributes zero (SERIOUS-3, -5).
- **Unattributable → no ghost** (§2) is the right call; the dangerous
  case is attributable-but-wrong (FATAL-2), not unattributable.
- **`delete_nodes`' fresh `symlink_metadata`** genuinely blocks the
  common (ENOENT) ghost-deletion case — just not the name-reuse case
  (FATAL-2), and only "by accident" (§6, honestly flagged).
- **Phase-2 assessment** (§8): the option's own "weak, and stated
  plainly" is accurate. Ghosts attach nothing reusable to btrfs-extent /
  hardlink-sibling work, which decorates real entries. Confirmed:
  `top_dirs_by_disk`/`by_errors` (`scan.rs:622`, `634`) iterate
  `dir_ids()` and never see file-kind ghosts, so the phase-2 dir-level
  machinery is orthogonal to — and unhelped by — the ghost set.

## Amendments that would make it correct (not cheap)

If adopted anyway, all of the following are mandatory, not optional:

1. New **public** core API to insert ghosts (node + run + `ghosts` set +
   `GhostInfo` map), owning a second post-scan mutation path (FATAL-1).
2. `Row.is_ghost` (+ `Tree::is_ghost`) threaded through `build_snapshot`,
   and a `MarkRefusal::Ghost` branch in `toggle_mark` **before** any other
   guard (FATAL-2). Deletion-side: an explicit ghost check in
   `apply_removal`/`delete_one`, not the accidental ENOENT (FATAL-2).
3. Ghost-awareness in `assign_identity`, the `slice_rows` build, the `%`
   cell, and a dedicated ghost variant of the selection card carrying
   `GhostInfo` (SERIOUS-3).
4. Freeable total tracked off the `(dev,ino)`/`GhostInfo` set,
   **decoupled from tombstone state**, so deleting an ancestor doesn't
   drop it (SERIOUS-4).
5. Skip-ghost clauses in the dump writer and every future arena
   traversal, with a clearly-named unfiltered iterator reserved for the
   freeable UI (SERIOUS-5).
6. A refresh strategy that bounds interner + run-list growth (e.g.
   compact ghost runs on refresh, or store ghosts outside the arena
   entirely — which is arguably the admission that they should never have
   been rows) (ANNOYING-6).

Once 1–6 are done, the ghost is no longer "the only concept the tool has,
reused for free" — it is a parallel, filtered-everywhere subsystem with a
data-loss failure mode guarded by new code. At that point the gauge /
annotation options deliver the same discoverability for less surface and
without FATAL-2. **Kill for phase 1.**
