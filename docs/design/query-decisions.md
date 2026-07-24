# Query language + palette — decisions (co-design session, 2026-07-23)

Outcome of the co-design session over the
[options dossier](query-options.md) and the three attack reports
([a](query-attack-a.md), [b](query-attack-b.md),
[c](query-attack-c.md)). Settled; reopening one requires a new
element. Covers HANDOFF next-step "Filter query language + Ctrl-K
palette".

## D1 — Shape: qualifier tokens (Option A), C's `/` graft, B as target

Phase 1 is **Option A amended**: live qualifier tokens
(`*.log >100M older:6mo`), implicit AND, per-term `!` negation, one
Ctrl-K palette (query-first, `>` sigil for commands) with `/` bound
to the same palette pre-scoped to filter mode (Option C's sole
surviving asset — no second surface, no split histories). Options B
and C are rejected as phase-1 designs per their attack verdicts; B's
full grammar (OR, parens) remains the *designed* phase-2 target,
reachable additively. Forward-compat sigils actually reserved by the
tokenizer: `(` `)` for grouping, `;` for value lists, `|` for pipes —
`<`/`>` are NOT reserved (already spent on size sugar; attack A
finding). Bare terms are smartcase substring matches. Literal
specials in filenames are expressible via double-quoted terms
(`"q(1).log"`), quoting rules documented with the tokenizer.

## D2 — Post-scan only

The filter engine runs on the frozen arena only. During a scan,
Ctrl-K/`/` show "filter available once the scan completes" (marks
pattern). Deliberately different from the flat view's live tier: the
filter predicate changes per keystroke and cannot be accumulated
incrementally on the owner; repeated global folds against a moving
tree would be wrong-by-construction and compete with the scan.

## D3 — Hardlinks: membership by any path

