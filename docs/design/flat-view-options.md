# Flat view + pattern aggregation — options dossier

**Status: draft — awaiting the co-design session.** Addresses HANDOFF
"Suggested next steps" §1 and the original vision in
[handoff-original.md](handoff-original.md) §"Vues et requêtes": *top N
files of the whole tree, out of the hierarchy* and *`node_modules` =
14 GiB cumulative, `*.log` = 3 GiB — people think in categories, not
locations*. This feature is smaller than freeable phase 1, so the
options live inline as sections rather than per-option files.

## Scope boundary (drawn deliberately)

The original vision's third bullet — **the re-aggregating filter**
("filter `*.mp4`, recompute the whole tree on the subset; combined with
'older than 6 months' it becomes a query language") — is **wave 3, not
this feature**, and ships together with the Ctrl-K palette reserved in
[tui-design.md](tui-design.md). The line between the two:

- **This feature** computes *additional read-only reports* over the
  frozen tree: one global top-files list, one fixed set of named
  pattern groups with totals. The tree view is untouched; nothing
  re-aggregates; there is no user-typed expression at runtime (config
  patterns are declared, not queried).
- **Wave 3** takes a user-typed predicate and rebuilds *the tree view
  itself* on the matching subset (alternate aggregate generations over
  the shared frozen structure — the scan-tree option C §9 idea). It
  subsumes any ad-hoc pattern query; this feature must not grow
  expression syntax, combinators, mtime/size predicates, or a query
  CLI, or it becomes a worse wave 3 shipped early.

Anything in the session that smells like "and then combine two
patterns with…" belongs to wave 3 and gets parked.

## Grounding: what the machinery already gives us

Facts from the code that the axes below lean on (all verified against
current `main`):

- **Node layout**: `camembert-core/src/tree.rs` — `Node` is exactly
  32 bytes (const-asserted): packed name/kind/flags `u32`, parent
  `u32`, apparent `u64`, disk `u64`, mtime `i64`. 10 M entries =
  320 MB of nodes, streamed linearly at DRAM bandwidth (8–20 GB/s
  effective) ≈ **15–40 ms for one full pass, single-threaded**. That
  number sizes every "cheap" claim below.
- **Names are interned and deduplicated** (`tree/interner.rs`):
  matching a glob against every *unique name once* and memoizing the
  verdict in a bitset makes the per-node pattern test a bit lookup.
  Typical trees dedup names ~2× or better; the bitset is ≤ 1 bit per
  unique name (≪ 1 MB).
- **DirId order is topological**: `Tree::add_dir` requires the
  parent's `DirId`, so a parent's index is always smaller than its
  children's. A single forward pass over the dir table can propagate
  "which dir patterns are active above me" with no recursion and no
  hash lookups.
- **Subtree aggregates exist per directory** (`DirMeta.ta/td/tn/te`),
  maintained through deletions (`apply_removal` subtracts up the
  chain). A dir-pattern group total over *outermost* matches is a sum
  of O(matches) already-computed aggregates.
- **Hardlinks**: first-seen links are counted; later links carry
  `NodeFlags::HARDLINK_EXTRA` and contributed **0** to every
  aggregate; post-scan canonical re-attribution (dump D2: smallest
  raw-byte path owns the inode) has already run by the time the arena
  freezes. `Tree::is_hardlink` answers "is this row any link of an
  nlink > 1 inode" in O(1). A flat sum that skips `HARDLINK_EXTRA`
  rows equals the root aggregate — no double counting, by
  construction.
- **Deletions tombstone**: `Tree::children` filters tombstoned rows —
  but a raw arena iteration does not. Any flat fold must either walk
  via `children()`/run lists (recommended below) or check
  `is_removed` per node.
- **Post-scan, the UI thread owns the frozen arena** (`ui.rs`
  `Phase::Done`) and serves navigation itself via
  `view::build_snapshot`; `serve_local` bumps its local generation on
  *every navigation*, so a cached report must not key on snapshot
  generation (it would recompute on every keystroke) — it needs a
  deletion epoch.
- **Marks already work from any row** post-scan (`MarkedEntry`
  captures node + path at mark time); the basket, review modal and
  confirm flow are view-independent.
- **CLI**: `--top N` (env `TOP`, default 20) already exists and feeds
  the `--no-ui` summary's "top directories by real size" list
  (`ScanOutcome::top_dirs_by_disk`).

## Prior art (web check, 2026-07)

- **ncdu** (1 and 2): no flat view, no category view. Its
  much-requested gaps (speed, diff, filters) are the ones camembert
  already targets; flat/top-files is answered in its orbit with "use
  `find | sort`" or another tool.
- **dust**: the closest to a flat view in the du generation — `-F`
  ("show only files") lists the largest files without hierarchy, plus
  `-e`/`-v` regex include/exclude and a `-t` group-by-filetype. All
  non-interactive output, no TUI browsing of the result.
- **gdu**: `--top X` prints the X largest files, *non-interactive
  mode only*; type filters (`--type`) exist but no aggregation view.
  **dua**, **pdu**, **diskonaut**, **baobab**: neither flat view nor
  pattern aggregation. No *interactive* TUI in the field browses a
  global top-files list.
- **qdirstat** and **WinDirStat** are the category references: both
  aggregate by filename *extension* over the whole tree (qdirstat's
  File Type Statistics with configurable suffix categories and
  drill-down; WinDirStat's Extension List with bytes/%/count columns
  and treemap tinting). qdirstat's authors note the known limit:
  extensions carry weak semantics on Unix, so much lands in "Other" —
  a point in favor of *named, user-meaningful* groups over automatic
  extension buckets.
- **Named-directory categories** (`node_modules`, `target`, `.git`)
  are a first-class group in *no* surveyed analyzer — that niche is
  held by single-purpose cleaners (npkill, kondo, cargo-sweep) people
  run alongside their analyzer. **No tool documents nested-match
  dedup** (npkill lists nested `node_modules` individually —
  double-counting if summed); the research found no prior art for the
  outermost-match rule below.

So: flat top-files exists only as non-interactive output (dust `-F`,
gdu `--top`) — an *interactive, sortable, mark-and-delete* flat view
is open field; and cross-directory pattern aggregation with honest
nesting/hardlink semantics exists nowhere. Both are differentiation
squarely on-thesis.

## Axis 1 — UI surface

### Option 1A — two sibling view modes, in-place (recommended)

Two new view modes at the same level as the tree browse, toggled by
one key each:

- **`t` — flat view**: the table region shows the top-N files of the
  whole scan; **`b` — breakdown view**: the table region shows one row
  per pattern group. `t`/`b` from anywhere post-scan; pressing the
  active mode's key (or `Esc`) returns to the tree at the exact
  directory + cursor the user left (the nav stack is untouched by mode
  switches).
- **What stays**: header, breadcrumb (shows `⊤ top files` / `⊤ by
  pattern` as a synthetic segment), metric cards, disk gauge, basket
  strip, footer — they are scan-global and remain true. Zen mode (`z`)
  composes orthogonally, as today.
- **What the donut shows**: the mode's own data — top-N file slices
  (+ an "everything else" slice) in flat view; **pattern-group slices
  in breakdown view** — a camembert of categories is the single most
  on-brand rendering this feature has. Identity colors per row/slice
  as in the tree.
- **Flat row** = `size · proportion bar (of scan total) · path`, with
  the directory prefix dim and the basename in the row's identity
  color. A `⛓` badge marks rows where `is_hardlink` is true.
- **Breakdown row** = `total · bar (of scan total) · count · label`
  (e.g. `14.2 GiB ▓▓▓ 37 dirs node_modules`).
- **Navigation**: `Enter` on a flat file row leaves flat mode and
  lands in the tree at the containing directory, cursor on that file
  (stack rebuilt from the parent chain — `DirMeta.parent` walk, then
  `serve_local`). `Enter` on a breakdown group row expands it in place
  to its top contributors (outermost matched dirs, or top matching
  files for a file pattern); `Enter` on a contributor jumps to the
  tree likewise.
- **Marks**: `Space` works on flat rows and on group *contributors*
  (they are real nodes; `MarkedEntry` already captures everything).
  The basket is shared with the tree view. Marking a whole *group* in
  one keypress is deliberately **not** in phase 1 (see decision 8).
- **Sort**: in flat view `d/a/m/n` apply as today; `c`/`e` flash "not
  applicable in flat view". In breakdown: `d/a` = group total, `c` =
  match count, `n` = label.
- **Esc precedence**: modals still eat Esc first (confirm > review >
  freeable > cheatsheet); below that, Esc in a non-tree mode returns
  to the tree instead of quitting; `q`/Ctrl-C always quit. This is
  the one behavior change to today's "Esc quits" — flagged as
  decision 1.

Cost: a `ViewMode` enum in `UiState`, two render paths for the table
region and donut input, one jump helper. No arena, snapshot, dump or
diff changes.

### Option 1B — virtual directories in the tree

Synthetic entries (`⊤ Top files`, `⊤ node_modules`, …) injected at the
root of the tree view, entered with the normal descend keys.

Rejected: this is the ghost-rows lesson from
[freeable-options.md](freeable-options.md) option C in miniature —
rows that are not filesystem entries flow into every consumer (dump
writer, diff, deletion marking, snapshot building all must filter
them, forever). The tree must remain the filesystem. Not carried to
the session.

### Option 1C — floating panel (like `f`)

A modal panel listing top files, precedence-stacked with the existing
modals.

Rejected as the *primary* surface: flat view is a browsing mode, not
evidence detail — it wants the full table (sort keys, marks, donut,
scroll), and the modal ladder is designed for transient overlays. The
freeable panel earns modality because its rows are non-tree evidence;
flat rows are real nodes the user acts on. Not carried to the session.

### Browse-during-scan position

**Post-scan only** (recommended, firmly): during the scan the arena
has another writer (D1) — the fold would have to run on the owner
thread between batch integrations, stalling integration for tens of
ms per publish, violating the "the scan never waits" identity; and
first-seen hardlink attribution makes a mid-scan top-files list
precisely the view where provisional double counting is most visible
(D3's footnote exists because of this). `t`/`b` during the scan flash
"available when the scan completes" — the exact idiom marks already
use (`MarkRefusal::ScanRunning`). A cancelled scan freezes the arena
like a completed one, so flat view works on partial trees (honest:
the cards already say the scan was cancelled).

The alternative — live flat view on a "provisional" note — buys a
during-scan toy at the cost of the scan's latency identity and an
extra owner-thread code path. Rejected; not carried unless the
session disagrees on the identity call.

## Axis 2 — Flat top-N semantics

- **Files only, `Kind::File` only.** A flat directory list is the
  tree sorted differently (and `--no-ui` already prints top dirs);
  symlinks (`st_size` = target-path length), devices and fifos are
  noise rows that would only ever surface as curiosities. Not
  carried as an option — stated as the rule.
- **N bounded at a compile-time cap of 1 000** (scrollable in the
  view). The fold keeps a min-heap of N entries (16 B key + node id):
  ~20 KB, O(pass + k·log N) with k = heap displacements — in
  practice almost every node fails the heap-min compare, so the heap
  is free. No full sort of 10 M entries, no configurable knob until
  someone asks (a knob would need config surface + docs for a limit
  nobody has hit).
- **Hardlinks: canonical owner only.** `HARDLINK_EXTRA` rows are
  skipped (listing the same inode twice under two paths is a lie in a
  *deduplicating* top list; the tree view remains the place where
  each path shows). The listed canonical row gets the `⛓` badge; its
  other paths are one `Enter` away in the tree. This matches dump D2
  and the aggregates exactly.
- **Tombstoned rows skipped** (deleted entries must leave the list —
  walking via run lists + the `children()` tombstone filter handles
  it).
- **Error entries**: stat-failed files carry size 0 and simply never
  rank; unreadable directories are already accounted as error counts
  in the cards/tree. The flat view adds no new honesty surface —
  and needs none, because it never claims completeness beyond what
  the errors card already bounds.
- **Excluded mounts / kernfs**: not scanned, contribute nothing,
  cannot appear. (They have no `DirMeta`; the fold never descends
  them.)

## Axis 3 — Pattern aggregation semantics

The subtle axis. Definitions first, then the two real forks.

### What a pattern is

A pattern is a **glob over the basename** (raw bytes, case-sensitive,
non-UTF-8 safe — same byte discipline as everything else), tagged
with a kind:

- **dir pattern** — matches directory names (`node_modules`, `.git`,
  `target`, `__pycache__`, `.cache`, `.venv`); written with a
  trailing `/` in config (`"node_modules/"`), the gitignore/rsync
  convention.
- **file pattern** — matches non-directory names (`*.log`, `*.tmp`,
  `core.*`).

**Full-path globs are wave 3** (they need the query language's
machinery and its UI to be usable; a basename glob is resolvable
against the interner once, a path glob is a per-node parent-chain
walk). This is the second place the scope boundary bites, stated so
it survives the session.

Matcher: basenames contain no `/`, so the needed glob subset is
`*`/`?` over bytes — a ~40-line two-pointer matcher, no new
dependency. If the session wants character classes (`*.[ch]`),
`globset` (byte-oriented, ripgrep's) is the fallback; recommendation
is to start without it.

### Built-ins and config

Both built-in presets and user-defined patterns, merged:

- **Preset set** (proposal — session trims/extends, decision 5):
  `node_modules/`, `.git/`, `target/`, `__pycache__/`, `.cache/`,
  `.venv/`, `*.log`, `*.tmp`. Short deliberately: every preset is a
  maintenance promise, and the config file is one line away.
- **`camembert.toml`**:

  ```toml
  [patterns]
  # label = basename glob; trailing '/' marks a directory pattern
  "conda envs" = ".conda/"
  "media" = "*.mp4"
  ```

  User entries append to the presets; reusing a preset's label
  shadows it; `presets = false` under `[patterns]`… is a session
  call (decision 5) — recommendation: allow shadowing, no global
  disable until asked (smallest surface).
- Documented in `--help` (key list + config syntax) and README in the
  same change, per CLAUDE.md.

### Nested-match dedup (the rule, precisely)

For a dir pattern `P`, the counted match set is the **outermost
matches**:

> M(P) = { dir d | name(d) matches P, and no ancestor of d matches P }

and `total(P) = Σ_{d ∈ M(P)} subtree(d)`. A `node_modules` nested
inside another `node_modules` is *inside the outer one's subtree
aggregate already* — counting only outermost matches counts every
byte **exactly once per pattern**, with zero extra work.

The fold implements this with a per-directory **active-pattern
bitmask** (u64, ≤ 64 patterns — enforced, error above): one forward
pass over the dir table in `DirId` order (topological, see
Grounding), `mask[d] = mask[parent(d)] | own_match(name(d))`. A dir
is an outermost match of `P` exactly when `own_match` has `P` but
`mask[parent]` does not. 1 M dirs × 8 B = 8 MB transient. File
patterns have no nesting concept: every matching non-dir,
non-`HARDLINK_EXTRA`, non-tombstoned node counts.

### Overlap between groups (the honest choice)

A `*.log` inside `node_modules` is in both groups. Two policies:

- **Overlapping groups, stated** (recommended): each group's total is
  independently true ("deleting all node_modules frees ~14.2 GiB";
  "log files hold 3.1 GiB"). The breakdown view carries one honesty
  line — and because the fold visits every file anyway (flat top-N
  shares the pass), it can *measure* the overlap for free: bytes
  whose group-membership count ≥ 2. The line reads e.g. "groups
  overlap: 1.3 GiB counted in more than one group — the column does
  not sum to the scan total". Shown only when overlap > 0.
- **Disjoint partition** (first-match-wins precedence): sums to ≤
  total, but every number becomes a function of an arbitrary
  precedence order — `node_modules` silently loses its logs, or
  `*.log` silently loses everything under `node_modules`. That is a
  wrong-but-plausible number wearing a right-looking property;
  the thesis (and freeable's decision history) says no.

Same-pattern nesting is *not* overlap (dedup rule above); overlap is
strictly cross-group.

### Hardlinks in groups

Contribution semantics everywhere: `HARDLINK_EXTRA` rows contribute 0
to every group (a `*.log` that is an extra link adds nothing; a
canonical link under `node_modules` attributes its bytes there even
if a second link lives outside). This is exactly the tree's own
convention — one footnote in the breakdown view's help text, no new
mechanism. The alternative (per-group inclusion-exclusion over link
sets) is freeable phase 2's non-additive channel — explicitly not
built here.

## Axis 4 — Engine

### Option 4A — one fused single-threaded fold (recommended)

One pass computes everything: dir-mask pass over the dir table
(topological, 1 M rows), then a dir-centric walk — for each live dir,
iterate its child runs (`children()` filters tombstones; runs make
this near-sequential over the node arena) — accumulating per node:

- flat top-N min-heap (files, non-extra);
- per-group totals/counts (`u64` adds indexed by the mask's set
  bits — dirs add their own inode size to the groups in their own
  mask, files add to `mask[parent]`-groups plus matched file
  patterns);
- per-group top-contributor mini-heaps (for the breakdown drill-in);
- overlap bytes (membership count ≥ 2).

Cost, grounded: the pass streams the 320 MB node arena plus the
~80 MB dir table ≈ **20–50 ms at DRAM bandwidth on 10 M entries**,
single-threaded; name-glob verdicts are memoized per unique interned
name (bitset per pattern, built in one interner sweep, ≪ 1 ms for
typical trees). An invariant test pins the fold to the existing
aggregates: for every pattern with no cross-group interference,
`Σ outermost DirMeta aggregates == folded group total` — the
aggregate identity is the cross-check, not the implementation,
because the fused pass also needs per-file visits for file patterns,
top-N and overlap anyway (using aggregates for dir groups would save
nothing and split the code into two half-folds).

### Option 4B — rayon parallel fold

`rayon` par-chunks over the arena. Rejected for phase 1: a new
workspace dependency to turn ~30 ms into ~8 ms, once per scan and
once per deletion batch — below perception at human interaction
frequency. The mask pass is sequential anyway (parent dependency).
The door stays open: the frozen arena is `Sync`, the fold is a pure
function of `&Tree`, and wave 3's per-keystroke re-aggregation (scan
tree option C §9) is where parallel folds earn their dependency —
that is the right moment to add it, with this fold as its first
customer.

### Option 4C — reuse the scan worker pool

Not actually available: the workers and owner exit at scan end
(`scan_live` returns the outcome); keeping them alive for view
features couples the scan module's lifecycle to the UI (the same
isolation argument as freeable D8). Rejected, not carried.

### Where results live, and invalidation

New module `camembert-core/src/flat.rs` (freeable D8 precedent:
nothing in tree/dump/diff): pure `fn compute(&Tree, &[Pattern],
top_n) -> FlatReport` (`top_files` + `groups` + `overlap_bytes`),
unit-testable on synthetic trees. The UI caches
`Option<(mutation_epoch, FlatReport)>` where `mutation_epoch` is a
new counter bumped only by successful deletion batches — **not** the
snapshot generation, which `serve_local` bumps on every navigation
(grounded above; keying on it would recompute per keystroke).

Compute lazily on first `t`/`b` after scan end or after a deletion,
**synchronously on the UI thread**: 20–50 ms is 1–2 dropped frames,
once per scan/deletion — simpler than the freeable sweep's channel
(which exists because `/proc` sweeps do syscalls, not because of
CPU), and off-thread sharing of `&Tree` would fight the single-owner
borrow structure of `Phase::Done`. Escape hatch if a bench on a 10 M
tree says otherwise: the sweep's "computing…" placeholder pattern.
Recompute after each deletion batch is the same 20–50 ms — fine at
human deletion frequency (and only paid if the user re-enters the
mode).

## Axis 5 — CLI surface

- **Phase 1 ships interactive `t`/`b`, plus one summary addition**:
  the existing `--no-ui` summary gains a "Top N files by real size"
  section, reusing the existing `--top` flag and the same fold —
  near-zero marginal surface, and it answers the most common
  non-interactive question (the original vision's `--top 20` bullet).
- **No pattern/JSON CLI in phase 1.** A bespoke `--patterns` report
  flag would be deprecated the day wave 3's query language ships its
  non-interactive form (which subsumes it: a pattern group is a
  canned query). Shipping surface we plan to obsolete violates the
  "smallest honest slice" rule that served freeable well. The
  breakdown stays interactive-only for now, marked in README as
  such.
- Everything new documented in `--help` and README in the same
  change (CLAUDE.md rule; keys `t`, `b`, config `[patterns]`, the
  summary section).

## Recommendation per axis

1. **UI surface**: option 1A — `t` (flat files) / `b` (pattern
   breakdown) as in-place sibling view modes; cards/gauge/basket
   stay; donut shows mode data (category camembert in breakdown);
   `Enter` jumps into the tree; marks work on real rows; post-scan
   only, with the marks-style flash during the scan.
2. **Flat semantics**: regular files only, canonical hardlink owner
   only (`⛓` badge), top-1 000 cap via min-heap, tombstones and
   excluded/error entries handled by construction.
3. **Pattern semantics**: basename globs only (dir patterns with
   trailing `/`, file patterns without); presets + `[patterns]` in
   camembert.toml with label shadowing; outermost-match dedup via the
   topological mask pass; **overlapping groups with a measured
   honesty line**, no disjoint partition; hardlink extras contribute
   0 everywhere.
4. **Engine**: one fused single-threaded fold in a new
   `camembert-core/src/flat.rs` (20–50 ms @ 10 M entries, grounded on
   the 32-byte node layout), cached per deletion epoch, computed
   synchronously and lazily; no rayon until wave 3 gives it a real
   customer.
5. **CLI**: interactive-first; the `--no-ui` summary gains top-N
   files reusing `--top`; no pattern flags or JSON until the wave 3
   query language subsumes them.

## Decisions needed in the co-design session

1. **Mode keys and Esc behavior.** `t`/`b` as proposed, and does
   `Esc` in a non-tree mode return to the tree (recommended) instead
   of quitting — the one change to today's "Esc quits"? (`q`/Ctrl-C
   always quit.)
2. **Donut in the new modes**: mode data — top-file slices / category
   camembert (recommended) — or hide the wheel like zen?
3. **Post-scan only** (recommended, identity call: the scan never
   waits) vs a provisional live flat view during the scan?
4. **Overlap policy**: overlapping groups + measured "counted twice"
   honesty line (recommended) vs disjoint first-match-wins partition?
5. **Preset list and config**: confirm/trim the 8 proposed built-ins;
   trailing-`/` syntax; label shadowing yes, global preset disable
   deferred (recommended)?
6. **Group-level marking**: `Space` on a breakdown *group* row to
   mark all its contributors ("mark every node_modules") — the killer
   cleanup gesture, but also the largest mass-deletion foot-gun in
   the app. Recommended: defer to a fast-follow with its own guard
   design; phase 1 marks individual contributors only.
7. **CLI slice**: top-N files in the `--no-ui` summary now
   (recommended), everything else waits for wave 3?
8. **Flat cap**: fixed top-1 000 (recommended) or configurable?
