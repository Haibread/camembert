# Adversarial review — Option C (split surfaces: `/` filter, Ctrl-K commands)

> Verdict: **KILL AS A STANDALONE OPTION.** The split's one good idea
> (`/` as a filter entry point) grafts cleanly onto A; everything C adds
> on top of that graft is either a semantic mis-sell of its own
> precedent or a second copy of machinery A already owns once. C is "A
> minus the palette's free-text fallback, plus a redundant input
> surface, wearing a manifesto."

Claims verified against `main`: the fixed six-region vertical layout
(`ui.rs` `Layout::vertical` — header 1 / cards 3 / gauge 1 / main Min(3)
/ basket 0-1 / footer 2), zen collapsing cards+gauge to zero
(`cards_and_gauge_heights`), the mini-donut riding `header_area`
sub-100-col (`draw_mini_donut`, `MIN_WHEEL_TERMINAL_WIDTH = 100`), the
modal ladder (`handle_key`: confirm > review > freeable > cheatsheet,
each branch `return`s), the absence of any text-capture input mode
(every arm of `handle_key`'s normal-mode match is a single-key command
dispatched straight into `UiState`/`keymap`), config-only XDG usage (no
state dir exists — `main.rs` after-help, research §2.5), post-scan-only
folding (`ensure_flat_summary_fresh` is a no-op under `Phase::Scanning`;
the UI holds no arena mid-scan), and canonical first-seen hardlink
attribution (`tree.rs` `is_hardlink`/`hardlink_firsts`, `flat.rs` fold
`HARDLINK_EXTRA` contributes 0).

## Findings

### 1. The k9s analogy is equivocation, and it is load-bearing [FATAL]

C's entire pitch (§1, §5.5) rests on "every TUI the audience runs reads
`/` as *filter this view*." But k9s `/` and camembert `/` are not the
same verb wearing the same key — they are different operations the pitch
calls by one name.

- **k9s `/`** is `grep` over the rows already on screen: an in-memory
  regex over the current view's visible list, instant, display-only,
  changes no aggregate, reverses for free. It narrows the *display*.
- **camembert `/`** is a qualifier-token query (`size:>100M older:6mo
  *.log`) that triggers a whole-tree parallel fold producing an
  *alternate dir-aggregate table* (research §2.3: ~50-120 ms @ 10 M,
  32 MB/generation), post-scan-only, that changes **every number in the
  cockpit** — the donut, the metric cards (total/entries), the disk
  gauge's coverage %, every subtree total in the breadcrumb chain. It
  narrows the *dataset and recomputes the world*.

The muscle memory a user brings to `/` — cheap, local, display-only,
instantly reversible — is *actively wrong* for what camembert's `/`
does: expensive, global, total-mutating, phase-gated. C borrows k9s's
discoverability precisely for the surface whose consequences k9s never
had. The precedent it leans on to justify the split is the precedent
that misleads about the split's effect. A design cannot cite "users
already know this key" as its safety argument when the thing behind the
key does something users have never seen that key do.

Worse, §2 makes it a selling point that the prompt *keeps the tree fully
visible* ("nothing covers the tree"). So the user types a filter,
watches the familiar browse surface stay put, and the donut + cards +
gauge silently re-compute to filtered numbers underneath a UI that still
reads like a plain k9s-style narrowing. That is the "mark a dir showing
42 MB of logs, delete 300 GB" honesty trap (research constraint 3)
dressed as a lightweight view-local filter. The one thing k9s `/`
guarantees — that filtering can never lie about totals, because it
computes no totals — is exactly what camembert `/` cannot guarantee.

### 2. The pure command palette dead-ends the single most-trained palette reflex [FATAL/SERIOUS]

C §3: Ctrl-K is a command palette "with no query grammar inside it,
ever." Empty input shows recents + saved queries; typed input fuzzy
-matches command *names* only.

The single most common thing a human does at a Cmd-K / Ctrl-K / Cmd-P
prompt, worldwide, is **type the name of a thing they are looking for**.
VS Code Ctrl-P: free text = file search. Slack Cmd-K: free text =
channel/person jump. Raycast, Linear, Telescope: free text = search.
The trained reflex is "palette open → type what I want → find it."

Concrete scenario: a user opens Ctrl-K and types `node_modules`
expecting to locate or filter it — the highest-frequency palette action
there is. In C, `node_modules` fuzzy-matches no *command name*. The list
goes empty. The most-practiced palette gesture on the planet produces a
blank result in a disk-usage tool whose whole job is finding
`node_modules`. The `filter…` delegation entry (§3) only rescues the
user who *scrolls to a command* instead of typing their target — i.e.
the user who does *not* have the reflex. C punishes exactly the users
whose habits it claims (§5.5) to be honoring.

