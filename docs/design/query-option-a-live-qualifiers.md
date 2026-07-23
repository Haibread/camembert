# Option A — live qualifier tokens (Everything's soul in the palette)

> Proposal pushed as hard as it honestly can be: the filter is an
> **instant, per-keystroke narrowing** of the whole tree, spoken in the
> qualifier-token dialect users already know from Everything, GitHub
> and Gmail. Not a decision. Facts referenced from
> [query-research.md](query-research.md) (§n).

## 1. Pitch

The product's identity is *instant + honest*. Browse-during-scan made
"instant" the feel of scanning; this option makes it the feel of
**asking**. Ctrl-K opens one input; you type `*.log >100M older:6mo`
and the tree, cards, bars and donut re-aggregate under your fingers —
debounced, parallel-folded over the frozen arena, never blocking a
keypress. The language is the qualifier-token family — the only syntax
family with direct evidence of *both* expressive-enough predicates and
per-keystroke fitness (research §4.1–§4.3, §4.10): bare terms work
with zero syntax knowledge, qualifiers are opt-in layers on the same
parser, and **no input is ever an error**. The same input, behind a
`>` sigil, is the command palette (research §4.7) — fuzzy over every
key-map action, recents on empty input. One surface, two modes, one
keystroke away.

No surveyed disk analyzer offers an interactive typed filter that
re-aggregates (research §4.6, §4.8) — this is open field, squarely
on-thesis.

## 2. The language

### 2.1 Grammar (phase 1)

A query is whitespace-separated **terms**, implicitly ANDed. A term is
one of:

