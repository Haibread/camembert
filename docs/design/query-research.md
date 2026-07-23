# Filter query language + Ctrl-K palette — research

**Status: research notes — no design decisions in this file.** Facts,
prior art and constraints feeding the
[options dossier](query-options.md). Covers HANDOFF next-step §1
("Filter query language + Ctrl-K palette — they ship together; the
palette is the language's UI, reserved in
[tui-design.md](tui-design.md)") and the original vision in
[handoff-original.md](handoff-original.md) §"Vues et requêtes":

> **Filtre qui ré-agrège** : filtrer `*.mp4` recalcule tout l'arbre sur
> le sous-ensemble ; combiné avec « plus vieux que 6 mois », ça devient
> un langage de requête. Même mécanique d'agrégation que le scan,
> réappliquée.

plus §"Dimension âge" (big **and** cold; relatime/noatime honesty) and
the per-owner (uid/gid) aggregation bullet. Open questions at the end.

## 1. What this feature inherits (settled decisions that bind it)

- **[flat-view-decisions.md](flat-view-decisions.md)** drew the
  boundary deliberately: the query language owns *full-path globs*,
  *expressions/combinators*, *mtime/size predicates*, *breakdown
  drill-down*, and the non-interactive pattern/JSON surface that D5
  refused to ship early. Group-level marking ("mark every
  node_modules") is a separate fast-follow **with its own guard
  design** — this feature must not accidentally ship it without that
  guard.
- **[flat-view-decisions.md](flat-view-decisions.md) D2** is the
  live-during-scan precedent: browse-during-scan is the product's
  identity, and flat/breakdown joined it via O(1) owner-side
  incremental accumulation — *not* by folding during the scan ("no
  whole-tree fold runs during the scan" is binding language).
- **[scan-tree-decisions.md](scan-tree-decisions.md) D1** noted Option
  C's frozen-structure substrate "for wave 2–3 (parallel filter/diff
  folds over the post-scan frozen tree)".
  [scan-tree-option-c-snapshot.md](scan-tree-option-c-snapshot.md) §9
  is the original sketch: *a filter is a rayon fold over the frozen
  arena producing an alternate dir-aggregate table (~0.2 s
  single-threaded, <100 ms on 8 cores), published as a snapshot over
  the same structure*. Flat-view option 4B rejected rayon for its own
  fold precisely because "wave 3's per-keystroke re-aggregation is
  where parallel folds earn their dependency".
- **[tui-design.md](tui-design.md)** reserves **Ctrl-K**: "command
  palette — reserved; ships with the filter/query language (wave 3) as
  its UI".
- **[freeable-decisions.md](freeable-decisions.md) D8**: the ledger is
  scan-level process evidence, never tree numbers — a filtered tree
  does not touch it.
- **[dump-format-decisions.md](dump-format-decisions.md)**: dump v1
  reserves an `ext` capability (uid/gid/mode); the writer currently
  emits `ext:false` (HANDOFF known limitation). Major-version changes
  are near-taboo.

## 2. Grounding: what the code stores today (verified against `main`)

### 2.1 The node — what a predicate can read for free

`camembert-core/src/tree.rs` — `Node` is **exactly 32 bytes**,
const-asserted (`assert!(size_of::<Node>() == 32)`):

| field | bits/bytes | query use |
|---|---|---|
| name ref | 26 bits of `name_kind: u32` | basename (interned, raw bytes) |
| kind | 3 bits | `Dir/File/Symlink/Block/Char/Fifo/Socket/Other` |
| flags | 3 bits (**full**) | `HARDLINK_EXTRA`, `ERROR`, `EXCLUDED` |
| parent | `u32` | ancestor chain / path reconstruction |
| apparent | `u64` | apparent size |
| disk | `u64` | `st_blocks * 512` |
| mtime | `i64` | age (unix seconds) |

**Stored and queryable at zero cost**: basename, kind, both sizes,
mtime, error/excluded/hardlink-extra status, parent chain (hence depth
and full path, by walking). `Tree::is_hardlink(node)` is O(1) (flag +
`hardlink_firsts` side set).

**Not stored anywhere**: **uid/gid**, atime, ctime, nlink count, mode,
ino/dev per file (dev exists per *directory* in `DirMeta.dev`; the
hardlink registry's `(dev, ino)` map is scan-transient). A predicate
over any of these is a **data-retention question**, not a parser
question — costs in §2.4.

### 2.2 The dir table — what re-aggregation composes from

`DirMeta` (~80 B, parallel arena): node, parent, child **run lists**
(D2), subtree aggregates `ta/td/tn/te` maintained through deletions,
`dev`, state. Two structural facts the flat fold already exploits and a
filter fold inherits:

- **`DirId` order is topological** (parent index < child index): one
  forward pass propagates any inherited per-dir state (the flat fold's
  coverage pass); one *backward* pass sums per-dir subtotals into
  subtree totals. No recursion, no hashing.
- **Names are interned** (`name_count()` sizes the memo): a glob is
  evaluated once per *unique name* and memoized in a dense per-name-id
  vector (`flat.rs` `NameMemo`, 2 B/name/kind — the machinery is
  directly reusable for query name predicates).

### 2.3 Measured / grounded engine costs

- Node arena @ 10 M entries = 320 MB; dir table ≈ 80 MB @ ~1 M dirs.
  A full streamed pass has a DRAM-bandwidth floor of **15–40 ms @
  10 M** (flat-view dossier grounding).
- The live flat accumulator measures **~66 ns/node** (HANDOFF) for
  memo lookup + counter add + heap compare — the realistic per-node
  cost band for predicate evaluation + bucket adds. Extrapolation for
  a full filter fold, single-threaded: **200 k ≈ 5–15 ms; 10 M ≈
  0.3–0.7 s**. Scan-tree option C §9's independent estimate agrees
  (~0.2 s single-threaded @ 10 M, < 100 ms on 8 cores).
- The bench tree (200 k files) scans in ~74 ms total (`--no-ui`,
  warm); the flat fold at that scale is already sub-frame. **The
  per-keystroke question only bites at multi-million-entry scale.**
- The parallelizable part is the per-node predicate + own-bytes
  accumulation (chunk the arena, per-thread partial buckets, merge);
  the two dir-table passes are sequential but O(dirs) ≈ few ms @ 1 M
  dirs. So a rayon fold's honest budget @ 10 M is **~50–120 ms**, and
  the sequential passes bound the speedup (Amdahl, stated up front).
- An **alternate dir-aggregate table** (option C §9's shape) costs
  32 B × dirs ≈ **32 MB @ 1 M dirs** per filter generation; a per-node
  match bitvec costs 1 bit/node ≈ **1.25 MB @ 10 M** (feeds flat mode,
  marking guards, and match counts).

### 2.4 Cost per candidate predicate field (the retention menu)

Fields a query could want, with the *precise* cost of making them
queryable. The 32-byte node is sacred (const-asserted, D4 memory
budget); anything new goes in side arrays or stays unstored.

| field | today | to make queryable | cost |
|---|---|---|---|
| basename glob | interner + memo | reuse `flat.rs` memo | ~0 (2 B/unique-name/kind, transient) |
| full-path glob | parent chain walk | match *dir* segments during the topological coverage pass, propagate down — O(dirs), not O(nodes) | small CPU; needs a real glob matcher (`**`), see §4.5 |
| size (both) | on node | — | 0 |
| mtime age | on node | — | 0 (honesty caveat §3) |
| kind | on node | — | 0 |
| error / excluded / hardlink | flags + side set | — | 0 |
| depth | derivable | compute during fold walk | 0 |
| per-dir entry count | `DirMeta.tn` | — | 0 (dir-level predicates) |
| **uid/gid** | **not captured** | add `STATX_UID\|STATX_GID` to the worker mask (the kernel fills basic stats anyway — no extra syscall); carry through `EntryStat`/message/owner; **side array** on the tree: interned-uid `Vec<u16>` + tiny uid table (machines have few uids) | **+20 MB @ 10 M** (u16) or +40 MB raw u32; touches worker/message/owner/tree; dump round-trip wants `ext:true` (slot exists, reader must accept both) |
| atime | not captured | side `Vec<i64>` | +80 MB @ 10 M **and** the honesty problem (§3) — weakest value/cost of the menu |
| nlink (count) | partial | `is_hardlink` is O(1); the count itself is scan-transient | store only if a predicate needs the number (none identified) |
| mode/permissions | not captured | side array + statx mask | no identified query; defer indefinitely |
| ino/dev per file | dev per dir only | — | no identified query; defer |

Key asymmetry: **uid/gid capture is nearly free at scan time but is a
cross-cutting change** (worker → message → owner → tree side array →
dump `ext` → ncdu import has no uid pre-2.x… import would carry
`ext:false`). Everything else in phase-1 reach is already on the node.

### 2.5 UI structures the palette must coexist with

`camembert/src/ui/` (state.rs, keymap.rs, ui.rs):

- **Modal ladder** (Esc/precedence, in order): delete-confirm >
  review list > freeable panel > cheatsheet; below modals, contextual
  Esc leaves a view mode (`t`/`b` → tree), and only quits from tree
  view; `q`/Ctrl-C always quit. A palette is a new rung; a persistent
  *filter* would be a new **Esc ladder level** below modes.
- **View modes**: `ViewMode::{Tree, FlatTop, Breakdown}`; mode-fed
  donut; flat rows are real nodes (marks work post-scan; live rows
  are basename-only, no path — `try_toggle_mark_flat` refuses during
  scan).
- **Keys in use**: j/k ↓/↑, g/G, p (apparent col), u (unmark all), ?,
  z, t, b, d/a/n/m/c/e (sorts), Enter/l/→, Bksp/h/←, Space, v, D, f,
  Esc, q/Ctrl-C. **Free: `/`, `s`, `o`, `w`, `x`, `r`, `i`, `Ctrl-K`
  (reserved for this feature)** among others.
- **Phases**: `Phase::Scanning(LiveScan)` (arena owned by the scan
  owner thread; UI reads arc-swap snapshots, requests nav via a
  latest-wins cell) vs `Phase::Done(ScanOutcome)` (UI thread owns the
  frozen arena, `serve_local` + `view::build_snapshot`). **During the
  scan the UI cannot fold the arena at all** — it has no reference to
  it. Any live filtered view must come from the owner thread, like the
  flat accumulator does.
- **Flat/query cache precedent**: `FlatSummary` carries a deletion
  `epoch`; `ensure_flat_summary_fresh` recomputes on render-time
  mismatch — the invalidation pattern a filter generation reuses.
- **Config**: `camembert.toml` with per-section-resilient parsing
  (D4); `[patterns]` label→glob in file order. An eventual `[queries]`
  (saved queries) drops into the same loader. No XDG *state* dir is
  used yet (a persisted palette history would introduce one:
  `$XDG_STATE_HOME/camembert/`).
- **Size literals already have a parser**: `camembert-core/src/size.rs`
  `parse_size` — `500M`, `2G`, `1.5GiB`, `2gb`; case-insensitive,
  binary units regardless of suffix spelling (`--threshold` uses it).
  The query language should reuse it verbatim (one size dialect per
  binary).
- `--filter` clashes with nothing (`-e`, `-x` etc. unused; clap
  subcommands `diff`/`import` exist — a global-ish scan flag is fine).

## 3. Age semantics: the honesty note (from the original handoff)

The vision's age dimension ("big **and** cold") comes with an explicit
honesty caveat: **atime is not trustworthy** — `relatime` (the
default) updates atime at most daily and only under conditions;
`noatime` never; and network/fuse filesystems do their own thing. The
handoff's position: *detect and fall back to mtime while announcing
it*. camembert stores **only mtime** — so phase-1 age predicates are
mtime-age by construction, and the honest statement is:

- `older:6mo` means "**not modified** in 6 months", not "not read".
  A file read daily but written once ranks as cold. Documented
  wherever the predicate is (help line, README).
- mtime itself lies in known ways: `cp -p`/`rsync -a`/`tar -x`
  preserve source mtimes (a file copied yesterday can be "10 years
  old"); moves keep mtime; a directory's own mtime reflects only
  direct-child churn (age queries should therefore match *files* and
  re-aggregate, not test directory mtimes).
- If atime is ever wanted, it is a *retention* cost (§2.4) **plus** a
  per-mount trust problem (parse `/proc/self/mounts` options to
  detect `noatime`/`relatime` and badge the result) — the research
  found no analyzer that does this honestly; qdirstat and every
  du-family tool use mtime for age. Defensible to defer indefinitely.

## 4. Prior art — query syntaxes users already know

(Web survey, 2026-07. Full syntax families first, then what each
contributes.)

### 4.1 The four families

1. **Flag-predicates** (fd, dust, gdu): each predicate is a CLI flag
   (`fd -e log --size +100M --changed-before 6months`). Composes in
   shells, useless inside a single interactive input line.
2. **Qualifier tokens** (Everything, GitHub search, Gmail):
   whitespace-separated terms, implicit AND, `field:value` qualifiers
   with comparison sugar (`size:>100mb`, `dm:lastmonth`), bare terms
   match names. Designed for *search-as-you-type*.
3. **SQL / expression DSLs** (fselect, osquery, jq, PromQL): full
   grammars with typed comparisons and boolean operators. Precise,
   completable only with real tooling, hostile to per-keystroke use.
4. **Fuzzy + operators** (fzf, telescope, broot): fuzzy match by
   default with a small operator vocabulary (`'exact`, `^prefix`,
   `!neg`, `|` OR). Instant, but weak for numeric/date predicates.

### 4.2 Everything (voidtools) — the qualifier-token reference

The de-facto gold standard for instant filtering over millions of
files. Syntax: space = implicit AND, `|` = OR, `!` = NOT, `<...>` =
grouping (angle brackets, *not* parens — literal parens are common in
filenames); documented precedence `<> ! AND OR`. Qualifiers:
`size:>1mb`, `size:2mb..10mb`, named buckets (`size:huge`),
`ext:jpg;png;gif` (`;` = value-list OR), `dm:lastmonth` /
`dm:>2024-01-01` / `dm:today` (modified; named relative buckets *and*
absolute dates/ranges), `dc:` (created), `parent:`; wildcards in bare
terms. Two properties made it stick:

- **results update per keystroke** against an index of millions of
  entries, and
- **no input is ever an error** — an incomplete `size:>` just matches
  nothing (or everything) until it parses; there is no "syntax error"
  state in the loop. Plain terms work with zero syntax knowledge;
  qualifiers are opt-in layers on the same parser (the documented
  reason it is learnable).

### 4.3 GitHub / Gmail qualifiers

Same family, hosted in a plain search box: `size:>100`,
`language:rust`, ranges `n..m` with `*` open ends (`stars:*..10`),
negation `NOT`/`-qualifier:value` (GitHub/Gmail use `-`, Everything
uses `!`). Gmail: `larger:5M`, `older_than:1y`, `newer_than:2d`,
`has:attachment`. Users demonstrably learn qualifiers *lazily* — bare
terms first, one qualifier at a time; docs lead with examples, not
grammar. Two documented frictions: OR is awkward (uppercase `OR`
keyword most users never find), and GitHub code search **requires at
least one free-text term** (`language:rust` alone is invalid) — a
constraint camembert must not copy (`>100M` alone is a legitimate
query).

### 4.4 fd / fselect / osquery — the two poles of power

- **fd**: pattern is regex by default (`-g` for glob); `--size +100M`
  / `-1G` / `5k..10k` (`+`/`-` = at-least/at-most). fd distinguishes
  decimal `b/k/m/g/t` from binary `ki/mi/gi/ti` — precise but
  double-dialect; camembert's `parse_size` is deliberately one
  dialect (binary whatever the spelling). `--changed-within 2weeks` /
  `--changed-before "2018-10-27 10:00:00"` accept humantime durations
  (`10h`, `1d`, `2weeks`) *or* absolute dates — the closest thing to
  a CLI convention for ages, worth copying.
- **fselect** (`fselect size, path from /tmp where size > 100mb and
  name = '*.log'`): SQL-over-files. Reception (HN) is genuinely
  positive — *from people who already think in SQL, evaluating it as
  a batch tool*. Nobody discusses typing SQL per keystroke; a
  half-typed `where size >` is structurally unparseable, which is the
  family's disqualifier for a live filter box.
- **osquery**: same lesson at platform scale — SQL shines for
  *stored/audited/one-shot* queries composed in a REPL, not for
  exploratory live narrowing.

### 4.5 Glob dialects (gitignore / globset)

gitignore semantics (`*` non-separator, `**` any depth, trailing `/`
dir-only, `!` re-include) are the most widely *known* path-pattern
dialect. camembert's flat view already speaks a subset (basename
`*`/`?` only, trailing `/` = dir pattern, D4) — a query language that
contradicts it would be a self-inflicted wound. `globset` (ripgrep's,
byte-oriented) is the crate if/when full-path `**` globs arrive;
flat-view's hand-rolled matcher covers basenames today.

### 4.6 Interactive TUI precedents

- **broot** is the closest existing thing to "filter re-aggregates a
  tree": typing filters the tree live and the tree re-composes around
  matches with their sizes. Input grammar `<mode><pattern>[/<flags>]`:
  no prefix = fuzzy path, `n/` fuzzy name, `/regex/` regex, `e/`
  exact, `c/` content — patterns **compose with `!`, `&`, `|` and
  parens** (docs even note operand order matters for speed: cheap
  filters first). Verbs (commands) share the same input behind a
  space/`:` sigil with first-unique-prefix matching, Tab completion,
  and — load-bearing lesson — **the status line echoes the fully
  resolved command before execution**, doubling as documentation and
  error surface. Pain point: the mode-sigil grammar is discoverable
  only from docs.
- **k9s** splits the two surfaces: `:` command mode (`:pod`,
  `:pod app=fred,env=dev` — verbs with structured args, light
  autocomplete) vs `/` filter mode (live regex over the current view,
  `!filter` inverts) — two keys, two prompts, no mode confusion;
  widely praised as learnable. The split is semantic: `:` = "go
  somewhere / do something", `/` = "narrow what I'm looking at".
- **telescope.nvim**: prompt + live list + preview; its
  live-grep-args extension adds `<C-space>` "freeze current results,
  then fuzzy-refine *within* them" — a narrow-then-narrow-again
  two-stage filter directly applicable to a big match set.
- **fzf**'s extended syntax (`'exact ^pre .log$ !excl |` OR, space
  AND) shows how much operator power fits in an incremental matcher
  before users stop reading it as "just type things"; its `reload`
  idiom debounces expensive backing queries ~50–150 ms while cheap
  in-memory filtering runs every keystroke undebounced.
- **Textual** (Python TUI framework) ships a palette blueprint: a
  provider contract with per-keystroke `search()` yielding scored
  hits and a separate `discover()` for the empty-input state — the
  clean architectural split between "match what I typed" and "show
  something useful when I typed nothing".
- **ncdu/qdirstat/gdu/dust**: scan-time exclusion flags or one-axis
  regex only (ncdu `--exclude`, gdu `-I`, dust `-e`/`-v` regex);
  qdirstat filters by click, not by typing. The surveyed field still
  has **no analyzer that re-aggregates the tree under an interactive
  typed filter combining name + size + age** (flat-view research
  reached the same conclusion from the other side). The niche is
  open.

### 4.7 Command palettes (VS Code / Slack / Linear / Raycast)

Converged conventions worth adopting wholesale:

- One overlay, one input, a ranked list below; **fuzzy
  (subsequence) match over command names** — "sort asc" matches
  "Sort Lines Ascending"; **recents/suggestions on empty input,
  never a blank box**; Enter runs, Esc closes; repeated hotkey
  toggles closed; arrow keys + type-to-narrow; grouped sections.
- **Sigil-switched modes in a single input**: VS Code's Ctrl-P is
  files by default, `>` switches to commands, `@`/`#` other scopes,
  and typing `?` lists all prefixes (self-documenting escape hatch).
  Ctrl-Shift-P is just Ctrl-P with `>` pre-typed.
- **Parameterized commands** resolve one of three ways: pick from a
  filtered sub-list when the argument space is enumerable (Linear
  submenus); short step-counted prompt chains (VS Code quick-pick —
  whose own guidelines say it is *not* a wizard); or inline
  chips/ghost text in the same line (Raycast arguments, Warp/Fig
  autosuggestions) — the last is the closest analog to typing a
  query expression.
- **Validation deferred and scoped**: never a hard error mid-typing;
  at submit, flag the specific bad token, not the whole input. Live
  "N results" counts are the preferred feedback channel.
- History: fuzzy-searchable single-list recall beats bash-style
  Ctrl-R cycling (fzf-history, McFly — frecency-ranked); Grafana's
  query history uses a **two-tier model**: rolling auto-pruned
  recents + explicitly starred/saved queries. Slack even gives
  recents their own shortcut, separate from search.

## 5. Constraints assembled (what any option must satisfy)

1. **The 32-byte node is untouchable** (const-asserted; D4 budget).
   New per-node data = side arrays, or stays unstored.
2. **During the scan the UI has no arena access** (Phase::Scanning);
   only the owner thread could compute live filtered aggregates, and
   D2's "no whole-tree fold during the scan" language binds. A
   mid-scan *typed* filter change cannot re-fold without stalling
   integration — any live-filter story is launch-time-filter-only or
   violates the identity.
3. **Totals must stay honest**: a filtered view shows filtered
   aggregates; anything the user can *delete* from a filtered view
   must make brutally clear whether it deletes the match set or the
   real subtree (the "mark a dir that shows 42 MB of logs, delete
   300 GB" trap). Group marking has a deferred guard design owned by
   the flat-view fast-follow.
4. **The donut, flat mode, breakdown mode, freeable gauge and diff
   all render numbers** — each needs a defined composition with an
   active filter (even if the answer is "unchanged, and here is
   why").
5. **Dumps are the interchange truth**: filtering must not silently
   produce partial dumps that later diff against full ones.
6. **CLI additions need `--help` + README in the same change**
   (CLAUDE.md); the palette needs cheatsheet/keymap entries likewise.
7. **Config parsing stays per-section resilient**; new sections
   (saved queries) inherit that.
8. **One size dialect** (`parse_size`), one duration dialect (to be
   chosen — fd's humantime family is the precedent), shared between
   the query language and every future CLI flag.
9. **Esc ladder discipline**: modal > palette > view mode > (filter?)
   > quit — every new rung must keep "where does this Esc go" fully
   predictable; `q`/Ctrl-C always quit.
10. **≤ 64-ish predicate vocabulary is a lie-free zone**: every
    shipped qualifier is a maintenance + honesty promise (age = mtime
    caveat, size = disk-vs-apparent choice, `user:` = retention
    change). The phase-1 set must be small and each member fully
    honest.

## 6. Open questions (for the options dossier to answer)

1. **Syntax family**: qualifier tokens, expression grammar, or a
   layered core (tokens desugaring to a tiny AST that a later grammar
   can also target)? What is reserved now (`|`, parens, `!`) so
   phase 2 needs no breaking change?
2. **Bare-term meaning**: glob? substring? fuzzy? (Everything:
   wildcard-ish; broot: fuzzy; flat view: glob with `*`/`?` only.)
3. **Phase-1 predicate set**: name/size/age/kind only? `ext:` sugar?
   `is:hardlink|error|excluded`? path globs now or phase 2? `user:`
   deferred until the uid side-array lands, or never phase 1?
4. **Size default**: does `>100M` test disk (the product default) or
   apparent? Is the `p` toggle respected or is the field explicit
   (`size:` vs `apparent:`)?
5. **Filter semantics**: re-aggregated browsable tree (the vision) —
   are non-matching *files* hidden entirely, and are ancestor dirs of
   matches always kept? What happens to empty-after-filter dirs?
6. **Composition**: filter × flat (`t`), × breakdown (`b`), × donut,
   × freeable gauge, × marking/deletion, × diff, × dump writing —
   each needs an answer or an explicit deferral.
7. **Live-during-scan**: post-scan only (fold needs the frozen
   arena), or launch-time `--filter` accumulated live by the owner
   (the only O(1)-compatible variant), or full live (rejected by
   constraint 2)? The flat-view precedent (live provisional chosen)
   pulls one way; the engine reality pulls the other.
8. **Engine**: re-fold per keystroke (debounced? how long?), on
   Enter only, or hybrid? Does rayon finally land (its reserved
   customer), and is the fold on the UI thread (1–2 dropped frames at
   200 k, unacceptable at 10 M) or async with a "filtering…"
   placeholder and latest-wins?
9. **Palette shape**: one Ctrl-K surface with sigil modes
   (query-first or command-first?), or split `/` filter + Ctrl-K
   commands (k9s school)? Is `/` bound at all?
10. **Error UX while typing**: never-error (Everything school:
    unparseable token = literal name match + hint), or hard errors
    with the last valid filter kept applied?
11. **History and saved queries**: persisted history (new XDG state
    dir) or in-memory? `[queries]` in camembert.toml read-only, or
    save-from-TUI (writes to config — precedent-setting)?
12. **CLI**: `--filter 'query'` in phase 1 (`--no-ui` summary +
    initial TUI filter)? Does `--filter` ever affect `-o` dumps
    (recommend: never — constraint 5)? `camembert diff --filter`
    deferred?
13. **Where does the parser live**: `camembert-core` (shared with
    future CLI/diff consumers) with the palette purely a frontend?
14. **Esc/clear ergonomics**: what clears an active filter, and where
    does it sit in the Esc ladder?
