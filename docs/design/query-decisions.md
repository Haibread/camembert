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
  filter. Esc ladder: palette > modal > mode > filter-clear > quit;
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
