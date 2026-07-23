# Adversarial review — Option A (live qualifier tokens + Ctrl-K palette)

> Verdict: **SURVIVABLE WITH AMENDMENTS** — the engine skeleton is sound
> (rayon over a frozen `&Tree` is genuinely safe, uid/gid really is absent,
> the epoch/invalidation pattern is real), but three claims the doc leans on
> are false as written: the filter silently reports **zero bytes for files
> that exist** (hardlink extra-link case), the "never blocks a keypress"
> pitch is untrue at 10 M under the phase-1 synchronous fold, and two of the
> "reserved for phase 2" sigils are **already spent** on live syntax. Plus a
> `q`-always-quits rule that makes the palette unable to type the letter q.

## The through-line flaw

A's identity claim is *instant + honest*. The engine reality it ships in
phase 1 is *debounced-then-blocking + honest-modulo-three-silent-gaps*. Every
serious finding below is one of those two words not being true where the doc
says it is:

- **"Instant / never blocking a keypress"** (§1) is contradicted by A's own
  §4: the fold is *synchronous on the UI thread*. At 10 M entries that is a
  50–120 ms freeze during which no keypress is processed and no frame is
  drawn. §1 sells the async feel; §4 ships the blocking one; §9.1 half-admits
  it. The pitch oversells the number's liveness exactly the way it warns
  against overselling numbers.
- **"Honest"** — the filtered view has three silent ways to print a number
  the user cannot stand behind: a name filter that names only a
  non-canonical hardlink reports **0** for a file that occupies space; a
  match-all filter shows a persistent unexplained GB-scale gap against the
  "of X scanned" subtitle; and breakdown-under-filter has no defined group
  numbers at all. None are red states — they are quiet wrong-but-plausible
  numbers, the freeable-attack sin transplanted into the query surface.

Neither is fatal — the architecture is right and every gap has a cheap patch
— but the doc must stop asserting the two adjectives where the code doesn't
earn them.

## Findings (severity-ranked)

### SERIOUS (honesty) — a name filter silently reports 0 for files that exist [1]

§2.2, verbatim: "`HARDLINK_EXTRA` entries contribute 0 and **never match**."
The doc states the mechanism and stops. The consequence it doesn't state:

`finalize_hardlinks` (scan end) moves each inode's *counted* link to its
canonical (smallest-path) link; every other link becomes `HARDLINK_EXTRA`
(flat.rs module doc; tree.rs `move_hardlink_first`/`set_hardlink_extra`). So
which *name* carries a hardlinked file's bytes is an internal
smallest-path tiebreak the user never sees. Now filter by name:

```
filter: *.bak
```

`backup.bak` is a 50 GiB hardlink whose canonical link is `data.db` (shorter
path, different basename). `backup.bak` is the `HARDLINK_EXTRA` link →
contributes 0 **and never matches**. The filtered view shows `*.bak`:
**0 matches, 0 bytes** — while a 50 GiB file named `backup.bak` sits on disk.
The user concludes "no .bak files are eating space" and moves on. The one
tool that promised honest numbers just hid a 50 GiB file behind a query that
literally names it.

Worse than a zero row: "never match" means it isn't even shown as a
0-byte `backup.bak` line the user could puzzle over. The visibility of a
physical inode under a *name* predicate depends on a canonicalization the UI
never exposes.

Amendment (mandatory): an extra link must still **match** its own name; it
contributes 0 to filtered *bytes* but appears as a row flagged
`⛓ counted under <canonical path>` (the canonical link is one
`move_hardlink_first`/registry lookup away). Then the number stays 0 but the
*presence* is honest, and the pill/help states "hardlinked bytes are counted
once, under the shortest path — filter by that name to see the bytes." A
filter that can name a file and report it absent is the exact
incoherent-number failure A's thesis forbids.

### SERIOUS — `q` always quits, so the palette cannot type the letter q [2]

