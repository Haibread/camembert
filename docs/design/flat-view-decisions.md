# Flat view + pattern aggregation — decisions (co-design session, 2026-07-23)

Outcome of the co-design session over the
[options dossier](flat-view-options.md) and its
[attack report](flat-view-attack.md). Settled; reopening one requires a
new element. Covers HANDOFF next-step "Flat view + pattern
aggregation". The wave-3 filter/query language and Ctrl-K palette are
explicitly out of scope (the dossier draws the boundary).

## D1 — Groups are disjoint: first match wins, outermost wins

Pattern groups form a **partition**: every byte is counted in at most
one group, plus an implicit "rest". Precedence:

1. **Directory coverage is outermost**: a directory matching a
   dir-pattern claims its whole subtree; nested matches (a
   `node_modules` inside a `node_modules`, a `.git` inside a claimed
   tree) do not re-claim. A `*.log` file inside a claimed directory
   counts in the directory's group, not in `*.log`.
2. **Among patterns matching the same name, list order wins**
   (presets first, then `camembert.toml` `[patterns]` in file order; a
   user pattern with a preset's label replaces the preset in place).

Chosen over overlapping groups (attack finding: overlap sums > 100 %
and cannot be rendered honestly in the donut — the category camembert
and the list must tell the same truth). The panel states the rule
("patterns apply in order; a claimed subtree is not re-counted").

## D2 — Live provisional during the scan, exact fold at the end

User decision (against the dossier's post-scan-only recommendation,
with the trade-offs on the table): `t`/`b` work **during the scan**,
badged provisional — browse-during-scan is the product's identity and
these views join it. Engine consequences, binding:

- **No whole-tree fold runs during the scan.** Live numbers come from
  **incremental accumulation on the owner thread**: O(1) per inserted
  node — glob matching memoized per interned-name id (bitset, computed
  on first occurrence of a name), directory coverage carried as
  owner-side per-DirId state inherited parent→child (topological
  insertion order guarantees the parent's coverage is known first),
  group byte counters bumped at insert, flat top-N maintained in a
  bounded min-heap keyed on `st_blocks` (deterministic tiebreak:
  size, then NodeId). Hardlink attribution is first-seen during the
  scan — same provisional caveat the TUI already shows; extras
  contribute 0.
- The owner publishes the accumulated summary alongside the existing
  view snapshots at the existing cadence (arc-swap; no new locks; UI
  stays wait-free). The provisional badge mirrors the hardlink
  provisional note's style.
- **At scan end** (after canonical hardlink re-attribution) the exact
  **frozen-arena fold** (`camembert-core/src/flat.rs`, single
  streamed pass, dossier engine) replaces the provisional summary —
  and is the only source post-scan: it recomputes after every
  deletion (render-time epoch check; attack serious finding — the
  flat/breakdown views must never show a deleted file as occupying
  space, including deletions performed from within the mode).
- The incremental and fold paths must agree on the frozen tree: an
  integration test scans a fixture and asserts accumulated == folded.

## D3 — UI: `t`/`b` in-place modes, contextual Esc, mode-fed donut

- `t` = flat top files (path shown per row), `b` = pattern breakdown;
  in-place table modes — cards, gauge, basket, footer stay. `t`/`b`
  toggle back to tree; **Esc becomes contextual**: closes a modal
  first, then leaves a mode, and only quits from tree view (`q`
  always quits). Keys documented in keymap/cheatsheet/--help/README.
- Donut shows mode data: breakdown = the category camembert (disjoint
  per D1, rest wedge included); flat = top files with sub-threshold
  entries merged into one "others" slice (amendment: the wheel gains
  an aggregated-others slice so the mode donut stays informative).
- `Enter` on a flat row jumps to the containing directory in tree
  view; marks work on real rows in both modes (basket shared).
- Flat list: regular files only, canonical hardlink owner only
  (`⛓` badge on multi-link rows), truncation line when capped.

## D4 — Patterns: basename globs, ~8 presets, `[patterns]` in toml

- A pattern is a basename glob (`*`/`?` only; `{}`/`[]` are literal —
  documented); trailing `/` marks a dir-pattern (`node_modules/`).
- Presets (initial set, tuneable): `node_modules/`, `.git/`,
  `target/`, `__pycache__/`, `.cache/`, `.venv/`, `*.log`, `*.tmp`.
- `camembert.toml` gains `[patterns]` (label → glob, file order
  significant) and `flat_cap` (top-N cap, default 1000, user
  decision: configurable). **Config parsing becomes per-section
  resilient** (attack serious finding: today one bad key resets the
  whole config — a broken `[patterns]` must not eat the theme);
  invalid glob = warning + skip, never fatal.

## D5 — CLI: top files in the `--no-ui` summary, nothing more

The non-interactive summary reuses the existing `--top` to also print
top files (respecting the `-o -` stdout gate — summary lines never
corrupt a dump stream). No `--patterns` flag, no JSON: wave 3's query
language subsumes those.

## D6 — Module boundary

Exact fold + shared types in new `camembert-core/src/flat.rs`;
incremental accumulation lives with the owner (scan side), publishing
a plain summary value. No arena layout change, no dump change, no
diff change. Group-level marking ("mark every node_modules") is a
deliberate fast-follow with its own guard design, not phase 1.

## Condensed reasoning trail

Condensed from the now-deleted `flat-view-options.md` (options
dossier) and `flat-view-attack.md` (adversarial review). Verdict on
the attack: SURVIVABLE WITH AMENDMENTS — the engine half (topological
`DirId` dedup, hardlink re-attribution timing) held as written; the
surface half (donut composition, cache lifecycle, config collision)
needed the fixes below, all landed in code.

### Options

**Axis 1 — UI surface.** 1A (two sibling view modes, in-place,
recommended) **won**: `t`/`b` toggle in-place table modes reusing
cards/gauge/basket/donut, cost one `ViewMode` enum plus two render
paths, no arena/dump/diff change — this is D3. 1B (virtual directories
injected into the tree) **lost**: the ghost-rows lesson from
freeable's option C — synthetic non-filesystem rows would need
filtering in the dump writer, diff, marking and snapshot building,
forever. 1C (floating panel like `f`) **lost**: flat view is a full
browsing mode (sort, marks, donut, scroll), not transient evidence
detail the modal ladder is built for. Separately, "post-scan only"
(recommended firmly, on the grounds that a mid-scan fold would stall
owner-thread integration and first-seen hardlink attribution is most
visibly wrong mid-scan) **lost** to the user's identity call: D2 picks
live provisional accumulation during the scan instead.

**Axis 2 — flat top-N semantics.** Mostly stated as rules (files only,
canonical hardlink owner only, tombstones skipped), not forked
options. The one real fork — a fixed compile-time cap of 1000
(recommended, "no knob until someone asks") vs. a configurable cap —
went against the recommendation: D4 makes `flat_cap` a
`camembert.toml` key.

**Axis 3 — pattern aggregation semantics.** Overlapping groups, each
independently true with a measured "counted twice" honesty line, was
the **recommended** policy. It **lost**: D1 instead adopts the
disjoint partition (first-match-wins, outermost wins) the dossier
explicitly argued against ("a wrong-but-plausible number wearing a
right-looking property"). The session's reasoning: overlap sums above
100% cannot render honestly in the donut, and disjointness also
collapses the per-pattern-bitset memory cost the dossier worried about
into one `Option<GroupId>` per name. The nested-match dedup rule
(outermost match via a topological `DirId` mask pass) and the
hardlink-in-groups convention (extras contribute 0) were not contested
and carried unchanged.

**Axis 4 — engine.** 4A (one fused single-threaded fold, recommended)
**won**: one pass — dir-mask then dir-centric walk — computing top-N,
group totals and overlap together, ~20–50 ms at 10 M entries; this is
D6's `camembert-core/src/flat.rs`. 4B (rayon parallel fold) **lost**,
deferred: the win (~30 ms → ~8 ms) is below perceptible latency at
scan/deletion frequency; the door stays open for wave 3's per-keystroke
re-aggregation. 4C (reuse the scan worker pool) **lost** outright: the
workers and owner exit at scan end, so this isn't actually available
without coupling the scan module's lifecycle to UI features.

**Axis 5 — CLI surface.** Interactive-first (recommended) **won**: the
`--no-ui` summary gains a "Top N files" section reusing the existing
`--top`, no dedicated `--patterns`/JSON flag — a bespoke flag would be
obsolete the day wave 3's query language ships its non-interactive
form. This is D5.

### Attack findings

1. **Cache invalidation missed "delete while already in the mode."**
   The "compute lazily on first `t`/`b`" trigger model doesn't cover
   marking and deleting from within flat/breakdown mode without ever
   leaving it, which left a stale frame showing a just-deleted file as
   still occupying space. Fixed with render-time epoch reconciliation:
   `UiState::bump_flat_epoch` advances on every successful deletion
   regardless of mode, and `ensure_flat_summary_fresh` checks the
   epoch every frame before drawing (`camembert/src/ui.rs`,
   `camembert/src/ui/state.rs`), with a test covering delete-from-
   within-flat-mode.
2. **The donut didn't compose with either mode or the overlap
   decision.** Flat mode's per-file slices all sit under the 2%
   merge threshold (a near-uniform gray disc); breakdown's originally-
   proposed overlapping totals would sum above 1 and distort every
   wedge. Resolved structurally: flat mode keeps the wheel's existing
   sub-threshold merge as the "aggregated-others slice" amendment
   (D3); breakdown mode is resolved by D1's disjoint partition itself
   — the uncategorized row is excluded from ranked slices and reaches
   the donut only as the wheel's automatic remainder, which now equals
   `summary.rest` exactly with no overlap artifact
   (`camembert/src/ui.rs` `draw_flat_table`/`draw_breakdown_table`).
3. **`[patterns]` collided with `config::parse`'s all-or-nothing
   fallback.** A malformed glob entry (or a stray `presets = false`)
   would have reset unrelated `theme`/`color`/`no_motion` keys, and
   `RawConfig` needed a `patterns` field or every launch would warn
   about an unrecognized key. Fixed: parsing is now per-section
   resilient — the file is parsed into a generic `toml::Table` first,
   then each top-level key is deserialized independently, isolating a
   bad `[patterns]` entry from the rest (`camembert/src/config.rs`,
   test `a_bad_flat_cap_does_not_reset_theme_or_patterns`).
4. **The glob memo's memory bound was oversold.** The dossier's
   "≪ 1 MB" claim dropped the per-pattern factor (≈80 MB at the
   64-pattern cap over 10 M unique names) and the memo saves nothing
   on all-unique-name trees (git object stores, Maildir). Superseded
   by D1's disjoint design, which needs only one `Option<GroupId>` per
   name rather than a bitset per pattern; `flat.rs` now states the
   honest bound (2×2×unique_names bytes, ~40 MB at 10 M names) and
   documents the all-unique-name tradeoff as deliberate.
5. **The top-1000 cap had no truncation indicator or deterministic
   tiebreak.** Silent capping is off-brand for a tool that otherwise
   counts every unreadable byte, and ties at the cutoff could reshuffle
   list membership between recomputes. Fixed: `FlatSummary.truncated`
   surfaces as a footer note, and the ranked-file keep-priority logic
   has a documented deterministic tiebreak (`camembert-core/src/flat.rs`,
   `camembert/src/ui.rs`).
6. **The `*`/`?` matcher silently treats `{}`/`[]` as literal instead
   of rejecting them**, so a plausible pattern like `*.{log,tmp}`
   matches nothing and never warns. Resolved as documented, deliberate
   behavior: stated in `flat.rs` and README ("only `*`/`?` are special;
   braces/classes are literal"), backed by the
   `glob_braces_and_classes_are_literal` test.
7. **The CLI's top-files summary needed an explicit `-o -` stdout
   gate.** Addressed: the section lives inside the same
   `!dump_to_stdout` branch as the existing top-dirs list, and the
   code says so inline (`camembert/src/main.rs`).
8. **Reusing `--top` for both top-dirs and top-files summaries (while
   the interactive view uses its own cap) makes one flag mean two
   things.** Documented explicitly in `--help` and README ("one flag,
   two lists; the interactive `t` mode's own cap is the separate
   `flat_cap` config key").
9. **"Excluded mounts contribute nothing" overstated it** — a mount's
   own inode size is folded into its parent's aggregate, and a mount
   that itself matches a dir pattern has no `DirMeta` to sum. Fixed:
   the fold special-cases excluded mounts explicitly, treating an
   uncovered one as its own outermost dir match
   (`camembert-core/src/flat.rs`).
10. **Breakdown mode's inapplicable sort keys were under-enumerated**
    — only `c`/`e` were flagged as flashing, leaving `m` (mtime, no
    meaning for a group) unhandled. Fixed generically: `try_sort`
    flashes "not applicable" for any sort key without a column in the
    active mode, verified by
    `sort_key_not_applicable_in_a_mode_flashes_instead_of_applying`.

The originals (`flat-view-options.md`, `flat-view-attack.md`) are
recoverable from git history.
