# Option B — expression grammar, deliberate apply (the query as a sentence)

> Proposal pushed as hard as it honestly can be: the filter is a real
> **expression language** — typed, combinable, precise — applied on
> Enter, identical in the TUI and on the CLI from day one. Not a
> decision. Facts referenced from [query-research.md](query-research.md)
> (§n).

## 1. Pitch

The vision sentence is *"combiné avec « plus vieux que 6 mois », ça
devient un langage de requête"* — a **language**. Languages have
grammar: AND is not enough the day someone wants "logs *or* tmp files,
but not under node_modules", and that day is week one. Option B builds
the full grammar now — `and`/`or`/`not`, parens, typed comparisons —
and refuses the pretense that a token soup grows into a grammar
without breakage. The palette becomes a small, excellent **query
editor**: completion for field names, a parse echo, real error spans
(legitimate, because errors happen at Enter, not per keystroke). The
same string works in `--filter` for scripts and CI, unchanged,
forever. What is deliberately given up: per-keystroke re-aggregation.
The tree updates when you press Enter — a deliberate act, like
osquery, like fselect (research §4.4) — and in exchange every applied
query is exact, complete, and reproducible in a shell one paste later.

## 2. The language

### 2.1 Grammar (phase 1, complete — there is no phase-2 syntax)

```
query   := expr
expr    := expr "or" expr | expr "and" expr | "not" expr
         | "(" expr ")" | pred
pred    := field op value | glob | dirglob
field   := name | path | size | apparent | mtime | kind | depth
op      := == | != | ~ (glob-match) | > | >= | < | <=
value   := size-literal | duration | date | string | kind-name | int
glob    := bare "*.log"        (sugar for  name ~ "*.log")
dirglob := bare "node_modules/" (sugar for  path ~ "**/node_modules/**")
```

Examples, in ascending effort:

```
*.log
*.log and size > 100M
(*.log or *.tmp) and mtime < -6mo and not node_modules/
size > 1G and kind == file and depth <= 3
```

- Size literals: `parse_size` (`500M`, `1.5GiB` — one dialect,
  research §5.8). Durations: `-6mo`, `-2w` relative to now
  (`mtime < -6mo` = "older than six months"); ISO dates
  (`mtime < 2024-01-01`) for absolutes — fd's two-form convention
  (research §4.4).
- `~` is the glob operator (flat-view basename dialect; `path ~`
  unlocks full-path globs via `globset` — in *this* option path
  predicates are phase 1, because the grammar is already paying the
  parser bill).
- Operator words are ASCII keywords, not sigils: `and`/`or`/`not`
  read aloud, survive shells unescaped, and match fselect/osquery
  muscle memory (research §4.4 — the family users compose
  deliberately).

### 2.2 Semantics

Same candidate model as option A §2.2 (non-dir entries; dir-glob
sugar = ancestor constraint; hardlink extras contribute 0; filtered
totals count matching entries, dir inodes excluded and stated) — the
*semantics* are not where B differs. Age honesty identical (mtime,
research §3).

### 2.3 Error model: real errors, once, at the right time

Mid-typing, the input is just text. On Enter, the parser either
applies the filter or paints a **span-scoped error** under the input
(`mtime < 6mo` → "did you mean `-6mo` (older than)? `6mo` is a
duration, `mtime` compares to a time") with the caret on the token.
The last valid filter stays applied; Esc dismisses the error. This is
the one option where teaching-quality errors are possible at all — a
per-keystroke language can never say this much (research §4.7:
validation deferred and scoped).

## 3. UI

Ctrl-K opens the palette (same ladder rung as option A). Input is
**command-first** (VS Code untranslated): fuzzy command list on bare
typing, recents on empty; a `.` or `filter:` command — or `/` from
the tree — enters **query-edit mode**: multi-token input with
field-name completion (`si⇥` → `size `), paren matching, the parse
echo, and history recall. Enter applies; the palette closes; the
filter pill (as in option A §3.2) shows the query. No live count
while typing — a static "press Enter to apply" hint keeps the
contract explicit. Composition with `t`/`b`/donut/freeable/marking
is identical to option A §3.3 (including the dir-marking refusal).

## 4. Engine: simple, because deliberate

Fold on apply, **single-threaded**, on the UI thread: 0.3–0.7 s @
10 M entries behind a "filtering…" placeholder row (the freeable
panel's idiom), sub-frame at ≤ 1 M. No debounce, no latest-wins race,
no rayon dependency — one fold per Enter is human-frequency, exactly
the regime flat-view 4B said single-threaded serves fine. The
alternate dir-aggregate table, match bitvec, epoch invalidation and
`view.rs` plumbing are identical to option A §4 (the engine substrate
is shared across options; only the *trigger policy* differs). If a
later option-A-style live mode is ever wanted, rayon can be added
then — B never needs it.

## 5. CLI: the strongest story of the three

`--filter 'EXPR'` ships day one with the *same parser*:
`--no-ui --filter '(*.log or *.tmp) and mtime < -6mo'` is a complete,
precise, scriptable probe — and `camembert diff --filter` is a
near-free phase 2 because the grammar already speaks fields that
exist in dumps. The TUI and CLI can never drift (one grammar, one
crate-level parser in `camembert-core/src/query.rs`). Dumps are
never filtered (`-o` full-tree, stated in `--help`) — identical to A.

## 6. History, saved queries, docs

Identical mechanics to option A §6 (`$XDG_STATE_HOME` history,
read-only `[queries]` in config) — queries here are longer, so saved
queries pull *more* weight. Docs same-change rule applies; the README
section is necessarily a small language reference (a real cost: B's
documentation is a grammar, A's is a table of tokens).

## 7. Honest weaknesses

1. **It kills the live feel.** The product whose identity is
   browse-during-scan ships a filter that updates on Enter. Everything
   and fzf are the loved precedents (research §4.10), and both are
   per-keystroke; fselect and osquery are respected and *niche*. This
   is the central bet, stated plainly: B trades the demo-magic moment
   for precision, and the field evidence says the magic is what
   converts.
2. **Typing cost of the common case**: `*.log and size > 100M and
   mtime < -6mo` vs A's `*.log >100M older:6mo` — roughly double the
   keystrokes for the query every user starts with, paid every time.
3. **Learnability cliff**: `mtime < -6mo` (a negative duration as a
   timestamp) must be *learned*; no bare-terms-first on-ramp exists
   for the numeric fields. Completion and error quality mitigate;
   research §4.6 (jq/PromQL) documents how far "good errors" carry a
   grammar with casual users — not far.
4. **Palette tension**: the reserved Ctrl-K promises a palette; B's
   palette hosts a query *editor* as a sub-mode, and editors inside
   single-line palette inputs accrete feature requests (multi-line?
   cursor movement? paren highlight?) that ratatui makes expensive.
5. **Grammar freeze**: shipping `and`/`or`/`not`/parens in phase 1
   means the syntax surface is maximal on day one — every
   precedence and sugar choice is immediately load-bearing and
   near-impossible to change (the dump-format "major versions are
   near-taboo" logic applies to query strings saved in configs and
   scripts).
6. Shared with A: excluded-mount/error-dir blind spots; the
   dir-inode accounting rule needs stating.