§3.2: "`q`/Ctrl-C still always quit." In `handle_key` that is literally the
first arm (`ui.rs:766`, `KeyCode::Char('q') => Action::Quit`). The palette is
a **text input**. If `q` always quits, the user cannot type `*.sqlite`,
`query`, `requirements.txt`, `.qcow2`, or any term containing `q` — the first
`q` keystroke kills the program mid-query. The existing non-text modals
(cheatsheet, freeable) get away with a blanket-key rule because they consume
*no* text; a query box is a different animal and the doc treats it like the
others.

Amendment: while the palette input has focus, **only Ctrl-C quits**; every
printable key (q included) is a character. `q`-always-quits resumes the
instant the palette closes. This must be stated — it is the one place A's
"one keystroke away, q always quits" reflex is actively wrong.

### SERIOUS — two "reserved for phase 2" sigils are already spent on live syntax [3]

§2.1 ships bare size sugar `>100M` **and** `<1G` as phase-1 features, and in
the same section reserves "`<`/`>` grouping" for phase 2 "so phase 2 needs no
breaking change." Those are the same two bytes. `<` and `>` are load-bearing
size-comparison prefixes the day A ships; they **cannot** later become
grouping delimiters without either ambiguity (`<1G` — a group open, or
"under 1 GiB"?) or a breaking change. Everything (research §4.2) avoids this
precisely by keeping size in the *qualifier* form (`size:>1mb`) and reserving
bare `<...>` for grouping; A took the bare-sigil size sugar *and* tried to
reserve the same sigils. It can keep at most one.

§9.4 frets about "reserved-token debt" in the abstract but never notices this
concrete self-collision. This is not a hint-text nicety — it decides whether
the phase-1 grammar is forward-compatible at all.

Amendment: pick one. Either (a) size stays qualifier-only (`size:>100M`,
`size:<1G`) and bare `<`/`>` are genuinely reserved for grouping; or (b) bare
`>100M`/`<1G` stay and phase-2 grouping uses a sigil that *isn't* spent
(Everything uses `<...>`; broot uses `(...)`) — in which case strike "`<`/`>`
grouping" from the reserved list and say grouping will be `(...)` or a
keyword. The reservation list must be internally consistent with the live
grammar in the very same section.

### SERIOUS — "never blocks a keypress" is false at 10 M; nothing renders mid-fold [4]

