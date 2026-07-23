# Adversarial review — Option B (annotated tree)

> Verdict: **SURVIVABLE WITH AMENDMENTS** — no corruption of the
> authoritative numbers (arena frozen, `ta`/`td` untouched, single owner
> post-scan), so nothing here is *fatal* the way the shared-arena race
> was. But B's headline card (§8 "build the channel once for phase 2") is
> **backwards**, and its most visible new surface — a `+N` column with a
> dedicated **sort key** — promotes a best-effort guess to ranking
> authority, in direct tension with the product's "correct where others
> lie" thesis. Amended, B collapses to "Option A plus an opt-in column,"
> which is the honest thing to call it.

Facts cross-checked against `freeable-research.md`, `tree.rs`,
`view.rs`, `ui.rs`, and `dump-v1.md`. Severity tags: **[FATAL] /
[SERIOUS] / [ANNOYING] / [COSMETIC]**.

## What genuinely survived (credit first)

These are load-bearing and they hold:

- **`st_dev`-set membership as the scope decision, path text only for
  display** (proposal §2, §3 step 4). This is the single most important
  robustness choice and it works: a mis-parsed, aliased, or lying path can
  misplace bytes *within* the tree or drop them to residual, but it can
  **never corrupt the filesystem-total** freeable figure. The blast radius
  of every path attack below is capped at "wrong row / residual," never
  "wrong total."
- **`(unreachable)` rejection + per-entry `dev == matched-dir dev` check**
  (§3 steps 2, 4) genuinely defuse the container cases: a `pivot_root`
  runtime renders host-side reads `(unreachable)` (rejected); a
  cross-device bind mount whose path prefix textually matches the scan root
  is caught by the dev mismatch (the file's `st_dev` is the bind source's,
  the scan-tree node's `dev` is the root's). The prompt's "browser in a
  flatpak / bind-mount prefix collision" scenario is *defended*, not open.
- **Side-map shape mirrors the `excluded` pattern** (`tree.rs:322`),
  stays out of the 32-byte `Node`, and is written single-owner post-scan.
  Unlike shared-arena-B, there is **no new concurrency surface** — this is
  the right integration idiom for this codebase.
- **Wholesale recompute on refresh** (§3) sidesteps the incremental
  negative-delta bugs that plague `apply_negative_delta`; the cost
  (tens of entries × depth) is real and trivial.
- Sweep core is correct: `st_nlink == 0` ground truth, `(dev,ino)` dedup,
  `st_blocks × 512` sizing, memfd/shm exclusion (research §1, §2, §5).

## Findings

### 1. Phase-2 rationale is backwards — B's strongest card is its weakest [SERIOUS]

§8 is the pitch's structural bet: build the additive per-directory channel
now, phase 2 "adds two more sources to an existing pipe." **The channel
shape does not fit phase-2 data.**

Phase-1 freeable is a *disjoint whole inode* attributed to one directory;
summing those up the ancestor chain (the proposal's whole mechanism,
mirroring `tree.rs:apply_delta`) is correct because the sets are disjoint.

Phase-2's two named sources are **non-additive by construction**:

- **btrfs `FIEMAP_EXTENT_SHARED`** is a *per-file fraction*, and the
  freeable-on-delete of a file is **not** its shared bytes — a shared
  extent frees nothing until its *last* referencer goes. "Freeable by
  deleting subtree S" = bytes in extents referenced *only* by files in S:
  an inclusion-exclusion / set-cover computation over the extent→referencer
  map. Two sibling subtrees can share extents, so
  `freeable(A ∪ B) ≠ freeable(A) + freeable(B)`. You cannot obtain
  subtree-freeable by adding per-directory partial sums up the chain
  without double-counting.
