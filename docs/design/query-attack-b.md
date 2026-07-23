# Adversarial review — Option B (expression grammar, apply-on-Enter)

> Verdict: **KILL as the phase-1 design.** Strategically dominated by
> option A, which reaches the same common-case queries *live* and can
> grow *additively* into exactly B's grammar if field evidence ever
> justifies it. B freezes the maximal syntax surface on day one — before
> any evidence that disk-cleanup users want boolean grammar — buying a
> thin phase-1 increment (OR + grouping, largely duplicable by a
> value-list sugar) at the cost of an irreversible maintenance surface
> and the product's live-cockpit identity. Survivable-with-amendments
> only if the project has *already decided* it wants the grammar; the
> amendments are listed at the end.

Claims were checked against `main`: `camembert-core/src/tree.rs` (node
fields, hardlink attribution), `camembert-core/src/flat.rs` (fold,
`HARDLINK_EXTRA` = contributes 0, memo), `camembert/src/ui/state.rs`
(mode/mark/phase state), `camembert/src/ui/keymap.rs` + `ui.rs` (Esc
ladder, `spawn_freeable_sweep`), `camembert/src/main.rs` (`--no-ui`
summary fold). Compared against `query-option-a-live-qualifiers.md`
throughout, since B defines most of its semantics as "identical to A."

## Findings

### FATAL