| term | meaning |
|---|---|
| `report` | bare word: substring match on the basename, ASCII-smartcase (all-lowercase input matches case-insensitively; any capital makes it exact — the fzf convention) |
| `*.log`, `data?` | contains `*`/`?`: basename glob, exactly the flat-view D4 dialect (`{}`/`[]` literal, raw bytes, case-sensitive) |
| `node_modules/` | trailing `/`: **ancestor constraint** — matches entries lying under a directory whose name matches the glob (the `[patterns]` convention, reused) |
| `>100M`, `<1G` | size sugar: disk bytes (the product's default column), `camembert-core` `parse_size` literals (`500M`, `1.5GiB`, `2gb` — one dialect, research §5.8) |
| `size:100M..2G` | explicit size qualifier with ranges; `apparent:>1G` tests apparent bytes |
| `older:6mo`, `newer:2w` | mtime age; durations `h/d/w/mo/y` (fd's humantime family, research §4.4) or absolute dates (`older:2024-01-01`) |
| `kind:file` | `file`, `dir`, `symlink`, `other` |
| `ext:log` | sugar for the `*.log` file glob |
| `is:hardlink`, `is:error`, `is:excluded` | node flags already stored (research §2.1) |
| `!term` | negation of any term (`!*.o`, `!older:1y`, `!node_modules/`) — `!`, not `-` (Everything/fzf/broot convention; `-` starts real filenames more often than `!`) |

**Reserved, rejected with a hint today** (so phase 2 needs no breaking
change): `|` (OR), `(`/`)` and `<`/`>` grouping, `;` value lists
(`ext:log;tmp`), `/`-containing path globs (`src/**/*.c`), `depth:`,
`entries:`, `user:`/`group:` ("needs owner data — not captured yet;
planned"). Every reserved token has a one-line hint so the user learns
the roadmap from the error, not the README.

### 2.2 What a query means (the semantics, precisely)

**Candidates are non-directory entries** (files, symlinks, devices —
`kind:` selects among them; default everything non-dir). A candidate
matches when every term holds:

- name terms and `ext:`/`kind:`/`is:`/size/age terms test the entry
  itself;
- `dir/`-terms test the ancestor chain: satisfied iff some ancestor
  directory's name matches. Implemented as inherited per-dir state in
  the topological coverage pass — O(dirs), not O(nodes·depth)
  (research §2.2, the flat fold's own trick);
- `HARDLINK_EXTRA` entries contribute 0 and never match (the tree's
  own convention); tombstoned rows are invisible by construction.

The **filtered view** is the tree re-aggregated over the match set:
every directory's row shows filtered `disk/apparent/items`; dirs with
zero matching descendants disappear; proportion bars and the donut are
relative to filtered totals. Directory *inodes'* own bytes are not
part of any match set — filtered totals count matching entries only,
stated in the help line (the alternative, counting retained dirs' own
inodes, makes totals depend on which dirs happen to be retained — a
wrong-but-plausible number; ~4 GiB across 1 M dirs is big enough to
matter, so the rule is explicit, not silent).

Age honesty (research §3): `older:` is **mtime** — "not modified",
never "not read" — one hint line in the palette help and the README,
plus the known mtime lies (`cp -p`, `rsync -a`) documented once.

### 2.3 Error model: never mid-typing

The Everything rule (research §4.2), verbatim: a trailing incomplete
term (`size:>`) is **inert** until it parses; a complete-but-unknown
qualifier (`sixe:10`) is inert with a dim status-line hint ("unknown
qualifier `sixe:` — term ignored"); the palette echoes the **parsed
interpretation** of the input on a status line (broot's resolved-echo
lesson, research §4.6) so the user always sees what is actually being
asked. There is no red state while the cursor is in the input.

## 3. UI: one palette, a filter pill, an honest cockpit

### 3.1 Ctrl-K (and `/`)

Ctrl-K opens a centered overlay (new modal-ladder rung: confirm >
review > freeable > **palette** > cheatsheet): one input line, a list
below, a status line. The input is **query-first**: bare typing is
filter terms; a leading `>` switches to command mode (VS Code sigil,
inverted priority — camembert's palette exists *for* the query
language). `/` from the tree opens the same palette already in query
mode — k9s muscle memory for free, one surface to maintain.

- **Query mode list**: live match count ("4 312 files · 1.2 GiB",
  debounced with the fold), parse echo, matching history entries,
  saved queries. Enter applies and closes; Esc closes without
  changing the applied filter.
- **Command mode list**: fuzzy (subsequence) over every key-map
  action — *generated from the `keymap.rs` dispatch table*, the same
  source as the `?` cheatsheet, so the palette can never drift from
  the keys — plus palette-only commands ("clear filter", saved
  queries by label). Recents on empty input (research §4.7: never a
  blank box).
- During the scan, command mode works; query mode shows the
  marks-idiom flash ("filter available when the scan completes",
  §5).

### 3.2 The filter pill

An applied filter renders as a **pill in the header** next to the
breadcrumb: `⧩ *.log older:6mo · 1.2 GiB · 4 312 files · Esc clears`.
Metric cards show filtered totals with the real total as subtitle
("1.2 GiB matched · of 120 GiB scanned") — the filter never hides
what the scan knows. Esc ladder grows exactly one level: modal >
palette > view mode > **active filter** > quit; the pill says so, and
`q`/Ctrl-C still always quit.

### 3.3 Composition with everything that shows numbers

- **`t` flat / `b` breakdown**: compose — top-N over the match set,
  groups over the match set (one shared fold pass; this *is* the
  deferred breakdown drill-down's engine, and phase 2's Enter-on-a-
  group seeds the filter with that pattern).
- **Donut**: filtered aggregates, identity colors unchanged.
- **Freeable gauge/panel**: untouched — the ledger is process
  evidence, scan-scoped (freeable D8); the gauge suffix stays
  scan-scoped and the panel says nothing about filters.
- **Marking**: `Space` on a *file* row works (real node, shared
  basket). `Space` on a *directory* row under an active filter is
  **refused** with a flash: "this directory shows only matching
  files — marking it would delete everything in it; mark files, or
  clear the filter". This is the honesty trap (research §5.3) closed
  at the cheapest possible point; "mark the matches" is the
  group-marking fast-follow with its guard design, not smuggled in
  here.
- **Dump/diff**: never affected. `-o` writes the full tree, always.

## 4. Engine: the fold that finally earns rayon

New `camembert-core/src/query.rs`:

- `parse(&str) -> Query` (terms + reserved-token hints; pure, shared
  with the CLI);
- `filter_fold(&Tree, &Query, epoch) -> FilterGeneration`:
  - **pass 1** (sequential, O(dirs)): topological dir-table walk
    propagating ancestor-constraint state — the flat fold's coverage
    pass shape;
  - **pass 2** (parallel): arena chunks folded by rayon workers, each
    producing partial per-dir own-bytes buckets + a match bitvec
    segment + partial counts, merged; name verdicts memoized per
    interned name (`NameMemo` reused, research §2.2);
  - **pass 3** (sequential, O(dirs)): reverse topological sweep
    summing child-dir subtotals into an **alternate dir-aggregate
    table** — scan-tree option C §9's shape, landing where it was
    reserved (scan-tree D1).
- `FilterGeneration` = alternate dir aggregates (32 B × dirs ≈ 32 MB
  @ 1 M dirs) + match bitvec (1.25 MB @ 10 M) + totals. Snapshot
  building reads filtered aggregates when a generation is active;
  `build_snapshot` gains an `Option<&FilterGeneration>`.

**Budget, grounded** (research §2.3): per-node work is the flat
fold's band (~66 ns measured for the accumulator's comparable work).
Single-threaded: ~5–15 ms @ 200 k, 0.3–0.7 s @ 10 M. With rayon on 8
cores, sequential dir passes bounding: **≈ 2–5 ms @ 200 k, ≈ 50–120 ms
@ 10 M**. Policy: recompute on a **100 ms debounce** after the last
keystroke (fzf's reload convention, research §4.6), synchronously on
the UI thread — worst case 2–4 dropped frames on a 10 M tree, at a
typing pause, with the previous generation still on screen until the
new one lands. Escape hatch if a 10 M bench disagrees: the freeable
"computing…" placeholder pattern with `Phase::Done` holding
`Arc<ScanOutcome>` so a worker thread can fold while the UI breathes
(deliberately *not* built until measured need — it complicates
deletion's `&mut`).

Invalidation: a generation is keyed on (canonicalized query, deletion
epoch) — the `FlatSummary` epoch pattern verbatim. A deletion under an
active filter recomputes on the next render, same as flat.

**Live-during-scan: post-scan only, phase 1.** During the scan the UI
has no arena reference at all (research §2.5) and D2's "no whole-tree
fold during the scan" binds. The flat view went live because its
patterns are *fixed at scan start* — O(1) incremental accumulation
works for a known predicate, not for one the user retypes mid-scan
(each retype would be a fold on the owner thread, stalling
integration). The one honest live variant — a `--filter` fixed at
launch, accumulated by the owner like the flat accumulator — is named
as phase-2 growth, not smuggled into phase 1.

## 5. CLI surface (phase 1)

- `--filter 'QUERY'` (env `FILTER`): in the TUI, applies the filter
  automatically at scan end (pill visible, clearable like any
  filter); with `--no-ui`, the summary shows filtered totals + top
  matching files. **Never** changes what `-o` writes — dumps are
  interchange truth (research §5.5); the `--help` text says so
  explicitly.
- `camembert diff --filter` is named phase-2 (the predicate
  vocabulary applies to dump entries; the streaming merge-join can
  filter both sides — designed then, not now).
- Same-change documentation: `--help` gets the full term table;
  README gets a "Filtering" section; the cheatsheet gets Ctrl-K, `/`,
  and the Esc-clears-filter line (CLAUDE.md rule).

## 6. History and saved queries

- **History**: last 100 applied queries in
  `$XDG_STATE_HOME/camembert/history` (first use of a state file —
  one line per query, newest last, trimmed on write; absent/broken =
  empty history, never fatal). Up/Down and fuzzy recall inside the
  palette (research §4.7: single-list fuzzy recall, not Ctrl-R
  cycling).
- **Saved queries**: `[queries]` in `camembert.toml` (label = query
  string, per-section-resilient like `[patterns]`), listed in both
  palette modes by label. Read-only phase 1 — the TUI does not write
  config (a "star this query" write-back is phase-2 UX with its own
  care).

## 7. Module boundary

Parser + fold + `FilterGeneration` in `camembert-core/src/query.rs`
(pure functions of `&Tree`, unit-testable on synthetic trees — the
`flat.rs` precedent); palette UI in `camembert/src/ui/palette.rs`;
zero changes to scan, dump, diff, freeable; `view.rs` gains the
optional filtered-aggregate source; rayon enters `camembert-core` as
the fold's dependency (its reserved customer, flat-view 4B). An
invariant test pins the fold: empty query ⇒ filtered aggregates ==
`DirMeta` aggregates minus dir own-inodes (the documented delta), and
a property test drives random queries against a brute-force oracle.

## 8. Phase-2 growth (designed for, not built)

`|` OR + grouping; `;` value lists; path globs via `globset`
(coverage-pass propagation, research §2.4); `depth:`/`entries:`;
`user:`/`group:` once the uid side array lands (research §2.4 sketch:
statx mask + interned `Vec<u16>`, +20 MB @ 10 M, dump `ext:true`);
launch-time `--filter` live accumulation; group-marking under filter
(with the flat-view fast-follow's guard design); breakdown
drill-down seeding the filter; `diff --filter`; "freeze and refine"
(telescope's two-stage narrowing) if big match sets want it.

## 9. Honest weaknesses

1. **The debounced fold is synchronous**: on a 10 M-entry tree each
   recompute costs ~50–120 ms of UI-thread time. Amortized behind a
   typing pause it reads as instant; on a slow/throttled box it is a
   visible hitch. The async escape hatch exists but is real work
   (Arc ownership vs deletion's `&mut`).
2. **Implicit-AND-only phase 1**: `*.log | *.tmp` — a completely
   reasonable first query — is rejected-with-hint until phase 2. The
   `ext:log;tmp` list form would cover the common case and is also
   deferred; the hint text has to carry the weight.
3. **Smartcase substring vs case-sensitive glob** is two matching
   regimes in one input; the parse-echo line mitigates, but a user
   can be surprised that `Report` (substring) is exact while `R*`
   (glob) is byte-exact too and `report` isn't.
4. **Reserved-token debt**: every reserved sigil (`|`, `()`, `<>`,
   `;`, `/`) is a promise; if phase 2 chooses differently the hints
   were wrong. The reservation list must be settled in the session,
   not improvised.
5. **A new UI habitat** (overlay palette with completion, echo, and
   list) is the largest single UI piece since the cockpit itself —
   more code than the fold and parser combined.
6. **Excluded-mount rows** carry no subtree data, so a filter can
   neither match inside them nor honestly claim they're empty; they
   are simply absent from filtered views (hint in help). Same for
   error dirs' unscanned contents — the filter inherits the scan's
   blind spots, and only the errors card says how blind.