A's sigil-switched single input (VS Code Ctrl-P school, research §4.7)
handles this natively: free text *is* the filter, `>` switches to
commands, `?` lists prefixes. The convention users actually have is
"free text first, sigil for commands" — which is A, not C. C inverts the
convention and calls the inversion clarity.

### 3. C is not a standalone option — it dissolves into "A plus a `/` alias" [FATAL, strategic]

C's own §6.1 concedes the Ctrl-K reservation (tui-design.md: "ships with
the filter/query language as its UI") reads most naturally as A, and
that under a literal reading "C is out of spec by definition." The
research survey already recommends grafting C's `/` onto A.

Follow that graft to its conclusion. Take C's one distinctive asset —
`/` as a dedicated filter key — and bolt it onto A. What is left of C?

- The engine, semantics, CLI, candidate model, dir-marking refusal,
  fold, `FilterGeneration`, epoch invalidation, `--filter`, dump rules:
  **all declared "identical to A" (§4).** Zero C content.
- The `/` filter key: **graftable onto A** (that is the survey's
  recommendation). Not exclusive to C.
- The remaining differentiator is therefore a *negative feature*:
  "Ctrl-K refuses free text." And that negative is precisely the
  muscle-memory dead-end of finding 2.

So C reduces to: A's engine + A's-graftable `/` + a palette
deliberately made *worse* than A's by removing its free-text fallback +
a second input surface with its own history and Esc semantics. There is
no positive standalone C. It is an amendment to A whose sole net
contribution over A-with-`/` is the removal of a feature users want.
Once the good idea is grafted, nothing of C remains that a reasonable
person would choose on purpose.

### 4. Duplication is not "two small things" — it is two embedded text editors [SERIOUS]

C §5.4/§6.2 frames the split as "each half simpler." Enumerate the
actual code surface against A's single palette, from the real
`handle_key`:

Today every normal-mode key is a single-char command dispatched straight
into `UiState`/`keymap` — there is **no text-input mode anywhere in the
UI**. A live `/` filter prompt requires a brand-new input mode that
captures every character key (letters, digits, `*`, `?`, `:`, `>`,
space, backspace) and routes them into a filter buffer *instead of* the
keymap — i.e. it shadows the entire command map (`j`/`k`/`g`/`d`/`t`/`b`
all become literal filter text while the prompt has focus). Ctrl-K needs
the identical text-capture machinery a second time. So the split ships
**two** embedded line editors in a TUI that otherwise dispatches single
keystrokes — not one small prompt and one small overlay.

Per-surface, C duplicates:

| concern | `/` prompt | Ctrl-K palette | A (single palette) |
|---|---|---|---|
| input widget | inline line editor + cursor | overlay line editor + cursor | one |
| render region | new persistent row (finding 5) | centered `Clear` modal | one modal |
| Esc semantics | restore pre-`/` state (stateful rollback) | close (stateless) | one |
| history | persisted XDG (new state dir, load/append/corruption) | in-memory recents | one |
| footer hints | filter hint set | command hint set | one |
| error model | never-error parse echo | none | one (sigil-scoped) |

Two of everything, and the two Esc behaviors are *different kinds*
(rollback vs close), the two histories are *different kinds* (persisted
vs volatile). "Each half is simpler" is true only if you never add the
halves together — which the user does, every session.

### 5. The inline `/` bar has nowhere to live [SERIOUS]

The vertical layout is six fixed regions with **no spare row**: header
Length(1), cards Length(3), gauge Length(1), main Min(3), basket
Length(0|1), footer Length(2). C §2 says the prompt "slides in above the
footer / between table and footer." That real estate is already claimed:

- **The basket strip already lives above the footer** (`basket_area`,
  Length 1 whenever anything is marked). With a mark active *and* the
  filter open you need two rows above the footer, both stolen from
  `main` — which is already Min(3) and already sheds the selection card
  below `left_area.height >= 9` and the wheel below its own threshold.
- The `/` bar therefore permanently shrinks the table on the product's
  core surface, for as long as it is open — the opposite of A's overlay,
  which taxes the tree transiently and returns every row on Esc.
- **Sub-100-col**, the mini-donut already rides `header_area`
  (`draw_mini_donut`), which is packed (signature + breadcrumb path +
  status + mode badge). The parse echo C wants "in the prompt's right
  half" (§2) competes for exactly the horizontal space that is already
  collapsing at that width.
- **Zen mode** (`z`) collapses cards+gauge to zero to maximize the
  table. A persistent `/` row fights that intent directly — and the
  numbers a live filter actually changes (donut, cards) are the very
  things zen *hides*, so in zen the inline bar shows a filter re-
  aggregating totals the user can no longer see.

C sells "the tree stays visible while filtering" (§5.3) as the split's
best moment. But the values that move under a re-aggregating filter are
the donut and the cards, not the row names — and the bar buys tree
visibility by permanently confiscating a table row precisely where rows
are scarcest.