A file matches if **any** of its paths matches (canonical or extra);
its bytes count once, attributed to the canonical owner as
everywhere else. `*.bak` finds a 50 GiB `backup.bak` even when the
canonical link lives elsewhere (attack A's cardinal finding). Costs
a hardlink reverse map (extra path → canonical NodeId), built lazily
on first filter use.

## D4 — Semantics and composition

- Candidates are non-directory entries; the filtered tree re-derives
  every directory total over matching files only; dir inodes' own
  size excluded and the "matched vs scanned" residual is explained
  in the filter pill/footer, not just in docs (attack A).
- `t`/`b`/donut compose over the match set; breakdown groups are
  computed over the match set by the same filtered fold (defined —
  attack A found it unspecified). Freeable ledger untouched.
- **Directory marks are refused under an active filter** with an
  explicit message (the 42 MB-shown / 300 GB-deleted trap); file
  marks work. Group/bulk marking stays a separate fast-follow.

## D5 — Engine

Debounced (~100 ms) parallel fold over the frozen arena, **off the
UI thread** (the freeable sweep's spawn+channel idiom; attack A
killed the synchronous variant's "never blocks" claim), guarded by
(query, deletion-epoch) so stale results never render. rayon is
accepted as a dependency if the implementation shows it earns its
keep at 10 M nodes; a chunk-by-DirId sharded fold with per-thread
heaps is the sketched shape (the flat NameMemo is `&mut self` and is
NOT reusable as-is — build an immutable verdict table per query).
Root-name fix required first: the scan interns the full start path
as the root node's name, which breaks `dir/` ancestor tokens
(attack A; fix in the same change).

## D6 — Palette, keys, state

- Ctrl-K opens the palette (query-first; `>` prefix = commands
  generated from the keymap tables); `/` opens it pre-scoped to
  filter. Esc ladder: palette > modal > mode > filter-clear > quit
  (amended 2026-07-24 by user request: the last rung is now
  ascend-one-directory, like `Left` — Esc never quits, `q`/Ctrl-C do);
  while the palette is open, **all single-char global keys are
  suspended** (text-input mode — fixes the `q` kill).
- History: XDG state dir (`camembert/history`), bounded, written
  atomically (`.part`+rename). Saved queries: read-only `[queries]`
  table in camembert.toml.

## D7 — CLI and dumps

`--filter 'tokens'` applies to the `--no-ui` summary (post-scan
fold; `-o -` stdout gate inherited) and pre-applies the filter when
the TUI opens. `-o` dumps are **never** filtered. `diff --filter`
deferred. uid/gid predicates deferred (scan retention change —
reserved `user:`/`group:` qualifiers error with "not retained by
this scan" wording that names the future capability).

## Condensed reasoning trail

Condensed from the eight source documents (research, options dossier,
three option proposals, three attack reports) that led to D1–D7
above; the originals are no longer in the tree — see the closing
line.

### Research

**Inherited decisions binding this feature**: flat-view-decisions.md
drew the boundary — the query language owns full-path globs,
expressions, mtime/size predicates and breakdown drill-down; group
marking is a separate fast-follow with its own guard. flat-view D2's
"no whole-tree fold during the scan" binds: browse-during-scan works
because flat/breakdown use O(1) owner-side incremental accumulation on
a *fixed* predicate, not a fold — a typed filter is retyped, so it
cannot reuse that trick. scan-tree-decisions D1 reserved the frozen
post-scan arena for exactly this: "parallel filter/diff folds over the
post-scan frozen tree" (scan-tree option C §9 sketch: ~0.2s
single-threaded / <100ms on 8 cores @ 10M). tui-design.md reserves
Ctrl-K for this feature's palette. freeable-decisions D8: the freeable
ledger is scan-level process evidence, never touched by a filtered
tree. dump-format: dumps reserve an `ext` capability (uid/gid/mode,
currently `ext:false`); major-version bumps are near-taboo.

**What the code stores (grounding, verified against `main`)**: `Node`
is exactly 32 bytes, const-asserted. Stored and free to query:
basename (interned), kind, apparent+disk size, mtime, error/excluded/
hardlink-extra flags, parent chain (path/depth by walking).
**Not stored**: uid/gid, atime, ctime, nlink, mode, per-file ino/dev.
`DirMeta` (~80B) holds per-dir aggregates; `DirId` order is
topological (parent index < child index), so one forward pass
propagates inherited state and one backward pass sums subtree totals —
no recursion, no hashing. Names are interned, so a glob predicate is
evaluated once per unique name and memoized densely (`flat.rs`
`NameMemo`, 2B/name/kind) — the flat fold's own trick, directly
reusable for query name predicates (with the caveat, found by the
attack, that the reuse must be an eager immutable precompute, not the
live `&mut`-memoizing structure, if the fold is to run in parallel).

**Measured/grounded costs**: node arena @ 10M = 320MB; dir table ≈
80MB @ 1M dirs; a full streamed pass has a DRAM floor of 15–40ms @
10M. The live flat accumulator measures ~66ns/node (memo lookup +
counter + heap compare) — extrapolated to a full filter fold,
single-threaded: 5–15ms @ 200k, 0.3–0.7s @ 10M (agreeing with
scan-tree option C's independent estimate). Parallelized (rayon),
sequential dir passes bound the speedup: ≈50–120ms @ 10M. An alternate
dir-aggregate table costs 32B×dirs ≈ 32MB @ 1M dirs; a match bitvec
costs 1 bit/node ≈ 1.25MB @ 10M.

**Retention menu** (cost of making each field queryable): basename
glob, size, mtime, kind, flags, depth are all already on the node or
free to derive — zero cost. Full-path glob needs a real glob matcher
(`**`) plus dir-segment propagation during the coverage pass — small
CPU, O(dirs) not O(nodes). **uid/gid** is nearly free to *capture*
(the kernel fills `STATX_UID`/`STATX_GID` on the same syscall) but is
a cross-cutting change: worker → message → owner → a new interned-uid
side array (+20MB @ 10M as u16) → dump `ext:true` — correctly deferred
to phase 2 as a retention question, not a parser question. **atime**
is the worst of the menu: +80MB side array *and* the honesty problem
below — weakest value/cost ratio surveyed.

**Age semantics — the mtime-only stance and why atime was rejected**
(this reasoning is cited directly by the code and must survive):
the original vision wanted an age dimension ("big *and* cold"), but
atime is **not trustworthy** as a cross-filesystem signal. `relatime`
(Linux's default) updates atime at most once a day and only under
specific conditions; `noatime` never updates it; network and FUSE
filesystems do their own undocumented thing. camembert stores **only
mtime**, so every age predicate is mtime-age by construction, and the
honest phrasing is deliberately narrow: `older:6mo` means "**not
modified** in 6 months," never "not read" — a file read daily but
written once ranks as cold, and this is documented at the predicate
site, not just in a README footnote. mtime itself is not perfectly
honest either: `cp -p`/`rsync -a`/`tar -x` preserve source mtimes (a
file copied yesterday can read as ten years old), moves keep mtime,
and a directory's own mtime reflects only direct-child churn — which
is why age predicates match *files* and re-aggregate rather than
testing directory mtimes directly. Reintroducing atime later is not
just the side-array cost above but a **per-mount trust problem**:
honest use would require parsing `/proc/self/mounts` to detect
`noatime`/`relatime` and badging the result accordingly. The research
found no disk-usage analyzer that does this honestly — qdirstat, gdu,
dust and the rest all use mtime for age too — so deferring atime
indefinitely is squarely in line with the field, not a shortcut unique
to camembert.

**Prior art, four syntax families surveyed**: (1) flag-predicates (fd,
dust, gdu) — one flag per predicate, composable in shells, useless in
a single interactive line; (2) qualifier tokens (Everything, GitHub,
Gmail) — whitespace = AND, `field:value` qualifiers with comparison
sugar, bare terms match names, built for search-as-you-type; (3) SQL/
expression DSLs (fselect, osquery, jq, PromQL) — full typed grammars,
precise but unparseable mid-keystroke; (4) fuzzy + operators (fzf,
telescope, broot) — fuzzy by default with a small operator vocabulary,
instant but weak for numeric/date predicates. **Everything** (family
2's gold standard) update per keystroke over millions of entries and
—the property that makes it learnable—**no input is ever an error**:
an incomplete `size:>` just matches nothing until it parses. GitHub/
Gmail confirm the family (`size:>100`, `larger:5M`, `-qualifier:value`
negation) and that users learn qualifiers lazily, one at a time. fd/
fselect/osquery mark the two poles: fd's `--changed-within 2weeks` /
absolute-date duo is worth copying; fselect/osquery's SQL is loved by
people who already think in SQL and is never discussed as a per-
keystroke tool. Glob dialects: gitignore-style (`*`, `**`, trailing
`/`) is the most widely known path pattern; camembert's flat view
already speaks a basename-only subset of it. TUI precedents: **broot**
is the closest existing "filter re-aggregates a tree" analog, with a
mode-sigil grammar (`n/`, `/regex/`, composable `!`/`&`/`|`/parens) and
a load-bearing lesson — echo the fully resolved command before
execution; **k9s** splits `:` command mode from `/` filter mode
cleanly; **fzf**'s extended syntax and debounced `reload` idiom (~50–
150ms) is the direct precedent for a debounced live fold; **Textual**'s
palette contract (`search()` per keystroke, `discover()` on empty
input) is the clean architectural split later reused. **Command
palettes** (VS Code/Slack/Linear/Raycast) converge on: one overlay,
fuzzy match, recents on empty input never a blank box, sigil-switched
modes in one input (`>` for commands), validation deferred to submit
with the specific bad token flagged, and a two-tier history (rolling
recents + starred/saved).

**Constraints assembled** (binding on every option): the 32-byte node
stays untouchable; the UI has no arena access during the scan, so any
live-filter story is launch-time-only or violates the no-fold-during-
scan identity; filtered totals must never let a user delete more than
the screen shows (the "42MB shown, 300GB deleted" trap); every number-
rendering surface (donut, flat, breakdown, freeable gauge, diff) needs
a defined composition with an active filter; dumps must never be
silently partial; CLI additions need `--help` + README in the same
change; config parsing stays per-section resilient; one size dialect
(`parse_size`) and one duration dialect shared everywhere; the Esc
ladder must stay fully predictable at every new rung; and the phase-1
predicate vocabulary must be small and every member fully honest (no
qualifier shipped without its cost and its caveat stated).

### Options

**A — live qualifier tokens, one palette.** Core idea: Ctrl-K/`/` open
one query-first input; whitespace-AND qualifier tokens (`*.log >100M
older:6mo`) re-aggregate on a ~100ms debounced fold, never erroring
mid-typing, with a status-line parse echo. Decisive pros: matches the
only syntax family with evidence of both expressive predicates and
per-keystroke fitness; bare terms work untaught; reserved-but-rejected
sigils (`|`, grouping, `;`, path globs) make phase 2 purely additive;
engine is the reserved rayon-over-frozen-arena customer. Decisive con
(closed by the attack, see below): "instant/never blocks" oversold the
synchronous fold at 10M scale, and the `NameMemo` reuse claim as
written doesn't compile under parallel access. **Won**: reaches the
same common queries live, degrades gracefully, and its designed
phase-2 upgrade path is exactly B's grammar without any breaking
change.

**B — expression grammar, apply on Enter.** Core idea: full `and`/
`or`/`not`/parens grammar with typed comparisons, applied only on
Enter, with real span-scoped teaching errors — the "strongest CLI
story," same grammar in TUI and scripts from day one. Decisive cons:
its entire phase-1 delta over A is disjunction/grouping (everything
else A already expresses), which a `;` value-list sugar covers without
a grammar at all; the grammar is maximal and irreversible on day one
(a saved query can never be un-shipped) with zero field evidence
anyone wants boolean composition; the BNF as written doesn't even pin
operator precedence, so the very first saved query freezes an
accidental reading; bare `and`/`or`/`not` keywords collide with real
filenames (`Rock and Roll.flac`) and parens collide with common
filename punctuation, with no escaping story specified; it kills the
live-cockpit feel that is the product's identity. **Lost** outright as
a phase-1 design (attack verdict: KILL), but its grammar remains the
*designed* phase-2 target — reachable additively once A ships, if
field evidence ever justifies it. Letter "B" is not shipped now but is
the named future shape.

**C — split surfaces, `/` filter + Ctrl-K commands.** Core idea: give
filtering and commanding different keys (k9s school) — `/` is an
always-visible inline filter prompt (nothing covers the tree), Ctrl-K
is a *pure* command palette with no query grammar inside it, ever.
Decisive cons: the k9s `/` analogy is a false equivalence (k9s's `/`
is a cheap, local, display-only grep; camembert's `/` triggers an
expensive whole-tree fold that silently rewrites every number in the
cockpit — borrowing the precedent's discoverability while hiding its
very different consequences); a *pure* command palette dead-ends the
single most-trained palette reflex (typing free text to find a thing —
Ctrl-K in C returns nothing for `node_modules`); once its one good
idea (`/` as a shortcut) is grafted onto A, nothing standalone is left
— it reduces to "A minus the palette's free-text fallback, plus a
duplicated input surface (two line-editors, two histories, two Esc
semantics) with nowhere to live in the fixed six-region layout."
**Lost** as a standalone option (attack verdict: KILL), but its sole
surviving asset — `/` as a direct shortcut into A's palette, pre-
scoped to query mode — is grafted into the winning design (D1).

**Recommendation** (adopted, becomes D1): Option A, amended with C's
`/` graft and B's grammar reserved as the designed-not-built phase-2
target — the same shape as the scan-tree co-design's own precedent
(Option A + a graft from a losing option).

### Attack findings

**Report A — live qualifier tokens** (verdict: SURVIVABLE WITH
AMENDMENTS; engine skeleton sound, three "honest" claims false as
written):

1. A name filter silently reports 0 bytes for an extant file: an
   extra hardlink link (`HARDLINK_EXTRA`) contributes 0 and was
   specified to *never match*, so `*.bak` could show "0 matches" while
   a 50GiB `backup.bak` sits on disk under its canonical name.
   Resolved: an extra link must still *match* its own name (0 bytes,
   flagged "counted under `<canonical path>`") — folded into D3's
   "membership by any path" rule.
2. `q` always quits, so the palette — a text input — could never type
   the letter q (`.qcow2`, `query`, `requirements.txt`). Resolved:
   suspend all single-char global keys, `q` included, while the
   palette has focus (Ctrl-C only) — this is D6's "text-input mode"
   fix.
3. Two "reserved for phase 2" sigils (`<`/`>` grouping) were already
   spent on live size sugar (`>100M`, `<1G`) in the same section —
   an internal self-collision, not just abstract debt. Resolved: `<`/
   `>` are explicitly NOT reserved (already spent); grouping uses `(`
   `)` instead — carried into D1.
4. "Never blocks a keypress" is false at 10M scale: the debounced fold
   was specified to run synchronously on the UI thread, a 50–120ms
   freeze with no frame drawn. Resolved: the engine must run off the
   UI thread (D5, the freeable sweep's spawn+channel idiom), not
   synchronously — the one design property that does survive (a
   blocking fold can't render a stale generation) must be replaced by
   an explicit arrival-time generation check if async is ever built.
5. The claimed reuse of `NameMemo` for the parallel fold pass doesn't
   compile: `NameMemo` memoizes lazily on `&mut self`, a data race
   under shared rayon access. Resolved (folded into D5): build a
   query-scoped *immutable* verdict table precomputed sequentially
   before the parallel pass; chunk by `DirId` not by node; use
   per-thread top-N heaps merged, not a shared heap.
6. Breakdown (`b`) under an active filter had no defined numbers — the
   specified fold computes no pattern-group buckets at all, so
   "groups over the match set" was either an unspecified third
   computation or silently unfiltered groups under a filtered header.
   Resolved (D4): breakdown groups are computed over the match set by
   the same filtered fold — explicitly defined, not left silent.
7. The dir-inode exclusion (correct in principle) was stated once but
   shown continuously as an unexplained ~4GiB gap between "matched"
   and "of X scanned," even at match-all. Resolved (D4): the residual
   is explained in the filter pill/footer itself, not only in docs.
8. Bare `>`/`<` size sugar and `word:` qualifiers make literal
   filenames like `>readme` or `kind:notes` unreachable by substring,
   with no quoting story. Resolved: guard bare `>`/`<` behind a
   following digit and reserve a literal-substring escape
   (double-quoted terms per D1's quoting rules).
9. Filtering makes finding matches trivial but bulk-acting on them
   tedious (no group marking in phase 1), and marks persist across
   filter changes so hidden marked items can still be deleted.
   Resolved (D4): directory marks are refused under an active filter
   with an explicit message; file marks work; group/bulk marking
   stays a named separate fast-follow rather than being smuggled in.
10. Applying a filter while standing inside a directory that becomes
    zero-match was undefined (disappear? bounce to an ancestor?).
    Resolved: the viewed directory always renders (empty table +
    hint), navigation never happens automatically — folded into D4's
    semantics.
11. Ancestor `dir/` tokens can't target the scan root, because the
    scanner interns the *entire start path* as the root node's name,
    breaking the "basenames contain no `/`" assumption for that one
    node. Resolved (D5): fix the root-name interning before/alongside
    the query engine ships, not deferred.
12. The filter pill's "Esc clears" claim is only true from tree view
    (from flat/breakdown, Esc first leaves the mode) — off by one
    press. Resolved: scope the pill's wording to its real behavior
    (rejected as stated; wording fix only, not a semantic change).
13. The history file's location was specified but write-safety was
    thin: unset-`XDG_STATE_HOME` fallback, concurrent-instance
    clobbering, and directory creation were unstated. Resolved (D6):
    history is written atomically (`.part`+rename) to the bounded XDG
    state file, addressing the write-safety gap directly.
14. Unstated whether `--no-ui --filter` filters the top-directories
    list or only top-files. Resolved: left as a documentation
    completeness item for the implementation to state in `--help`,
    not a semantic dispute — no dedicated D-item, folded into D7's
    general "document in the same change" expectation.

**Report B — expression grammar** (verdict: KILL as the phase-1
design; strategically dominated by A):

1. The entire phase-1 value B buys over A is disjunction/grouping —
   every other predicate A already expresses, and the dominant
   cleanup use case (several extensions) is covered by a `;`
   value-list sugar without any boolean grammar. Rejected: B's
   marginal gain doesn't justify its cost; folded into D1's rationale
   for choosing A.
2. Shipping the maximal grammar first makes evolution strictly worse
   than A's purely-additive path: B can never simplify or retreat from
   a form once a query is saved, with zero field evidence anyone wants
   it. Rejected: this asymmetry is the core reason B is deferred
   rather than built now (D1).
3. The BNF as specified doesn't pin operator precedence or
   associativity, so the first saved query would freeze an
   accidental, unstated reading forever. Rejected as specified; not
   applicable to A (implicit-AND has no precedence to get wrong).
4. Apply-on-Enter is a dead island in an otherwise fully live cockpit
   (live scan, live flat accumulator, animated donut) — no visual
   response while typing a 30-character expression. Rejected: conflicts
   with the product's live-cockpit identity that D1/D5 preserve via
   A's debounced fold.
5. Bare `and`/`or`/`not` keywords and `(`/`)` collide with ordinary
   filenames (`Rock and Roll.flac`, `report (final).log`) with no
   escaping grammar specified, and filenames are raw bytes (non-UTF8)
   which a naive quoted-string lexer can't represent. Rejected: this
   collision surface is strictly larger than A's (whitespace + leading
   sigils only); A's quoting (D1) sidesteps it.
6. The engine section contradicts itself at 10M: "single-threaded on
   the UI thread" behind a "placeholder" that the freeable idiom it
   cites is actually off-thread — you cannot have both. Rejected: the
   same async-vs-blocking choice A faces was not honestly resolved
   here either.
7. B's on-Enter error dismissal adds a third, confusable Esc meaning
   that only B has, on top of A's two new rungs — violating the
   "Esc must stay fully predictable" constraint. Rejected: A's
   never-error model has no error to dismiss, keeping its ladder one
   meaning shorter (D6).
8. Hardlink canonical attribution under a partial filter can silently
   show a real multi-GB file as a 0-byte match — sharper here because
   B's pitch promises exactness. Resolved the same way as report A
   finding 1: the counted link's group holds the bytes; a matched
   extra must be visibly badged, never a silent 0-byte row (D3).
9. `mtime < -6mo` is the least learnable token surveyed (three
   concepts at once: time comparison, negative duration, "older"
   direction), with B's own worked example misquoting its own syntax.
   Rejected: A's `older:6mo` reads as English with no such cliff.
10. Spaced `<`/`>` operators are a shell-redirect footgun
    (`--filter size > 100M` unquoted silently truncates a file and
    passes a wrong filter) undermining the "strongest CLI story"
    claim. Rejected: A's attached-sigil sugar (`>100M`) is safer and
    the CLI expressiveness gap was mostly OR, already covered by
    value-lists.
11. `not` sharpens the marking-honesty gap between "the number shown"
    and "what deleting frees" beyond A's positive-only common case.
    Rejected: extra rope B hands the user that A's D4 dir-mark refusal
    doesn't need to reason about.
12. B's documentation cost is a full language reference (precedence,
    quoting, escaping), the largest doc surface of the three options.
    Rejected: weighed against A's simple qualifier table in choosing
    A for phase 1 (D1, D7).

**Report C — split surfaces** (verdict: KILL as a standalone option;
its one good idea grafts onto A):

1. The k9s `/`-as-filter analogy is a false equivalence: k9s's `/` is
   a cheap, local, reversible grep over visible rows; camembert's `/`
   triggers an expensive whole-tree fold that silently rewrites every
   cockpit number — the "tree stays visible" framing dresses the
   42MB/300GB honesty trap as a lightweight, familiar narrowing.
   Rejected: the borrowed precedent misleads about the key's actual
   consequences; not carried into D1 beyond the bare `/`-as-shortcut
   idea.
2. A *pure* command palette (no free-text filtering) dead-ends the
   single most-trained palette reflex — typing a target name — since
   Ctrl-K in C only fuzzy-matches command *names*, returning nothing
   for e.g. `node_modules`. Rejected: A's query-first single input
   (D1) is the convention users actually have.
3. Once C's one distinct asset (`/` as a filter key) is grafted onto
   A, nothing standalone remains — engine, semantics, CLI, and marking
   rules are all declared identical to A, leaving only a *negative*
   differentiator (a palette deliberately worse than A's). Rejected:
   C dissolves into "A plus a `/` alias," which is exactly what D1
   adopts, without the "worse palette" part.
4. The split requires building two embedded text-input surfaces
   (line editor + cursor, history, Esc semantics, footer hints) where
   today the UI has no text-capture mode at all — "two small halves"
   is actually two full copies of new machinery. Rejected: D1's one
   palette avoids the duplication entirely.
5. The fixed six-region vertical TUI layout has no spare row for a
   persistent `/` prompt without permanently shrinking the table (and
   fighting zen mode, which hides exactly the numbers a live filter
   changes). Rejected: D1's overlay palette taxes the tree only
   transiently, returning every row on Esc.
6. The split adds two Esc rungs of different *kinds* (a stateless
   palette-close and a stateful `/`-prompt rollback), one more than A
   needs for the same functionality. Rejected: D1 keeps a single
   input surface's Esc semantics.
7. Persisting history for `/` but not for Ctrl-K commands creates an
   unexplained asymmetry a single-palette design doesn't have.
   Rejected: D6's one history store avoids the wart.
8. Cross-cutting traps (filtered totals × hardlinks, marks under a
   filter, live-during-scan inconsistency) are inherited by the split
   with no improvement over A. Noted as shared, not a C-specific
   defect; resolved the same way for both (D3, D4, D2).

Originals for all eight source documents are recoverable from git
history.