1. **The phase-1 value B buys over A is thin, and B pays for it forever
   [FATAL — strategic].** Enumerated exactly: A phase-1 already gives
   implicit-AND of terms *and* per-term negation (`!term`, any term).
   So B's entire delta over A in phase 1 is **disjunction and
   parenthesized grouping** — nothing else. Every other predicate B
   lists (size, mtime-age, kind, basename glob, dir-glob, path glob,
   flags) is expressible in A too; B's own §2.1 admits path globs ride
   along only "because the grammar is already paying the parser bill,"
   which is bundling, not necessity. Now weigh the delta against real
   disk-cleanup queries:
   - "big and cold" — `size>100M older:6mo` — AND only. No OR.
   - "all node_modules" / "all .cache" — dir-glob. No OR.
   - "several extensions" — `*.log or *.tmp or *.bak` — the dominant
     disjunction in cleanup, and it is precisely what A reserves the
     `;` value-list sugar for (`ext:log;tmp;bak`), which covers it
     **without a boolean grammar at all**.

   The residual that genuinely needs parens — cross-field disjunction
   grouped against an AND constraint, `(*.log or *.tmp) and not
   node_modules/` (the doc's *own* headline example) — is rare in
   cleanup, and semantically odd: you delete logs and tmp on different
   retention judgments anyway, and "not under node_modules" is
   redundant when you'd delete node_modules wholesale. So B ships the
   full precedence/quoting/escaping/parser surface (findings 2, 3, 6)
   on day one for a phase-1 increment that is (a) mostly one operator,
   OR, and (b) mostly coverable by a sugar that is not a grammar. This
   is the mirror image of scan-tree option B's kill: **the one real
   innovation is A's designed phase-2, and belongs grafted onto A when
   evidence warrants — not shipped as a distinct maximal design now.**

2. **"Complete grammar, no phase-2 syntax" is irreversible in the worst
   direction, and locks in before any evidence [FATAL].** A reserves a
   *small* token set and rejects `|`/`()`/`;`/path-globs *with hints*,
   so A's evolution is **purely additive**: previously-invalid input
   becomes valid; no saved A query can ever break, because A only adds
   meanings. B ships the maximal grammar first (§2.1), so its evolution
   is the inverse and strictly worse:
   - **Cannot simplify.** If the field says `mtime < -6mo` was a
     mistake, B can add an `older:` sugar (additive) but can never
     *remove* the `mtime < -6mo` form — saved queries and CI scripts
     use it. B can only accrete: it starts complex and can only get
     more complex, never shedding what didn't earn its keep.
   - **Cannot retreat.** If the grammar proves unlearnable — the exact
     risk B is built around (§7.1–7.3) — the project cannot fall back
     to qualifier tokens: saved boolean queries become unparseable. A
     retreat is a breaking change. A's reserved-but-unshipped `|` costs
     nothing to walk back.
   - The bet is made *before launch*, with zero field evidence that
     disk-cleanup users want boolean grammar at all, and the research
     evidence (§4.4, §4.6) points the other way — the SQL/expression
     family is "respected and *niche*"; the loved live filters are
     qualifier tokens and fuzzy. B §7.5 concedes this ("near-impossible
     to change," "dump-format major-versions-are-taboo logic applies").
     That concession *is* the kill: you do not freeze a maximal,
     irreversible syntax before you know anyone wants it.

3. **The shipped "complete" grammar does not specify precedence or
   associativity — and the first saved query freezes whatever the
   implementation happens to do [FATAL as specified].** §2.1's BNF is
   literally ambiguous: `expr := expr "or" expr | expr "and" expr | ...`
   encodes no binding. `*.log and size > 100M or *.tmp` has two
   readings. For a design whose whole pitch is "exact, complete,
   reproducible," an unstated precedence is not a detail — it is the
   load-bearing decision, and per finding 2 it is unchangeable the
   instant one user saves a query or a CI job pins one. Fixing a wrong
   precedence later silently changes the meaning of saved queries: a
   correctness regression with **no error and no diff**. A never has
   this problem in phase 1 (implicit-AND has no precedence to get
   wrong). This must be pinned *in the design doc* before any code, and
   even then it is frozen forever on first use.

### SERIOUS

4. **Apply-on-Enter is a dead island in a live cockpit — it fights the
   product's identity harder than A's debounce does [SERIOUS].** Every
   other surface is now live and reactive (verified): browse-during-scan,
   the live flat accumulator publishing a provisional summary every
   frame (`flat.rs` `Accumulator`), animated bar/donut morph
   (`view_change_seq` in `state.rs`), the "updating…" note, mouse-hover
   updating the card without moving the cursor. Into that, B drops the
   *one* input that produces **no** visual response until commit: a
   static "press Enter to apply" hint, no live count, while the user
   types a 30-character expression like `(*.log or *.tmp) and mtime <
   -6mo`. The donut/table don't feel "dead" — they feel *frozen
   relative to the input* for several seconds per query, showing the
   previous filter's numbers the whole time. This is strictly worse
   than A's 100 ms debounce, where the numbers track the typing. The
   closest prior art to camembert's filter-re-aggregates-a-tree —
   **broot** — has `&`/`|`/`!`/parens *and applies them live* (research
   §4.6). B adopts broot's operator grammar and throws away broot's
   live application: the cost of the grammar without the habitat that
   makes it feel alive. B §7.1 states the bet plainly; the field
   evidence (§4.10) is that the live-magic moment is what converts.

5. **`and`/`or`/`not` as bare ASCII keywords collide with real filenames,
   forcing a quoting/escaping story B never spells out [SERIOUS].**
   Concrete: a music library with `Rock and Roll.flac`, a report
   `Q1 and Q2.xlsx`, a directory `black and white/`. To match the
   literal substring " and ", the user **cannot type `and`** — the
   tokenizer reads the operator. Typing `Rock and Roll` bare becomes
   `glob("Rock") AND <operator> AND glob("Roll")` — a parse error, or
   worse a silent wrong match. The escape hatch is `name ~ "*and*"`, but
   then:
   - the sugar (`glob := bare "*.log"`) is *unquoted*, so the moment a
     filename glob contains a space, a paren, or a keyword the user
     must abandon the sugar and switch to quoted `name ~ "..."`;
   - filenames are **raw bytes, not UTF-8** (`tree.rs` interns raw
     bytes; the test round-trips `caf\xe9.log`), so the quoted-string
     lexer must carry non-UTF-8 bytes a naive `&str` parser cannot
     represent;
   - a filename can contain `"` (`say "hi".txt`), so `\"` escaping is
     needed, which forces `\` escaping, an escape grammar of its own.

   Also **parens**: B uses `(` `)` for grouping; Everything deliberately
   chose `<>` *because* "literal parens are common in filenames"
   (research §4.2). `report (final).log` is a legal, common name. A's
   collision surface is only whitespace plus leading sigils (`!`, `>`,
   `<`, `:`); B adds three English words *and* parens to the reserved
   set — a strictly larger surface that bites on ordinary filenames.
   None of this escaping story is in the proposal.

6. **The engine section is internally contradictory at 10 M, and the
   contradiction hides a choice B claims to have dodged [SERIOUS].**
   §4: "Fold on apply, **single-threaded, on the UI thread**: 0.3–0.7 s
   @ 10 M behind a 'filtering…' placeholder row (**the freeable panel's
   idiom**)." Verified: the freeable panel's idiom is `spawn_freeable_sweep`
   — a `thread::Builder` worker polled non-blockingly (`ui.rs`), i.e.
   **off-thread**, the opposite of "on the UI thread." You cannot have
   both: either the fold blocks the UI thread for up to 0.7 s (then
   there is no animated placeholder, and every keypress — including
   `q`/Ctrl-C — queues dead for that window) **or** it goes off-thread
   (then it is not single-threaded-on-UI, and it needs the
   `Arc<ScanOutcome>` ownership dance A explicitly defers because it
   collides with deletion's `&mut`). B markets itself as dodging A's
   async complexity; the 0.7 s number forces the same choice A faces.
   Mitigation that is real: apply-on-Enter is human-frequency, so the
   hit lands once per deliberate Enter, not per keystroke — a single
   0.7 s freeze per Enter is far more tolerable than repeated ones while
   typing. That is B's one honest engine advantage, but it is not
   "non-blocking single-threaded placeholder" — that phrase is false.

7. **Esc gains an error-dismiss meaning A never has, making the ladder
   unpredictable [SERIOUS].** Current ladder (verified in `ui.rs`):
   confirm > review > freeable > cheatsheet; then contextual Esc leaves
   a `t`/`b` mode, else quits from tree. A adds two rungs (palette,
   active-filter) but keeps Esc monotone. B adds a **third** Esc meaning
   that only B has: §2.3 "Esc dismisses the [on-Enter] error." So at one
   cursor position, after a bad query: Esc #1 dismisses the red span
   (palette stays open), Esc #2 closes the palette, Esc #3 clears the
   previously-applied filter, Esc #4 leaves the mode, Esc #5 quits. Five
   Escs, five effects, and the first two are indistinguishable to the
   user ("did that close the palette or just the error?"). This
   directly violates research §9 ("where does this Esc go must stay
   fully predictable"). A's never-error model has no error to dismiss,
   so A's Esc is one meaning shorter and cleaner — a direct consequence
   of B choosing a hard-error model.

8. **Hardlink canonical attribution under a partial filter silently
   reports 0 bytes — and B's precision pitch makes it worse [SERIOUS,
   shared with A but sharper here].** Verified in `tree.rs`/`flat.rs`:
   a `HARDLINK_EXTRA` link contributes 0 and never ranks; only the
   canonical (counted) link carries the bytes. Scenario: an inode with
   canonical link `/keep/big.iso` (4 GB, counted) and extra link
   `/scratch/big.iso` (0, extra). Query `*.iso and not keep/` matches
   the *extra* under `/scratch` — which contributes 0 — so the filtered
   view shows `big.iso · 0 B`. The user concludes it is not worth
   deleting, when deleting *both* links frees 4 GB. B defers this to
   "same candidate model as option A §2.2," but A's §2.2 doesn't spell
   the canonical-under-filter case out either, and B's headline claim
   ("every applied query is exact, complete") makes a silent 0-byte
   match for a real 4 GB file especially damaging. Both options must
   state the rule (the counted link's group, not the matched link's,
   is where the bytes live); B needs it more because it promised
   exactness.

### ANNOYING

9. **`mtime < -6mo` is the least learnable token in either option, with
   no on-ramp [ANNOYING].** It requires the user to grasp three things
   at once: `mtime` compares to a *time*, `-6mo` is a *negative
   duration* meaning "6 months before now," and `<` a past time means
   "older." A's `older:6mo` is one named qualifier that reads as
   English. B §7.3 concedes there is no bare-terms-first on-ramp for
   numeric/date fields, and leans on the teaching-error to rescue it —
   but research §4.6 (jq/PromQL) says good errors carry casual users
   "not far." So B's flagship error-quality capability is spent mainly
   papering over a self-inflicted wound (`-6mo`-as-timestamp) that A
   does not have. (Aside: the task's own example `older(6mo)`
   function-call syntax appears *nowhere* in B's grammar — the age
   idiom really is `mtime < -6mo`, evidence the form is unintuitive
   enough that even its advocates misquote it.)

10. **Spaced `<`/`>` operators are a shell redirect footgun; the
    "strongest CLI story" is oversold [ANNOYING].** `camembert --filter
    size > 100M` unquoted does not error — the shell **truncates a file
    named `100M`** and passes `size` as the filter. `<` is an input
    redirect. B's spaces-around-operators style (`size > 100M`,
    `mtime < -6mo`, plus `(`/`)`) means the expression is dense with
    shell-significant characters that *mandate* quoting, and a forgotten
    quote is silent data loss, not a syntax error. A's attached-sigil
    sugar (`>100M older:6mo`) reads as single tokens and is marginally
    safer. And the CLI "strongest story" claim (§5) rests on
    expressiveness that finding 1 shows is mostly OR — in a script you
    can run the tool twice and union, or use the value-list sugar; the
    incremental scripting value of full boolean grammar over
    qualifier-tokens-with-value-lists is thin, bought with a more
    shell-hostile syntax.

11. **`not` worsens the marking honesty trap [ANNOYING].** B reuses A's
    dir-marking refusal under an active filter (correct — verified
    `toggle_mark` currently has no filter awareness, so both options
    must *add* it). But B's `not` lets a user write `not *.log`, under
    which a directory row shows its *non-log* subtree size while `Space`
    on a file row still marks the real node. The gap between "the number
    on this row" (filtered) and "what deleting frees" (real subtree) —
    research §5.3 — is more counterintuitive under negation than under
    A's positive-only common case. Marginal, but it is extra rope B
    hands the user that A does not.

### COSMETIC

12. **Docs cost is a language reference, not a token table [COSMETIC].**
    B §6 admits it: the README section is a grammar (precedence, quoting,
    escaping, the `-6mo` idiom, operator words) versus A's table of
    tokens. Per CLAUDE.md's same-change docs rule, every one of the
    escaping rules in finding 5 must be written and maintained. Not
    fatal, but it is the largest doc surface of the three and grows with
    every accreted-but-never-removable form (finding 2).

## Survived (steelmanned — B is not stupid)

- **Shared core parser, zero TUI/CLI drift** (`camembert-core/src/query.rs`,
  one grammar): sound. A shares it — not unique to B.
- **Apply-on-Enter genuinely enables teaching-quality errors A cannot
  have.** A's never-error model structurally *can't* say "did you mean
  `-6mo`?" This is a real, unique B capability (§2.3). It is just
  deployed mostly to fix B's own syntax wounds (findings 5, 9).
- **One fold per Enter avoids per-keystroke rayon/debounce/latest-wins.**
  At human frequency this is a real engine simplification over A —
  provided finding 6's 10 M blocking-vs-off-thread choice is made
  honestly.
- **Post-scan-only, dumps-never-filtered, `diff --filter` near-free,
  module boundary**: all sound — and all shared with A.
- **The grammar is the right long-term target.** B's syntax is
  essentially A's designed phase-2. The disagreement is *timing and
  reversibility*, not direction: A can become B additively; B cannot
  become anything else.

## Recommendation

Do not ship B as phase 1. Ship A: it reaches the same common-case
cleanup queries *live*, reserves `|`/`()`/`;`/path-globs additively so
it can grow into precisely B's grammar **if** field evidence ever shows
disk-cleanup users want boolean composition, and it freezes nothing —
no precedence, no maximal surface, no irreversible saved-query format —
before that evidence exists. B's grammar is A's phase-2, and the correct
time to build it is when a real user's real cleanup task is blocked by
the lack of OR — not before launch, on the strength of a headline
example (`(*.log or *.tmp) and not node_modules/`) that no one cleaning
a disk actually types.

### If B is chosen anyway — mandatory amendments

1. **Pin precedence and associativity in this doc** (`and` binds tighter
   than `or`; `not` tightest; left-associative), and state that they are
   frozen on first saved query (finding 3).
2. **Write the full quoting/escaping grammar** now: quoted-string rule,
   `\"`/`\\` escapes, raw-byte (non-UTF-8) handling, and the rule for
   matching a filename containing `and`/`or`/`not`/`(`/`)`/spaces
   (finding 5). Consider `<>` grouping instead of parens to dodge the
   filename-paren collision, per Everything.
3. **Resolve the engine contradiction** (finding 6): commit explicitly
   to off-thread fold with the `Arc<ScanOutcome>` ownership plan and its
   interaction with deletion's `&mut`, or accept and document a bounded
   UI-thread freeze with a hard cap and a responsive quit path. "Single-
   threaded on the UI thread behind a placeholder" is not a thing.
4. **Collapse the Esc ladder** (finding 7): make the on-Enter error a
   non-modal inline hint that does *not* consume an Esc, so Esc stays
   monotone (close palette > clear filter > leave mode > quit).
5. **Add an `older:`/`size:`-style named-qualifier sugar layer** over the
   grammar (finding 9) so the common case has an English on-ramp and
   `mtime < -6mo` is the escape hatch, not the front door. (Note this is
   additive and safe — and it is also most of A, which is the point.)
6. **State the hardlink-canonical-under-filter rule explicitly** in the
   semantics (finding 8): the counted link's group holds the bytes; a
   matched extra shows 0 and must be visibly badged, never a silent
   0-byte row.
