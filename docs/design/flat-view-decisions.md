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