- **Hardlink siblings**: the same non-additivity. The tree *already* models
  this correctly (first-seen attribution + `HARDLINK_EXTRA`,
  `tree.rs:122`, and the deletion dialog's own "frees nothing unless last
  link" warning). Freeable for a linked inode is a function of whether the
  *whole* link set is inside the selection — not a per-directory scalar.

Concrete: `/data/snapA` and `/data/snapB` are btrfs reflink snapshots
sharing 90 GiB of extents. Under an additive `per_dir`, each shows
`+90 GiB` freeable and the parent shows `+180 GiB`. True freeable of
deleting *either* alone: ~0. Deleting *both*: 90 GiB. The additive channel
cannot express any of those three numbers.

So phase 2 does **not** "add a source to the pipe" — it needs a *different*
aggregation (per-selection, not precomputed per-dir). Pre-building the
additive channel does not de-risk phase 2; it entrenches a scalar model
that phase 2 must tear out. B's headline justification should be **struck**,
and B judged on phase-1 merits alone — at which point it is "A + a column."

### 2. The sort key promotes a best-effort guess to ranking authority [SERIOUS]

The opt-in, `+`-prefixed, only-when-nonzero *column* is a defensible
best-effort hint. `SortKey::Freeable` is not. Sorting reuses the exact
machinery that makes `real`/`apparent` sorts trustworthy — the arrow
glyph (`ui.rs:1220`), the reorder, the identity colors — and points it at
a number that §9 pt1 admits can be wrong. The user has **no in-band signal**
that this ranking axis is epistemically weaker than the others in the same
sort menu (`state.rs:736`).

Worst interaction, combining with finding 4: the user asks the tool "where
are my freeable bytes," it sorts, and puts a **wrong** row
(`…/base/16384 +30 GiB`, an impostor-recycled Postgres OID dir) at the top
with full visual authority. That is precisely the "tool that lies" failure
the product exists to prevent — and B has built a keystroke to surface it.

Compounding: the honesty mechanism (gauge split "1.8 shown / 0.2
unattributed", §6) lives in a **different screen region** (`draw_disk_gauge`,
`ui.rs:1159`) from the column (`draw_table`, `ui.rs:1207`). A user reading
the sorted top row does not see the caveat in the same glance. The number
and its disclaimer are spatially divorced.

Amendment: drop `SortKey::Freeable`, **or** make the residual/unattributed
bucket a real table row so sorting cannot hide the un-placed mass, and
co-locate a caveat marker on the column header itself.

### 3. `freeable` desynchronizes from `ta`/`td` across delete + refresh [SERIOUS]

`apply_removal` (`tree.rs:491`) tombstones a subtree and propagates
negative deltas into `ta`/`td`/`tn`/`te` **instantly**, and knows nothing
about the `FreeableAttribution` side maps. `per_dir` is recomputed only on
explicit `r`-refresh (§3, §5). So between an in-app delete and the next
refresh, a directory's `disk` column reflects the post-delete tree while
its `freeable` column reflects the pre-delete sweep — two columns, two tree
states, side by side.

If the user sorts by freeable (finding 2) and then deletes a subtree, the
sort is now ordered on stale numbers on a *frozen* tree — the one context
where every other column is rock-stable across refreshes. §9 pt4 already
concedes "a column that shifts under the user's eyes on a frozen tree
erodes trust in the *tree*, not just the column"; the delete path makes it
worse than pt4 admits, because it is not even a re-walk, it is a silent
divergence.

Amendment: after every `apply_removal`, either recompute `per_dir`
immediately (cheap) or subtract along the chain in lockstep; never allow
`freeable` to be the sort key while it disagrees with `td`.

### 4. A wrong-but-plausible number reaches a row while every §3 guard passes green [SERIOUS]

The proposal (§9 pt1) flags this as the thing to press hardest. Pressing:
the dangerous case is **ancestor-directory recycling**, and it slips past
*all* of step 3–4's guards — prefix match, dev match, tombstone, excluded —
because none of them detect that a same-named directory now means something
different.

Worst realistic case: Postgres. A backend holds a deleted relation file
`/var/lib/postgresql/data/base/16384/98765` open until its transaction
commits (30 GiB, a big table drop). Meanwhile `DROP DATABASE` + `CREATE
DATABASE` recycles OID **16384** to a different database, and the scan
(run after the recreate) records a *new* `16384` dir for that other DB.
readlink gives `.../base/16384/98765 (deleted)`; attribution drops the last
component, walks into the impostor `16384` (same bytes, same dev, live,
not tombstoned), and hangs 30 GiB of "freeable" on the **wrong database's
directory**. Every guard is satisfied; the number is wrong; and per finding
2 the sort key will float it to the top.

Directory-name recycling is not exotic — mail spools, `git/objects/tmp_*`,
`/var/lib/docker/overlay2/<hash>` GC, sharded browser caches all recycle
directory names routinely. The *filesystem total* stays correct (credit,
survived-list), so this is SERIOUS not FATAL — but it is a live counter-
example to "correct where other tools lie," which is the whole product
premise.

The rename-then-delete variant is milder: readlink tracks renames *live*
(research §1), so a file moved after the scan and then deleted attributes
to its **new, on-disk-correct** directory — truthful, but unfamiliar to a
user who remembers the old location. Confusing, not wrong.

### 5. Dump `fb`/`fbn` keys: severable, and should be severed [ANNOYING]

§7 proposes `fb`/`fbn` on the `e` line, informational, ignored by diff.
Three problems against `dump-v1`:

- **Violates §10 rule 5** ("capabilities declared in the header, never
  inferred from data"). With `--no-proc-sweep` (§7) the writer omits `fb`,
  and a reader cannot distinguish "swept, found 0" from "did not sweep."
  That capability must be a header flag (`sweep:true`), like `ext` /
  `ordered` / `allino`. The proposal adds no such flag.
- **The monitoring justification is self-defeating.** Monitoring wants
  *trend* — diff two dumps for freeable growth. But `fb` sits on the `e`
  line and is "ignored by diff," so `camembert diff` surfaces nothing. The
  user is left to `zstdcat | jq` two files by hand, at which point storing
  the key bought almost nothing.
- **Unverifiable process-state number in a filesystem-snapshot format.**
  Every other number in a dump is derivable from its entries; a whole-scan
  `fb` with no per-directory breakdown is a "trust me" annotation a future
  reader/differ can cross-check against nothing — precisely the kind of
  number the format otherwise refuses.

Recommend cutting `fb`/`fbn` from phase 1. If kept: add the header
capability flag and make diff actually surface the delta, else the stated
purpose is unmet.

### 6. "Scan-time publish path is untouched" is inaccurate [ANNOYING]

§4 claims only the post-scan `build_snapshot` call site grows a lookup.
But `build_snapshot` (`view.rs:177`) is the **single shared function** that
both the scan-time owner (`ViewPublisher::tick` → publish) and all
post-scan navigation (`ui.rs:863`, `ui.rs:2454`) call. Adding `freeable`
to `Row` and `DirTotals` forces one of: (a) a signature change threading
`Option<&FreeableAttribution>` through — which *does* touch the owner call
site, contradicting "untouched"; or (b) a `Row.freeable` that is dead-0 on
every snapshot the owner publishes at 33 ms during the scan. Neither is
fatal, but the claim as written is wrong; pick (a) and document that
`Row.freeable == 0` during the scan.

### 7. Conditional per-directory column shifts table geometry on navigation [ANNOYING]

Current fixed widths (`ui.rs:1236`): mark 1 + marker 1 + real 10
(+ apparent 10) + % 6 + bar 12 + items 9, before `name` `Min(10)`. A
`+1.8G` freeable column (~8–10 cells) pushes the fixed block to 47–57 (57–67
with apparent) before name. At the classic 80 cols with apparent on, name is
squeezed to ~13 cells. That alone is survivable (names already truncate).

The smell is B's "only when at least one visible row is nonzero" rule: the
column **appears and vanishes as you navigate** between directories, sliding
the name column and every bar left/right per view — on a UI that works hard
to keep bar/identity-color alignment stable across frames. The table has
**no existing width-responsive column-shedding** (only the user's
`show_apparent` toggle, `ui.rs:1241`); freeable would be the first column
whose presence is data-dependent per-directory. Make presence a stable rule
(session-level, not per-view), not a per-directory flicker.

Related: §6 wants a "bright tick *beyond* the bar end," but the bar is a
fixed `Constraint::Length(12)` column (`ui.rs:1253`) — there is no "beyond"
without widening that constraint or bleeding into `%`/`items`. The
rendering needs its own reserved cells, more layout than §6 implies.

### 8. Selection-card and server-incompleteness oversell freshness/coverage [COSMETIC]

- The card's "nginx (PID 1234) holds 1.8 GiB" (§6, backed by
  `entries_at`) is a specific, actionable claim that can be stale by the
  time it is read — the PID may be gone (research §6 confirms the race is
  benign to *collect*, but the card *asserts*). Inherent to any /proc tool;
  phrase it as "at last sweep," not as present fact.
- On a multi-user server the unprivileged sweep sees essentially none of
  the interesting holders (research §4: `www-data`/`postgres` fds denied).
  The column then shows `+2 GiB` when the truth is `+200 GiB`, disclosed
  only by a one-line footer against a column that *looks* authoritative.
  Shared with every option, not B-specific — but B's column makes the
  under-count more prominent and more trusted than a gauge suffix would.

## Recommendation

Do not kill, but stop calling it a standalone design. After the amendments
below, B **is** "Option A + an opt-in best-effort column," and should be
proposed and costed as exactly that.

Amendments (in priority order):

1. **Strike the phase-2 justification (§8).** Shared-extent and hardlink
   freeable are non-additive; the additive per-dir channel cannot represent
   them. Justify the column on phase-1 merits or not at all.
2. **Drop `SortKey::Freeable`** — or make the residual bucket a real,
   sortable table row and annotate the column header as best-effort.
3. **Close the delete/refresh consistency window**: keep `per_dir` in
   lockstep with `ta`/`td`, and never sort on a stale freeable column.
4. **Cut the dump `fb`/`fbn` keys** for phase 1 (or add the header `sweep`
   capability and make diff surface the delta).
5. **Fix the "untouched" claim**: `build_snapshot` gains an explicit
   `Option` param; `Row.freeable == 0` during the scan, documented.
6. **Stabilize column presence** (session-level rule, not per-directory
   nonzero) and reserve real cells for any beyond-bar tick.
7. **Co-locate the honesty caveat** with the column, not only on the
   distant gauge line.

The finding B itself nominated as the one to press hardest (wrong number on
a row, §9 pt1) is real and survives its own guards (finding 4) — but the
`dev`-scope decision caps it at "wrong row, correct total," which is what
keeps B out of the fatal column. The thing that actually guts B is quieter:
its reason to exist as more than a column (phase-2 reuse) does not hold.