§1: "re-aggregate under your fingers … never blocking a keypress." §4: the
debounced fold runs "**synchronously on the UI thread** … worst case 2–4
dropped frames." Both cannot hold. The debounce (100 ms after the last
keystroke) does correctly avoid a per-keystroke fold — but when it fires at
10 M it seizes the UI thread for 50–120 ms (the doc's own budget): no
keypress is processed, no "computing…" frame is drawn (the thread that would
draw it is folding), the last frame simply freezes and then the result snaps
in. On a throttled/loaded box (§9.1) it is a visible multi-hundred-ms hitch
with zero feedback.

Note the one thing that *does* survive here: because the fold blocks the
single thread, a **stale generation can never render after a newer
keystroke** — the race the prompt worried about is closed *by construction*
in the synchronous design. That guarantee is real and worth stating. But it
is exactly the guarantee the async escape hatch (§4, "not built until
measured") would *lose*: a worker-thread fold for query N can land after the
user has typed query N+1, and the doc's generation key (canonicalized query,
epoch) only saves you if it is **checked on arrival and discarded on
mismatch** — which the escape hatch, being ungesigned, doesn't specify.

Amendment: (a) drop "never blocking a keypress" from §1 or scope it to
"≤ 200 k (2–5 ms)"; at 10 M say plainly "a visible pause at the typing
pause." (b) If the fold stays synchronous, spend one frame drawing a
`filtering…` pill *before* the blocking call so there is feedback, or accept
the freeze and say so. (c) When the async hatch is built, spec the
arrival-time generation check — "latest-wins, stale folds discarded" — as
part of it, or it reintroduces the stale-render race the sync design avoided.

### SERIOUS — the rayon fold cannot "reuse `NameMemo`"; the parallel pass needs a different memo [5]

§4 pass 2 is "arena chunks folded by rayon workers … name verdicts memoized
per interned name (`NameMemo` reused, research §2.2)." The `NameMemo` it
points at (flat.rs:301) memoizes **lazily on `&mut self`**: `lookup(&mut
self, …)` writes the `MEMO_UNCOMPUTED` slot on first sight of a name. Shared
across rayon threads that is a data race on `Vec<u16>` — it does not compile
under a shared `&`, and forcing it (per-slot atomics / a lock) throws away
the "plain dense index, no hashing" property that made it cheap.

The parallel-safe shape is a *different* structure: evaluate every unique
name once, **sequentially, before** the parallel pass, into an immutable
`Vec<verdict>` the workers only read (`unique_names ≤ node_count` globs, tens
of ms at worst, trivially cacheable per query). That is clean and correct —
but it is an eager precompute, not the lazy `NameMemo` "reused." The doc
should say "a query-scoped immutable verdict table, precomputed from the
interner (the memo's dense representation, not its lazy `&mut` fill)."

Two more parallel-shape corrections while here, both cheap:

- The top-N over the match set (§3.3) needs **per-thread heaps merged**, not
  a shared `TopHeap` (`offer(&mut self, …)`, flat.rs:486).
- Chunk **by `DirId`, not by node**. Child runs are contiguous per dir
  (tree.rs `ChildRun`), so a `par_iter` over dirs lets each dir fold its own
  direct children into its own alternate-aggregate slot and its own bitvec
  segment with **zero** cross-thread contention on the per-dir table. The
  doc's "arena chunks … partial per-dir buckets, merged" is the
  chunk-by-node variant, which forces an N-thread × dirs partial-bucket merge
  (8 × 32 MB ≈ 256 MB transient @ 1 M dirs) the by-dir shape avoids entirely.

None of this is fatal — it *strengthens* the "rayon finally earns its keep"
claim — but "reuse `NameMemo`" as written is not implementable.

### SERIOUS — breakdown (`b`) under an active filter has no defined numbers [6]

§3.3: "`t` flat / `b` breakdown: compose — top-N over the match set, groups
over the match set (one shared fold pass)." But the fold §4 specifies
produces "alternate dir aggregates + match bitvec + totals" — it computes
**no pattern-group buckets**. The pattern partition lives in a *different*
engine (flat.rs `fold`, disjoint `node_modules/`/`*.log`/… groups), driven by
`PatternSet`, not by the query. So "groups over the match set" is either (a)
a third computation nobody specified — the flat.rs disjoint-partition fold
re-run restricted to the bitvec, with its own coverage pass and memo — or (b)
silently the *unfiltered* groups sitting under a filtered header, which is
finding [1]'s dishonesty again.

Concretely: filter `older:1y`, press `b`. What does the `node_modules` row
show — all node_modules bytes, or only those older than a year? The doc
promises the latter and ships an engine that computes neither.

Amendment: either spec the filtered pattern-fold explicitly (the flat.rs fold
gains an `Option<&FilterGeneration>` mask exactly as `build_snapshot` does in
§4, skipping non-matching nodes), or defer `b`-under-filter to phase 2 and
have `b` **clear or ignore** the filter with a one-line flash. Silent
unfiltered groups under a filtered cockpit is not an option.

### SERIOUS — the dir-inode gap is stated once but shown continuously [7]

§2.2 is right that directory inodes' own bytes must be excluded from the
match set (otherwise totals depend on which dirs are retained — a real
wrong-but-plausible number, ~4 GiB / 1 M dirs by the doc's own estimate). The
problem is the *display*: the pill and cards show "1.2 GiB matched · **of
120 GiB scanned**" (§3.2), where 120 GiB is the real tree aggregate
(`DirMeta.td`, which *includes* every dir inode). So even a **match-all**
filter shows "≈ 116 GiB matched · of 120 GiB scanned" — a persistent ~4 GiB
gap with no matching entry to attribute it to. The invariant test (§7) bakes
this in: empty query ⇒ filtered == aggregate **minus dir own-inodes**. Stated
once in a help line; shown on every filtered frame.

This is the mirror of the freeable attack's [1]/[2]: a headline number
sitting next to a second number it is not a clean fraction of. A user who
filters broadly and eyeballs "matched vs scanned" sees an unexplained
residual and — correctly — distrusts it.

Amendment: make the subtitle's denominator honest to the match model.
Either subtract dir-inode bytes from the "scanned" figure shown *next to a
filter* ("of 116 GiB filterable"), or add a third term ("+3.9 GiB directory
overhead, never matched"). One consistent decomposition, not a headline minus
a silent constant.

### ANNOYING — no escape for literal leading sigils or `field:` colons [8]

Bare terms are substring matches, but a term that *starts* with `>`/`<` is
size sugar and a term shaped `word:value` with a known `word` is a qualifier.
Unix basenames legally contain `>`, `<`, `:`, `!`, `;`, `|`. So:

- a file named `>readme` or `<draft>`: typing it parses as (inert) size
  sugar; it is unreachable by substring;
- a file named `kind:notes` or `ext:backup`: parses as the `kind:`/`ext:`
  qualifier, never as the literal substring;
- `!` and `;` and `|` mid-name similarly collide with negation / reserved
  tokens.

There is no quoting/escaping in the grammar. Everything and fzf both provide
`"exact"` / `'literal` affordances precisely for this. Phase 1 can live
without full quoting, but it must (a) only treat `>`/`<` as size when
*immediately followed by a digit* (so `>readme` is a literal substring,
`>100M` is size), and (b) name a reserved `'`-prefix or `name:` qualifier for
"literal basename substring" in the reserved list, so the escape hatch exists
before someone needs it. As written, a nonzero slice of real filenames is
simply unqueryable.

### ANNOYING — the filter is see-but-can't-bulk-act; and marks hide under it [9]

Good news the prompt doubted: the dir-mark refusal message **is** spec'd
(§3.3, "this directory shows only matching files — marking it would delete
everything in it; mark files, or clear the filter"). That closes the honesty
trap. But it opens a workflow cliff:

- Under a filter, a directory row is refused (`toggle_mark`, state.rs:683,
  gains a filter-active branch beside the existing mount-point one). Group
  marking ("mark the matches") is deferred to phase 2. So to delete 10 000
  matching files the user must `Space` each one individually — the filter
  makes *finding* them trivial and *acting* on them tedious. The feature's
  headline use case ("filter `older:1y`, delete them") is exactly the one
  phase 1 refuses to make ergonomic.
- Marks are `NodeId`s in a shared basket that **persists across filter
  changes** (state.rs `marks`/`marked_set`). Mark three files under filter A,
  switch to filter B where they don't match → they vanish from the table but
  stay in the basket and in the confirm/`D` count. Clear the filter and
  they're still marked. `v` (review) shows them by path so it's recoverable,
  but "marked things I can't see get deleted" deserves a line.

Amendment: state the see-but-can't-bulk-act limitation as the headline caveat
of phase-1 filtering (not buried), and either freeze the basket while a
filter is active or flash "N marks hidden by the filter" when a filter change
strands marks.

### ANNOYING — applying a filter while viewing a now-empty subtree is undefined [10]

Filtered semantics (§2.2): "dirs with zero matching descendants disappear."
But the *viewed* directory can be one of them. You are at `/a/b/c`, you apply
`*.mp4`, `c` contains none → `c` has zero matching descendants. Do you see an
empty table? Get bounced to the nearest matching ancestor? The doc's row
model says `c` "disappears," but you are standing inside it. Nothing in §3 or
the Esc ladder defines this. (The reverse — navigating *into* a
zero-match dir that's still shown as a spine to matches below it — also needs
a rule.)

Amendment: define it. Simplest honest behavior: the viewed dir always renders
even at zero matches (empty table + "no matches here · Esc clears filter"),
and never auto-navigates — navigation is the user's, the filter only reshapes
counts and hides *child* rows.

### ANNOYING — ancestor `dir/` tokens cannot target the scan root [11]

§2.2's `dir/` token globs ancestor directory *names*. Verified against the
code: the root node's name is the **entire start path** as bytes
(`scan.rs:316`, `path.as_os_str().as_encoded_bytes()` → `push_root_node`). So
scanning `/home/theo/projects` interns a root name of `/home/theo/projects`
— slashes and all. A basename glob `projects/` (stripped to `projects`) will
never match `/home/theo/projects`, and the flat matcher's "basenames contain
no `/`" assumption (flat.rs `glob_match` doc) is violated for the one node
that is everyone's ancestor. So `dir/`-ancestor tokens silently can't
reference the root by any intuitive name; scanning `.` makes the root name
`.`.

Amendment: either exclude the root from ancestor-token matching by contract
(document: "ancestor tokens match scanned subdirectories, not the scan root")
or special-case the root's display name to its final component for matching.
Cheap; just needs stating so it isn't rediscovered as a "why doesn't
`projects/` work" bug.

### COSMETIC — the pill's "Esc clears" is true only from tree mode [12]

§3.2 renders "… Esc clears" in the pill, and the Esc ladder is "modal >
palette > view mode > active filter > quit." From `FlatTop`/`Breakdown` with
a filter active, the first Esc **leaves the mode** (ui.rs:771–776 already does
mode-before-quit; the filter rung slots below it), so Esc does *not* clear the
filter — the pill's promise is off by one press. The one-level insert itself
is clean and matches the existing contextual-Esc shape. Just make the pill
honest: "Esc clears (from tree view)" or clear-filter-before-leave-mode, and
pick one deliberately.

### COSMETIC — history file: location is specified, write-safety is thin [13]

Contra the prompt's worry, §6 *does* name the location
(`$XDG_STATE_HOME/camembert/history`) — and it is the **first writable
surface** in a tool whose config is deliberately read-only (verified:
config.rs only ever *reads* `XDG_CONFIG_HOME`; no state dir exists). What's
thin: the `XDG_STATE_HOME`-unset fallback (`~/.local/state`) isn't named;
concurrent camembert instances clobber the file last-writer-wins (non-fatal
per "absent/broken = empty," but lossy); dir creation (`mkdir -p`) is
implied, not stated; and persisting searched **filenames/paths** to plaintext
is a mild privacy surface worth one sentence. All minor, all one-liners.

### COSMETIC — `--no-ui --filter` summary: which sections filter? [14]

§5 says `--no-ui` "shows filtered totals + top matching **files**." The
current summary prints top *directories* **and** top *files* (main.rs:747,
765). Does `--filter` filter the top-dirs list too, or only files? Unstated.
Credit where due: the `-o -` hazard the freeable attack caught ([5] there) is
**already handled** here — the entire summary branch is gated behind
`!dump_to_stdout` (main.rs:718–781), and the flat fold sits *inside* that
`else`, so a filtered summary line inherits the gate for free and can never
corrupt a dump stream. Just say whether top-dirs is filtered.

## What survived the attack (genuinely)

- **rayon over the frozen `&Tree` is sound — the single-owner invariant is
  untouched.** Verified: `Tree::node`/`dir`/`children` are all `&self`
  (tree.rs:370–409); every mutator is `pub(crate)` and the D1 single-writer
  rule is about *scan-time mutation*. Post-scan the arena is frozen and
  read-only; N threads holding `&Tree` is plain shared-read, not a violation.
  The doc's "parallel-folded over the frozen arena" is the correct framing —
  the invariant it might seem to threaten simply doesn't apply post-scan.
- **The uid/gid absence claim is TRUE, verified at the syscall.** The statx
  mask is `TYPE | SIZE | BLOCKS | MTIME | NLINK | INO` (worker.rs:412–417) —
  no `STATX_UID`/`STATX_GID`, and the `fstatat` fallback reads no owner
  either. So `user:`/`group:` really is a cross-cutting *retention* change
  (worker → message → owner → side array → dump `ext`), correctly deferred to
  phase 2, not a parser question. Research §2.4 is accurate.
- **The epoch/invalidation reuse is real.** `FlatSummary.epoch` +
  render-time recompute on mismatch (state.rs `flat_epoch`/`bump_flat_epoch`,
  set_flat_summary) is exactly the pattern §4 keys the `FilterGeneration` on;
  a deletion under a filter recomputing on the next render is a proven shape,
  not a hope.
- **`build_snapshot` gaining an `Option<&FilterGeneration>` is a clean, real
  seam** — it mirrors how the flat summary is already threaded through the UI,
  and the module boundary (pure `query.rs` on `&Tree`, unit-testable on
  synthetic trees like flat.rs) is the right one.
- **The no-input-is-error model and one-size-dialect reuse are grounded.**
  `parse_size` exists (size.rs:87) and is the correct single dialect; the
  Everything "trailing incomplete term is inert" rule is a real, learnable
  convention (research §4.2).
- **The synchronous fold's one virtue is real:** blocking the single thread
  makes a stale-generation render impossible by construction (see [4]) — a
  genuine correctness property, not just a limitation.

## Verdict: SURVIVABLE WITH AMENDMENTS

A is not killable. Its engine thesis holds where it matters most — rayon over
a frozen arena is safe (not just defensible, *safe*, verified against the
single-writer rule), the retention menu is priced honestly, the invalidation
machinery already exists, and the module boundary is clean. The parser/palette
is a lot of new UI (§9.5, true) but nothing in it is unsound.

What it does **not** survive is its own spec sheet. Three "honest" claims are
quietly false (a filter that reports 0 for extant files [1], a match-all gap
shown but explained once [7], breakdown groups that don't exist under a filter
[6]); the "instant, never blocks" pitch is untrue at the scale that motivated
the whole debounce discussion [4]; two reserved sigils are already spent [3];
and `q`-always-quits makes the palette unable to type `q` [2]. A courtesy pass
waves these through because the *architecture* is right — but the tool's entire
brand is *not shipping a number you can't stand behind*, and [1], [6], [7] each
ship one.

Required amendments (severity order):

1. **Extra hardlinks must still *match* their name** (0 bytes, flagged with
   the canonical path) — never "never match." A filter that names a file and
   reports it absent is the cardinal sin. [1]
2. **Suspend `q`-always-quits while the palette input has focus** (Ctrl-C
   only). [2]
3. **Resolve the `<`/`>` collision**: size stays qualifier-only *or* grouping
   uses an unspent sigil — the reserved list must not name bytes the live
   grammar already owns. [3]
4. **Stop claiming "never blocks a keypress"** at 10 M; draw a `filtering…`
   frame before the blocking fold or state the freeze; spec the async hatch's
   arrival-time generation check for when it's built. [4]
5. **Rewrite the fold's memo story**: a query-scoped *immutable* verdict
   table (precomputed sequentially), per-thread top-N heaps merged, chunk by
   `DirId` not by node — not "reuse `NameMemo`." [5]
6. **Define breakdown × filter**: filtered pattern-fold, or `b` ignores the
   filter with a flash — never silent unfiltered groups under a filtered
   header. [6]
7. **Make the "matched · of scanned" denominator honest** to the
   dir-inode-excluded match model. [7]
8. Guard bare `>`/`<` behind a following digit and reserve a literal-substring
   escape (`'term` / `name:`) so real filenames stay reachable. [8]
9. State the see-but-can't-bulk-act limitation as the headline phase-1 caveat;
   handle marks stranded by a filter change. [9]
10. Define filter-applied-while-inside-a-now-empty-subtree; document that
    ancestor tokens don't target the scan root; fix the pill's "Esc clears"
    to its real scope; fill the history-file write-safety and `--no-ui`
    top-dirs gaps. [10–14]

With 1, 6, 7 done, A is the honest option it claims to be. With 2, 3, 4 done,
it is also usable and forward-compatible. Without 1–3 it prints exactly the
kind of number — and refuses exactly the keystroke — its own thesis forbids.