### 6. The Esc ladder gains two filter-flavored rungs of different kinds [ANNOYING]

The verified ladder is confirm > review > freeable > cheatsheet, then
contextual Esc leaves a flat/breakdown mode, then quits from tree view;
`q`/Ctrl-C always quit. C inserts *two* new surfaces of *different
kinds*: Ctrl-K as a modal rung (fine, like cheatsheet), and the `/`
prompt as a non-modal-but-input-capturing surface whose Esc does a
**stateful rollback** ("restores the state before `/`", §2) that nothing
else in the ladder does. Post-split, Esc means, in order: close palette
| cancel-and-rollback the `/` prompt | clear the committed filter (tree
view) | leave a mode | quit. Five meanings, two filter-related, one of
them a rollback with no sibling in the current design. A adds the same
filter-clear rungs but with a *single* input surface's Esc to reason
about, not two whose semantics diverge.

### 7. Asymmetric history persistence is a user-model wart [ANNOYING]

C §3 mandates a **new** `$XDG_STATE_HOME/camembert/history` for `/`
(the codebase uses no state dir today — verified: only
`$XDG_CONFIG_HOME` is read), while Ctrl-K gets in-memory recents. So one
of the two surfaces introduces precedent-setting persistent IO (startup
load, append-on-commit, corruption handling) and the other does not. The
user-visible result: "why does my filter history survive a restart but
my command history doesn't?" A single palette with one history store has
no such seam. The split manufactures the asymmetry, then has to document
it.

### 8. Cross-cutting traps C inherits and the split does not help [inherited, MAJOR but shared]

These bind every option; noted because §5.2 implies the split addresses
them and it does not:

- **Filtered totals × hardlinks**: canonical attribution is first-seen /
  smallest-path (`hardlink_firsts`; `HARDLINK_EXTRA` contributes 0). A
  filter that hides the canonical link but keeps an extra link attributes
  0 bytes to the visible file — a filtered total that under-counts real
  occupancy. This is a totals-semantics problem; C's "two small error
  models" (§5.2) is an *input-parsing* framing and does not touch it.
- **Marks under a filter**: the 42 MB-visible / 300 GB-real deletion trap
  (constraint 3). C's split says nothing about it; group-marking's guard
  is a separate deferred design.
- **mode × filter × scan-phase inconsistency**: `/` is post-scan-only and
  flashes "available when the scan completes" mid-scan (§2, correct —
  the UI holds no arena under `Phase::Scanning`). But `t`/`b` flat modes
  *do* work live, badged "provisional." So the product tells the user
  "flat views work during the scan, but the filter that feeds the same
  fold engine does not" — a phase-consistency wart the split makes more
  visible by giving the filter its own always-present key that is dead
  half the time.
- **`--filter` fold source (`--no-ui`)**: folds once in `summary()` over
  the finalized tree — fine, and shared with A. Not a C-specific defect,
  listed only to close the checklist.

## Survived

- **Independent shipping (§5.4)** is genuinely true: the `/` filter plus
  the engine can land without the palette. But that is an argument for
  *sequencing* — ship the filter first, the palette later — not for
  *two permanent surfaces*. A can ship its filter half first and add
  command mode behind the same `>` sigil later; sequencing does not
  require splitting.
- **"Two small error models" (§5.2)** has a sliver of merit: a
  never-error prompt plus a no-error palette is conceptually clean. But
  A's sigil already scopes the error model — filter-mode feedback vs
  command-mode feedback in one input — so the benefit is marginal, not
  structural, and does not survive being weighed against findings 2-5.
- **Engine, semantics, CLI, dump rules**: sound — because they are A's,
  copied verbatim (§4).

## Recommendation

**Kill C as a standalone option.** Adopt A. Salvage exactly one idea from
C into A as an amendment:

1. **Bind `/` as a shortcut that opens A's single palette pre-scoped to
   filter mode** — the inverse of VS Code's "Ctrl-Shift-P is Ctrl-P with
   `>` pre-typed." This gives the k9s-familiar `/` key *without* a second
   input surface, *without* split histories, *without* the inline-bar
   layout tax, and *without* the pure-palette free-text dead-end.
2. **Keep the palette free-text-first** (A's sigil model): typing a
   filename filters; `>` switches to commands. This is the convention
   users actually have (finding 2).
3. If a live "tree stays visible" filter is later judged worth its cost,
   revisit it as an *A-mode variant* (palette collapses to a one-line
   docked form after commit), decided on its own merits — not as a
   reason to maintain two input widgets from day one.

C's contribution to the design record is real but singular: it forced
the "one input vs two" question into the open (its own §4 says so).
Answered honestly, the question kills the split. `/` is a keybinding A
should have; it is not a second surface A should grow.
