# Subtree rescan — dossier (2026-11-13)

Decision-ready dossier for
[issue #35](https://github.com/Haibread/camembert/issues/35): refresh one
directory of a finished scan without rescanning the root. **Not settled**
— options, an attack on them, and one recommendation, for a co-design
session. Nothing here is binding until it lands in a decisions doc.

---

## Displacement first

**What does this work displace?** HANDOFF's value-ordered list puts
freeable phase 2 slice 3 first and wave 4 (ssh, HTML export, **watch
mode**, dated cache) second. Subtree rescan sits on the watch-mode path
rather than beside it: watch mode is "rescan the subtrees a change
notification marked dirty, graft the result", and the graft is the part
nobody has designed yet. The scan-tree decisions already name its main
cost: tombstone growth over long sessions, in Attack A finding 8 of
[scan-tree-decisions.md](scan-tree-decisions.md). Building
the graft as a manual, user-triggered action first proves the mutation
without the notification plumbing, and without a session that runs for
hours.

**Does the thesis agree?** The thesis is honest answers to real
questions. "Did the cleanup I just did actually free the space?" is one
of the most common questions a disk analyzer gets, and today camembert
answers it only after a full rescan. On Windows, where in-app deletion
is absent by design (HANDOFF, *The Windows TUI is reduced, not absent*),
every cleanup happens outside the tool, so that loop is the main loop.
The risk to the thesis is the opposite one: a tree that mixes two
instants and does not say so. Every option below is judged against
that.

## What exists to build on

- **The post-scan mutation envelope.** `execute_deletion`
  (`camembert/src/ui.rs`) already does everything around a tree change:
  it cancels the floor pass before taking the write lock on
  `Phase::Done`'s `RwLock<ScanOutcome>`, mutates the tree, bumps the
  flat epoch (flat view, breakdown, filter fold and hardlink index all
  key on it), invalidates oracle caches, clears and respawns the floor,
  and rebuilds the view from the nearest surviving directory. A rescan
  slots into the same envelope; only the mutation in the middle is new.
- **Removal with negative-delta propagation** (`Tree::apply_removal`):
  tombstones a subtree and subtracts its aggregates up the ancestor
  chain. It is half of a graft.
- **Run-list children** (scan-tree D2): a directory's children are a
  list of contiguous runs, so appending a new run of children at the end
  of the arena needs no new structure.
- **A scan can root anywhere.** `Scanner::scan(path)` with the session's
  `ScanOptions` produces a self-contained `ScanOutcome` for any directory.

## What is missing

1. **Positive grafting.** Nothing copies nodes from one arena into
   another, remaps `NodeId`/`DirId`/parent links, re-interns names, and
   carries the side tables (`excluded`, `error_reasons`,
   `hardlink_firsts`) across.
2. **Hardlink groups that straddle the boundary.** Attribution is one
   counted link per `(dev, ino)`, chosen as the canonical (smallest raw
   path, dump D2) by `hardlink::reattribute`. A subtree scan knows only
   the links inside the subtree, so it counts its own first link. If the
   inode also has a live link outside, the grafted tree counts it twice.
   - **Unix** (`LinkPolicy::StatNlink`): every `nlink > 1` inode is
     registered with its `(dev, ino)`, inside and outside, so the graft
     can merge groups by key and re-run canonical selection on each
     merged group. The result is exact for every link the original scan
     registered.
   - **Windows default** (`LinkPolicy::FileIdRepeat`): an inode is
     registered only when one scan reaches it twice. A link inside the
     subtree whose sibling is outside is seen once by the subtree scan,
     so it is never registered, and the merge has no key to match on. The
     fix is to **seed** the subtree scan's `first_by_ino` with the inodes
     of live outside groups, so the first sighting inside counts as the
     repeat.
   - **Both platforms, inherent:** a hardlink created *after* the scan
     between the subtree and the outside is invisible, because the
     outside link was a plain single-link file at scan time and nothing
     records its `(dev, ino)`. The outside is, by definition, not
     re-observed. That is the honest limit of a partial rescan, and it
     must be stated rather than patched.
3. **Saying which part of the tree is newer.** The tree has one
   timestamp (`elapsed`/scan end). Grafted subtrees need a "refreshed at"
   instant that the UI can show.
4. **Marks inside the subtree** (Unix) point at nodes the graft
   tombstones.

## Options

### Option A — full rescan only

A key re-runs the whole scan with the same options and swaps the
outcome. The current directory and cursor are restored by path.

- **For:** no graft, no hardlink merge. The tree is one instant, so
  there is nothing to disclose. A few hundred lines, mostly lifecycle.
- **Against:** it does not answer the issue. On `C:\` it is the
  minutes-long rescan the user is trying to avoid, and the
  browse-during-scan machinery means the session restarts its scan phase
  (tombstoned marks, floor, sweep, toast).

### Option B — subtree graft with hardlink merge *(recommended)*

`r` on a directory row runs `Scanner::scan` on that path in the
background. The old subtree stays visible, with a `rescanning…` badge on
its row and the breadcrumb. On completion, under the write lock:

1. `apply_removal` on the old directory's **children** (not the
   directory itself: its node, name and position in its parent's run
   stay, so the cursor and every ancestor `DirId` stay valid).
2. Copy the fresh arena's nodes below its root into the session arena as
   new runs of the kept directory, remap ids, re-intern names, carry
   `excluded`/`error_reasons`, and add the fresh aggregates up the
   ancestor chain (`apply_delta`, which the scan owner already uses).
   The kept directory's own node takes the fresh root's size and mtime.
3. Hardlinks: drop the tombstoned links from `hardlink_links`, append
   the fresh ones remapped, and re-run canonical selection **only on
   groups that touch the graft**. That is `reattribute`'s per-group body,
   applied to a filtered set, with one extra case: a group whose counted
   link was tombstoned and has no fresh replacement promotes its
   canonical live link (adds its size). On Windows the subtree scan is
   seeded as described above.
4. Bump the flat epoch, invalidate the oracle, respawn the floor and
   rebuild the view, exactly as a deletion does. Marks under the
   directory are dropped, with a toast saying how many.
5. Record `(DirId, Instant)` in a side map. Any directory under a
   refreshed one reports the newest refresh at or above it.

The selection card gains `refreshed 2 min ago` under a refreshed
directory. The scan-time card gains `· N refreshed`. The root itself is
rescanned by swapping the whole outcome, which is option A, so A is a
special case of B and costs nothing extra.

- **For:** answers the issue on both platforms. It reuses the deletion
  envelope wholesale and builds the graft that watch mode needs. Totals
  stay exact for every link the original scan registered.
- **Against:** the largest change here. A new tree mutation kind, plus
  a hardlink merge that needs adversarial tests. The arena grows by the
  subtree size on every rescan (tombstones are not reclaimed), and that
  also spends the 2^26-name interner budget. Repeatedly rescanning a
  multi-million-entry subtree is the pathological case.

### Option C — open the subtree as a separate session

`r` opens a fresh, independent scan rooted at the directory, as a
nested view with its own breadcrumb root. `Esc` back out returns to the
untouched parent session.

- **For:** no graft at all. Each view is one instant.
- **Against:** the parent's totals never change, so it does not answer
  "did my cleanup free space *here*, in this tree". It is the
  `camembert <subdir>` workaround with a nicer exit.

### Option D — graft without the hardlink merge

B without step 3. The subtree is treated as hardlink-isolated, and the
limitation is disclosed.

- **Rejected.** Cross-boundary links are common (package stores,
  `node_modules` deduplicators, WinSxS on Windows), and double-counting
  them is precisely the lie the project exists to not tell.

## Attack on B

1. **Mixed instants.** A parent total is now a sum of a fresh subtree
   and stale siblings. That is true, and B says so twice (card +
   selection card). A stale *sibling* is no less true than it was before
   the rescan. The rescan only removed staleness.
2. **Concurrent change during the rescan.** The background scan runs
   while the user browses. The swap happens under the write lock, and
   the scan reads only the filesystem, never the arena, so there is no
   race on the tree. The filesystem may move during the scan, which is
   true of any scan.
3. **The rescanned directory itself vanished or changed type.** If the
   scan fails at its root (`ENOENT`, `ENOTDIR`, access denied), there is
   no graft. The directory is removed (`ENOENT`) or keeps its old
   subtree with an error toast (anything else). It never becomes an
   empty directory pretending to be fresh.
4. **Excluded rows** (mount points, kernfs) are refused like deletion
   refuses them: their subtree was never scanned under the session's
   rules, so a rescan would change the rules for one row.
5. **`--one-filesystem`.** The subtree scan's boundary is its own root
   device. That equals the session's root device for any non-excluded
   directory under `--one-filesystem`, so the rule is unchanged.
6. **Dumps.** `-o` writes at scan end and is not rewritten. A dump
   records the scan's instant and never claimed to be live, so a later
   rescan does not make it wrong. The README's dump section gains one
   line saying so, so nobody expects a rescan to update it.
7. **Arena growth.** Bounded by user action, not by time. Compaction is
   watch mode's problem, which the scan-tree decisions already filed. B
   logs the arena size at each graft so the cost is visible.

## Recommendation

**B**, with `r` = rescan the directory row under the cursor (file rows
flash, like other keys with no meaning there), available post-scan on
both platforms, in tree view and from flat-view rows via the containing
directory. Root rescan is the same key on the root's breadcrumb (or `r`
from the root view with nothing selected), implemented as the
whole-outcome swap.

Questions for the co-design session:

1. **Key and target.** `r` on the row under the cursor (recommended),
   or `r` on the directory being viewed? The first matches `o`/`y`,
   which act on the cursor row.
2. **Background with old numbers vs blocking modal with progress.**
   Background is recommended: it keeps the tool usable during a long
   rescan, and the browse-during-scan design already proves the
   approach.
3. **Wording of the freshness disclosure** (`refreshed 2 min ago`,
   `· N refreshed`). This is a user-facing surface, so it is the user's
   call.
4. **The Windows seeding.** Seeding `first_by_ino` touches the scan
   owner, which is hot-path code under the benchmark mandate. Accept the
   before/after bench as part of the change, or ship Unix-exact first
   and Windows with a disclosed caveat in a second landing?
