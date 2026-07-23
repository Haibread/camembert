# Option C — ghost rows (synthetic entries in the tree)

> Proposal by a design agent instructed to push the "deleted files show
> up where they used to live, as rows" angle as hard as it honestly can.
> Not a decision. Facts referenced from
> [freeable-research.md](freeable-research.md) (§n).

## 1. Pitch

The table *is* camembert's interface: rows are what users sort, click,
select, and reason about. Option C makes each attributable
deleted-but-open file a **synthetic row in its last-known directory** —
`access.log (deleted — nginx, PID 1234)`, dimmed coral, sized, sorted
among its former siblings. Discoverability is maximal and free: no new
panel, no new column, no gauge suffix to notice. The selection card
already shows details for any row; for a ghost it shows holders and the
truncate-through-fd recipe (research §8). An ncdu refugee understands
the feature in one glance because it reuses the only concept the tool
has.

The honest framing: a ghost is not a claim that the file exists — its
styling and name suffix say the opposite — it is a *located* piece of
the df-vs-du gap, displayed at the position the kernel itself reports.

## 2. What phase 1 claims

Sweep core identical to the other proposals (non-negotiable, research
§1/§2/§5): fd-held regular files with `st_nlink == 0` (never the
`" (deleted)"` string), `O_TMPFILE` included, deduped by `(dev, ino)`,
sized `st_blocks × 512`, scoped by `st_dev` ∈ scanned devices,
memfd/shm excluded from all disk figures, mmap-only holders and loop
devices scoped out. Attribution uses the same deepest-live-ancestor
walk as the annotated-tree proposal (final component dropped, raw-byte
component match, device must agree). Unattributable files cannot be
ghosts — they fall back to a single filesystem-level figure on the disk
gauge (there is no honest directory to put them in, and a synthetic
"unattributed" folder under the root would be exactly the invented
entry this option must not create).

## 3. Data model and arena mechanics

Post-scan (frozen arena, single owner — the same phase where
`apply_removal` already mutates):

- For each attributed deleted file, append a node to the arena under
  the deepest live ancestor as a **new child run** (the run-list
  representation, D2, supports appending a run to a completed
  directory — this is the watch-mode insertion path arriving early).
- `NodeFlags` is full (3 bits), so ghost identity lives in a side set,
  the `tombstones`/`hardlink_firsts` pattern:
  `ghosts: FxHashSet<NodeId>` plus a side map `NodeId →
  GhostInfo { dev, ino, holders }` for the selection card.
- **Ghosts contribute zero to every aggregate.** `ta`/`td`/`tn` are
  scanned-filesystem truth and must keep matching `du` and the dump;
  a ghost's size lives on its own row only. Freeable totals are
  computed from the ghost set, not from subtree aggregates.
- Refresh (`r`): tombstone all current ghosts, re-sweep, re-append.
  Arena and interner grow monotonically per refresh (tombstoning
  leaks by design — accepted for deletions, now recurring); a
  pathological refresh loop on a churny container host grows the
  arena without bound.

## 4. Sweep timing

As in the other proposals: once at scan completion on a background
thread (37 ms measured, research §6 — but ghost insertion mutates the
arena, so the *insertion* must happen on the thread that owns the
frozen outcome, i.e. the UI thread, after the sweep thread delivers);
`r` refresh; unfiltered `open_file_index` re-run at delete-confirm time
for the open-file warning (research §7). Never during the scan — the
owner is integrating batches and the arena is not append-safe for
out-of-band rows.

## 5. UI

Almost nothing new, which is the pitch:

- Ghost rows render dimmed in the error-coral family with a
  `(deleted — comm, PID)` suffix; they sort by size like any row;
  the wheel **excludes** them (slices are shares of the directory's
  real total; a ghost slice would misstate every other slice).
- Selection card on a ghost: size, holders, last-known full path, and
  the coverage line (`365 of 505 processes unreadable — run as root
  for the full picture`, research §4).
- Disk gauge: filesystem freeable total including unattributed
  residue. `/proc` absent: no ghosts, no gauge suffix, debug log.
- Marking a ghost for deletion is refused with a toast explaining the
  real fix (restart the holder / truncate via `/proc/PID/fd/N`) —
  refusal-with-education instead of a dead key.

## 6. Dump, diff, and every other consumer

This is where the option pays, and the list must be complete because
each miss is a correctness bug in a *different* subsystem:

- **Dump writer** must skip ghosts (a dump line for a nonexistent file
  corrupts diff and re-import; import→self-diff = zero is a shipped
  invariant). Every future exporter inherits the same obligation.
- **Diff** never sees dumps with ghosts (writer filters), but in-app
  future diff-views over the arena must filter.
- **Deletion**: mark refused (§5); `delete_nodes` defense-in-depth
  must also skip ghosts (its fresh `symlink_metadata` guard would skip
  them anyway — ENOENT — but by accident, not by design).
- **`hardlink_files_in`**, flat view (next feature in HANDOFF value
  order), pattern aggregation, the future filter language: every
  current and future arena traversal needs a "and skip ghosts" clause
  or a filtered iterator. `Tree::children` can centralize it the way
  it centralizes tombstones — but then the freeable UI needs the
  *unfiltered* iterator, so two iterators exist and every call site
  chooses one. A silent wrong choice compiles fine.

## 7. CLI surface

`--no-proc-sweep` (env `NO_PROC_SWEEP`), as in the other proposals.
Documented in `--help` + README in the same change. No ghost-specific
surface.

## 8. Phase-2 growth

Weak, and stated plainly: phase 2's data (btrfs shared extents,
hardlink siblings) attaches to **real entries already in the tree** —
it needs per-entry side data and a bar segment, not synthetic rows.
Ghosts contribute no reusable mechanism to that; the phase-2 plumbing
would be built from scratch next to them. The ghost machinery remains
forever specific to the deleted-open source. (The one reusable piece —
sweep + attribution walk — is common to all three proposals, not a C
advantage.)

## 9. Honest weaknesses

1. **The tree stops being the filesystem.** Every invariant the
   codebase leans on — arena rows correspond to scanned inodes; dump =
   arena; totals = Σ rows — acquires an exception. The §6 list is the
   cost *today*; the real cost is every future feature paying the
   ghost tax or silently miscounting.
2. **Zero-aggregate ghosts make the table self-inconsistent**: a
   directory shows children summing to more than its own total. The
   alternative (counting ghosts into totals) is worse — totals would
   contradict `du`, the dump, and the thesis. There is no rendering
   that makes both the row and the total read honestly at a glance.
3. **Wrong-directory placement puts a fake row in a real directory** —
   strictly worse than option B's wrong number in a real column,
   because a row asserts existence-shaped locality ("this thing is
   here") rather than an annotation. Rename/replacement races
   (research §1, §3) make this reachable.
4. **Refresh leaks arena** (§3) — bounded in practice, unbounded in
   principle, and the first structure in the codebase that grows on a
   user-triggered loop.
5. **Mutating the frozen arena post-scan** widens the one phase whose
   simplicity (single owner, append-only-plus-tombstones) the
   scan-tree design fought for. `apply_removal` was the single public
   mutation; this adds a second, with an insertion path the scan
   engine itself doesn't use yet.
