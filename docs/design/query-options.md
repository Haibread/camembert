# Filter query language + Ctrl-K palette — options dossier

**Status: draft — awaiting the co-design session.** Addresses HANDOFF
"Suggested next steps" §1 — the wave-3 flagship: the re-aggregating
filter from [handoff-original.md](handoff-original.md) §"Vues et
requêtes" and the Ctrl-K command palette reserved in
[tui-design.md](tui-design.md), which **ship together** (the palette
is the language's UI). Facts and prior art in
[query-research.md](query-research.md) (cited as §n); the three
candidate designs in
[query-option-a-live-qualifiers.md](query-option-a-live-qualifiers.md),
[query-option-b-expression-apply.md](query-option-b-expression-apply.md),
[query-option-c-split-surfaces.md](query-option-c-split-surfaces.md).

## Problem statement

Give the user a typed way to ask "*.mp4, older than six months, over
100 MB" and get back **the whole tree re-aggregated over exactly that
subset** — browsable, sorted, donut and cards included — plus the
command palette the TUI design reserved on Ctrl-K. This is the
feature the flat view deliberately deferred to (path globs,
expressions, drill-down, any user-typed predicate), the reserved
customer for parallel folds over the frozen arena (scan-tree D1 /
option C §9), and — per the field survey — a capability **no existing
disk analyzer has** (research §4.6, §4.8). It must stay honest:
filtered numbers labeled as filtered, real totals never hidden,
deletion never quietly acting on more than the screen shows, dumps
never silently partial.

## Design dimensions

1. **Language syntax** — qualifier tokens (Everything/GitHub family)
   vs full expression grammar (and/or/not, parens) vs a split-hosted
   token language. Learnability on-ramp, typing cost of the common
   query, mid-typing error UX, completion fit, power ceiling and the
   upgrade path between them (research §4.1–§4.10).
2. **Predicate set + data** — what phase 1 can honestly test. On the
   node today: basename, kind, both sizes, mtime, flags; **not
   stored**: uid/gid (side-array + scan/message/dump changes,
   +20 MB @ 10 M), atime (+80 MB and a trust problem) (research
   §2.1, §2.4). Age = mtime with the stated honesty caveat
   (research §3).
3. **Filter semantics** — candidates and re-aggregation rules; how a
   filter composes with flat/breakdown modes, the donut, the
   freeable ledger, and above all **marking/deletion** (the
   "42 MB of logs shown, 300 GB deleted" trap, research §5.3).
4. **Engine** — when the fold runs (per keystroke debounced vs on
   Enter), whether rayon finally lands, sync-on-UI-thread vs async
   placeholder, and the cache/invalidation shape. Grounded budget:
   ~5–15 ms @ 200 k, 0.3–0.7 s @ 10 M single-threaded, ≈ 50–120 ms
   @ 10 M with rayon (research §2.3).
5. **Palette UX** — one Ctrl-K input with sigil modes vs split
   `/`-filter + Ctrl-K-commands; history, saved queries, empty
   state, Esc ladder placement.
6. **CLI** — `--filter` now or later; the dumps-are-never-filtered
   rule; diff interaction.
7. **Phasing** — the smallest honest slice vs the full language.

## The options, side by side

All three share the **semantic core** (candidates = non-dir entries;
`dir/`-terms as ancestor constraints; hardlink extras contribute 0;
filtered totals exclude dir inodes, stated; post-scan only in
phase 1; dumps never filtered) and the **engine substrate**
(`camembert-core/src/query.rs`: parser + fold → alternate
dir-aggregate table ≈ 32 MB @ 1 M dirs + match bitvec, epoch-keyed
like `FlatSummary`). They differ on the axes that are genuinely
contested:

### Option A — live qualifier tokens, one palette

`*.log >100M older:6mo` typed into a single Ctrl-K overlay
(query-first; `>` sigil for fuzzy commands; `/` opens the same input
in query mode). Re-aggregates live on a 100 ms debounce with a rayon
fold; never an error mid-typing (inert tokens + parse echo);
reserved sigils (`|`, grouping, `;`, path globs) rejected with
hints. Filter pill in the header; Esc clears the filter as a new
ladder level. `--filter` reuses the parser for `--no-ui`.

### Option B — expression grammar, apply on Enter

`(*.log or *.tmp) and mtime < -6mo and not node_modules/` — full
grammar (and/or/not, parens, typed comparisons, path globs) shipped
complete in phase 1, applied on Enter with span-scoped teaching
errors. No live narrowing, no debounce, no rayon (single-threaded
fold behind a placeholder at human frequency). The strongest CLI
story (same grammar day 1, diff-ready), the steepest on-ramp, and a
frozen maximal syntax surface from day one.

### Option C — split surfaces: `/` filter + Ctrl-K commands

Option A's language and engine, different home: `/` opens an inline
one-line filter prompt (tree stays fully visible, live
re-aggregation, Esc restores pre-`/` state), while Ctrl-K is a pure
command palette (fuzzy keymap actions + saved queries as commands,
no query parsing inside it). Two small surfaces instead of one
compound one; re-reads the "palette is the language's UI"
reservation as "ships alongside".

### Comparison

| axis | A — live qualifiers | B — expression grammar | C — split surfaces |
|---|---|---|---|
| on-ramp | bare terms work untaught; qualifiers layered (the Everything on-ramp) | grammar must be learned before numeric predicates work | same as A |
| common query typing cost | `*.log >100M older:6mo` | `*.log and size > 100M and mtime < -6mo` (~2×) | same as A |
| live narrowing | yes (debounced fold) | **no** — Enter only | yes, with the tree never covered |
| mid-typing errors | never (inert + echo) | n/a until Enter; then span-scoped, teaching-quality | never (inert + echo) |
| completion fit | qualifier names complete cleanly | field/keyword completion possible but mid-grammar is hard | same as A, minus room for a list in a one-line prompt |
| power ceiling | AND + `!` now; OR/parens/paths reserved for phase 2 | everything, day 1 (also frozen day 1) | same as A |
| engine | rayon lands; sync debounced fold (50–120 ms @ 10 M worst) | single-threaded on Enter (0.3–0.7 s @ 10 M, placebo-free placeholder) | same as A |
| CLI | `--filter` same parser, phase 1 | strongest: full grammar scriptable day 1 | same as A |
| palette coherence | one surface, sigil modes (broot's discoverability risk) | palette hosts a query *editor* sub-mode (feature-accretion risk) | two idiomatic surfaces, two histories, delegation seam |
| Ctrl-K reservation | literal fit (palette *is* the language's UI) | literal fit | re-interpretation — needs an explicit call |
| ships in slices | palette+language are one widget | editor+grammar are one lump | `/` and Ctrl-K can land separately |
| impl weight | parser S + fold M + palette L | parser M + fold S + editor L | parser S + fold M + prompt M + palette S |
| identity fit | *instant + honest*, extended to asking | precise but deliberate — the one identity clash | instant, and the cockpit stays visible |

## Recommendation

**Option A, grafting C's two best organs** — the same move as
scan-tree D1 (Option A + Option B's graft):

- **Language**: A's qualifier tokens with the reserved-sigil list
  settled in-session (research says this family is the only one with
  evidence of both power and per-keystroke fitness — §4.10). B's
  grammar is explicitly *not* built, but A's parser produces an AST
  whose phase 2 adds `|`/grouping without breaking any phase-1
  query — the upgrade path is designed, not hoped for.
- **From C**: `/` as a direct shortcut into query mode (both
  muscle memories served), and the command inventory generated from
  the `keymap.rs` dispatch table (palette, cheatsheet and keys share
  one source of truth).
- **Engine**: the debounced rayon fold — this is the reserved
  moment where the dependency earns its place (flat-view 4B), with
  the sync-with-escape-hatch policy and the (query, epoch) cache.
- **Semantics**: the shared core, with the dir-marking refusal
  closing the honesty trap until the group-marking fast-follow
  brings its guard design.
- **CLI**: `--filter` phase 1 (`--no-ui` + initial TUI filter),
  dumps never filtered, diff deferred.

Where C is *not* followed: the palette overlay remains the single
home (the reservation's literal reading; an inline prompt can be
revisited if the overlay demonstrably hides too much cockpit — new
element required).

## Decisions needed in the co-design session

1. **Syntax family**: qualifier tokens (recommended, = option A) vs
   expression grammar (B) vs tokens-in-split-surfaces (C)?
2. **Reserved-sigil list** (binding on phase 2): `|` OR, grouping
   (`(...)` vs Everything's `<...>`), `;` value lists, `/` path
   globs, `depth:`/`entries:`, `user:`/`group:` — confirm the set
   and the rejected-with-hint behavior (recommended as listed,
   parens over angle brackets).
3. **Bare-term semantics**: ASCII-smartcase substring, wildcards ⇒
   byte-exact glob (recommended) — or always-glob / always-fuzzy?
4. **Negation sigil**: `!` (recommended — Everything/fzf/broot) vs
   Gmail's `-`?
5. **Phase-1 predicate set**: name/glob, `dir/` ancestor, size sugar
   `>100M` + `size:`/`apparent:`, `older:`/`newer:` (durations +
   ISO dates), `kind:`, `ext:`, `is:hardlink|error|excluded`
   (recommended); anything to cut or add? `>100M` tests **disk**
   bytes (recommended — the product's default column)?
6. **Filtered-totals rule**: dir own-inodes excluded from filtered
   aggregates, stated in help (recommended) vs counting retained
   dirs' inodes?
7. **Marking under filter**: file marks allowed, dir marks refused
   with explanatory flash (recommended); "mark the matches" waits
   for the group-marking fast-follow's guard design?
8. **Composition**: `t`/`b`/donut re-aggregate over the match set;
   freeable gauge/panel untouched; cards show filtered totals with
   real totals as subtitle (recommended)?
9. **Live-during-scan**: post-scan only in phase 1, marks-idiom
   flash during scan (recommended — the flat-view live precedent
   does not transfer: its predicate is fixed at scan start, a typed
   one is not); launch-time `--filter` live accumulation as named
   phase-2 growth?
10. **Engine policy**: debounced (100 ms) synchronous rayon fold,
    (canonicalized query, deletion epoch) cache, async-placeholder
    escape hatch only on measured need (recommended) — or B's
    on-Enter fold / async from day 1? Rayon dependency accepted?
11. **Palette shape**: single Ctrl-K overlay, query-first with `>`
    command sigil, `/` as direct query-mode shortcut (recommended =
    A + C graft) vs C's full split vs command-first input?
12. **Esc ladder**: modal > palette > view mode > **active filter** >
    quit — Esc in tree view clears an applied filter, pill says so
    (recommended); or a dedicated clear key only?
13. **History & saved queries**: persisted history (new
    `$XDG_STATE_HOME/camembert/history`, cap 100) + read-only
    `[queries]` in camembert.toml, both listed in the palette
    (recommended); TUI never writes config in phase 1?
14. **CLI slice**: `--filter` (env `FILTER`) in phase 1 — `--no-ui`
    filtered summary + auto-applied TUI filter; `-o` dumps never
    filtered, stated in `--help`; `diff --filter` deferred
    (recommended)?
15. **uid/gid**: defer retention to a later wave; `user:`/`group:`
    reserved with a roadmap hint (recommended) — or pull the side
    array (+20 MB @ 10 M, scan/message/tree/dump-`ext` changes,
    research §2.4) into phase 1 to unlock the per-owner view early?
