# Adversarial review — freeable phase 2, Option C (viewport-first FIEMAP trickle + oracle)

> Target: [freeable2-option-c-viewport-trickle.md](freeable2-option-c-viewport-trickle.md).
> Method: every load-bearing claim checked against the real render loop
> and engine (`camembert/src/ui.rs` event loop + slice-5 idle polling,
> `camembert/src/ui/state.rs` epoch machinery, `camembert-core/src/tree.rs`
> `apply_removal`/`children`, `camembert-core/src/freeable.rs` sweep
> lifecycle), against Option B ([freeable2-option-b-eager-floor.md](freeable2-option-b-eager-floor.md))
> as the cost baseline, and against the research digest
> ([freeable2-research.md](freeable2-research.md), §n). Not against the
> option's own prose.
>
> **Verdict: KILL for phase 2. Two independent fatal problems. (1) The
> repeated promise that the mapper is "channel-woken, idle-quiescent loop
> preserved — no 33 ms busy polling returns" (§4-3, §4-4, §11) is false
> against the actual render loop: `event::poll` watches only the terminal
> fd, and the codebase busy-polls at 33 ms cadence for as long as any
> background receiver is pending (`ui.rs:506-512`, `755-779`). A
> continuous trickle keeps that receiver pending for the whole mapping
> window — so C's target beneficiary, the short laptop session, is the one
> session that is _never_ quiescent. (2) C's central invariant — "a partial
> floor is a valid floor, its only error direction is understatement"
> (§1, §2, §11.2) — does not survive C's own long-lived streaming mapper
> crossing a deletion: a late in-flight floor delta for a just-deleted
> subtree lands on ancestors that already subtracted the partial
> `dir_floor`, making the ancestor floor _overstate_ — the one direction
> the whole proposal swears it cannot lie in. Beyond the two fatals, C
> collapses into B for any session that outlives the trickle (§11.5, its
> own admission), does _nothing at all_ on ext4 (§8: tier-H is instant),
> and its "savings" survive only in a btrfs × short-session × huge-tree ×
> quit-early intersection that is narrower than its permanent cost.
> Recommend: ship B (or A). Do not build the scheduler.**

The single sharpest fact: C's differentiator over B is entirely a
_scheduler_ (§11.1 concedes "the scheduler is the design"), and that
scheduler's two headline properties — "cheap because the loop stays
idle" and "honest because a partial floor can only understate" — are
each contradicted by code C never reconciles with. Strip both and C is
B with more moving parts and a patchier display.

## Findings (severity-ranked)

### FATAL-1 — "channel-woken, idle-quiescent loop" is not how this UI works; the trickle pins the loop at 33 ms for its whole lifetime, and pins it hardest for the sessions C exists to help

§4-3: floor deltas go "up a channel; the UI thread folds them into
`FloorMap` at its own cadence (channel-woken, idle-quiescent loop
preserved — no 33 ms busy polling returns)." §4-4 and §11 repeat the
claim. It is the load-bearing answer to the battery objection.

The render loop cannot be woken by a channel. `event::poll(deadline)`
(`ui.rs:512`) is crossterm's terminal poll — it waits on the **terminal
input fd only**. There is no `crossbeam::select!`, no self-pipe, no
eventfd, no waker bridging an mpsc/crossbeam channel into that poll
(confirmed: the only `select`/`recv_timeout` in the tree is the scan
engine's own `crossbeam_channel`, `scan.rs:466`, not the UI). The way
the codebase reconciles "a background result must land promptly" with
"a terminal poll can't see a channel" is brute force: it shortens the
poll deadline.

```
let mut deadline =
    if needs_frequent_polling(&phase, &flash, &toasts, &motion, &sweep_rx, &palette_rt) {
        FRAME            // 33 ms  (ui.rs:113)
    } else {
        IDLE_POLL        // 3600 s (ui.rs:121)
    };
```

`needs_frequent_polling` (`ui.rs:764-779`) returns `true` whenever
`sweep_rx.is_some()` or `palette_rt.fold_rx.is_some()`. That is the
entire mechanism: while a background receiver is pending, the loop wakes
**every 33 ms** and `try_recv`s it (step 2.5, `ui.rs:630-650`). It is a
busy-poll by construction — the freeable sweep already works exactly
this way, and it is tolerable there **only because the sweep is
one-shot** (`spawn_freeable_sweep`, `ui.rs:737-752`): the receiver is
`Some` from scan-end until the single `Ledger` lands (~23 ms, §phase-1
`freeable.rs:50`), then it is set to `None` and the loop drops back to
the 3600 s idle poll. Slice 5's whole point (the `IDLE_POLL` constant,
`ui.rs:115-121`) was to make a quiescent post-scan UI cost ~nothing.

C's mapper is **not** one-shot. It is "one background mapper thread,
alive for the whole post-scan session" (§4) streaming deltas
continuously. For C's UI to fold those deltas via the only mechanism the
render loop has, `needs_frequent_polling` must stay `true` for the
**entire mapping window** — i.e. the loop busy-polls at 33 ms the whole
time the trickle runs. That is precisely "the 33 ms busy polling
returns," verbatim the thing §4 promises it doesn't.

Now the inversion that makes this fatal rather than merely wrong. How
long is the mapping window?

- **Long session on a 10 M btrfs tree**: ~2 min of trickle (§4 costs) —
  so ~2 min of 33 ms polling, then quiescent. B pays the _same_ ~2 min
  of busy-poll (its eager pass keeps `sweep_rx`-shaped state pending
  too), then quiescent forever. C draws with B here.
- **Short session on a laptop** — _the exact profile C's pitch is built
  on_ (§1: "glances at the top two levels and quits"): the session is
  short **because the user quits early**, and the trickle is running for
  **all of it** (viewport priority-1 re-primes the queue on "every
  navigation event," §4-1, so it never drains while the user browses).
  The loop is therefore at 33 ms cadence for 100 % of the session and
  **never reaches `IDLE_POLL` at all.** C's beneficiary is the one user
  who never gets the slice-5 quiescent state.

So C's own framing (pay only where the user looks, quit cheap) buys a
_smaller total ioctl count_ at the cost of a _loop that is hot the whole
time the user is present_ — on the laptop where that matters most. B's
eager pass, ironically, lets a short-session user reach quiescence
sooner on ext4 (tier-H floors land instantly, §8, receiver drops
immediately) and on btrfs the busy window is bounded by the pass, not by
how long the user keeps browsing. C strictly worsens the battery story
for its target profile while claiming to improve it.

Making the claim true is not a tweak — it requires bridging the mapper's
channel into `event::poll` (a self-pipe/eventfd registered alongside the
terminal fd, or migrating the loop to `crossbeam::select!` over
terminal-events + channel). That is a rework of the render loop's core
wait primitive — new surface in the exact place slice 5 just finished
stabilising, and none of it is in C's phasing (§10).

### FATAL-2 — C's floor can OVERSTATE (the one forbidden direction) because a long-lived streaming mapper's in-flight delta outlives a deletion; "a partial floor is a valid floor" does not survive C's own concurrency

The invariant the entire proposal rests on (§1, restated §2, defended
§11.2): "a partial floor is a valid floor. Σ exclusive bytes over any
subset ≤ the full floor ≤ true freeable … its only error direction is
understatement (≥)." Every honesty argument in the document — the
progressive display, the coverage tips, "≥ 1.2 GiB · 54 % mapped" being
true at every instant — depends on `dir_floor` being a monotone lower
bound that _cannot exceed_ true freeable.

It can, once C's mapper crosses a deletion. Trace it against the real
removal path.

`apply_removal(D)` for a directory (`tree.rs:504-563`) computes its
delta from `D`'s **current aggregates** (`meta.td` etc., `tree.rs:521-527`)
and subtracts them up the ancestor chain (`apply_negative_delta`,
`tree.rs:569-586`). C §7 extends this: "subtract the removed subtree's
`dir_floor` … up the chain." At the moment of deletion, `dir_floor[D]`
is C's **partial** value — say 1.2 GiB at 54 % coverage. So the ancestor
`P` has 1.2 GiB subtracted.

Meanwhile the mapper (§4-3, mechanics shared with B §4) extracts a
chunk's paths + `(NodeId, dev, ino, nlink, disk)` under a **read** guard,
_drops the guard_, then does `open`+FIEMAP+`close` **lock-free**, then
publishes deltas. A deletion takes the **write** guard (the filter-fold
doc, `ui.rs:876-880`, spells out this single-writer discipline). So there
is a window: a chunk covering files under `D` is extracted, the user
deletes `D` (write guard, `apply_removal` runs, `dir_floor[D]` subtracted
from `P` at 1.2 GiB), and _then_ the in-flight chunk's deltas arrive —
"file `f` under `D` has 0.3 GiB exclusive."

The UI folds that delta the only way an additive `dir_floor` can be
maintained: add 0.3 GiB at `f`'s node and **propagate up the ancestor
chain** (the mirror of `apply_delta`, exactly as `dir_floor` must to stay
additive). `P` is on that chain. So `dir_floor[P]` gains 0.3 GiB
attributable to a file that **no longer exists** — its subtree was
already accounted as removed at the partial figure. `P`'s floor now
claims "deleting `P` frees ≥ X + 0.3 GiB" when 0.3 GiB of that is already
gone. **The floor overstates.** A user who trusts the ambient "≥"
segment on `P` and deletes it frees _less_ than the number promised —
the precise failure mode §2's whole "why the floor doesn't lie" section
swears is impossible.

Why this is C's problem and not equally B's:

- C's mapper is **alive for the whole session** and **re-primed by
  navigation** (§4-1). The in-flight-delta-straddles-deletion window is
  therefore open continuously and — worse — _correlated with user
  behaviour_: you navigate **into** a directory to decide whether to
  delete it, which is exactly the signal that enqueues it at priority 1
  (§4-1) and puts its chunk in flight, and then you delete it. The race
  is not a rare interleaving; it is the mainline "inspect, then delete"
  gesture.
- B's pass is **one-shot** and its §4-5 cancellation is a per-chunk
  **deletion-epoch check** — B stamps its work against the epoch and
  drops chunks whose epoch moved. C's §4 mapper mechanics **never
  mention epoch stamping of floor deltas** (only §5's _oracle_ carries
  `(fingerprint, epoch)`). The codebase's stale-guard idiom
  (`in_flight: Option<(u64, u64)>`, `ui.rs:814`; discard-on-mismatch,
  `ui.rs:1008-1020`; `flat_epoch` bumped per deletion,
  `state.rs:1125-1131`) works because every guarded result is a
  **whole-value replace-or-discard**. C's `dir_floor` is an **incremental
  accumulator** — there is no "discard the stale result," only "this
  delta already got folded into a persistent sum." The epoch idiom the
  project relies on does not compose with a streaming accumulator, and C
  supplies no substitute.

The fix (epoch-tag every floor delta; on fold, verify the target node is
neither tombstoned nor from a superseded epoch; reconcile partial
subtractions) is real, invasive concurrency work at the post-scan
engine/UI boundary — and it is the boundary the project "has kept
deliberately simple" (C's own §11.1). Until it exists, C ships a floor
that lies upward.

### SERIOUS-3 — the win is confined to a btrfs × short-session × huge-tree × quit-early corner; the cost is permanent and paid even where the mapper does nothing

C's savings claim (§1, §11.5) is "a short session on a huge tree pays
only for what it looked at." Measure the population that actually
realises it.

- **ext4 and every non-extent filesystem** (tier H, §8): floors are
  "computed instantly at scan end … no ioctls … always 100 % mapped from
  the start." The mapper, the priority queue, the coverage vecs, the
  trickle, the gauge — **none of it runs**. On the most common Linux
  desktop filesystem, C's entire differentiator is inert, yet its cost
  (the +8 MB `dir_mapped` vec §3 admits is "C's own price," the
  scheduler code, the busy-poll machinery) is still carried. C degrades
  to "B, but with an extra 8 MB vec and a dead scheduler."
- **btrfs/XFS, session longer than the trickle**: C "converges on Option
  B's end state" (§4-2) and "the idle trickle converges on B's full pass
  anyway in any session longer than ~2 min" (§11.5, C's own words). Past
  that point C **is** B, plus the scheduler, plus the coverage bookkeeping
  B doesn't need, plus (FATAL-1) a longer hot-loop window.
- **The residual win** is therefore: btrfs/XFS **and** session shorter
  than the trickle **and** tree big enough that the trickle can't finish
  first **and** the user quits before it does. On a typical tree the
  trickle finishes in seconds (§4), collapsing the window to near-zero;
  the corner only widens at the 10 M design target, which is itself the
  rare tree.

Against B's "eager cost" the brief asks C to beat: B's cost is ~2 min of
_off-thread_ ioctl churn on a 10 M btrfs tree, once, then silence
(§B-11.2). C does not remove that work — it defers and reorders the same
open+FIEMAP+close per tier-F file (§4-3, identical mechanics). It saves
ioctls **only** for files the user never causes to be mapped **and** that
the idle trickle never reaches before quit. For that conditional,
bounded ioctl saving, C pays: a permanent scheduler (§11.1), +8 MB RSS
over B, a never-quiescent loop for short sessions (FATAL-1), an
overstating-floor race (FATAL-2), and findings 4–9 below. The savings do
not clear the cost for any session profile that isn't already the corner
case — and in the corner case the battery cost (FATAL-1) points the wrong
way.

### SERIOUS-4 — viewport priority inverts discovery: C shows floors where the user already looked, not where the space is

§4 priorities: (1) viewport — the current dir's visible rows and their
subtrees, on **every navigation event**; (2) idle trickle — the rest,
largest-first, only "when the viewport queue is empty." "Largest-first"
orders _within_ a tier; viewport (tier 1) unconditionally preempts the
breadth trickle (tier 2).

Scenario: a user drills a deep narrow path to inspect a suspected hog —
`a/ → a/b/ → a/b/c/ → a/b/c/d/`. Each `Enter` fires a navigation event
that re-enqueues the new viewport at priority 1 (§4-1), so the mapper is
kept busy on the drill path and its immediate children. The priority-2
breadth trickle — the pass that would map `a`'s _siblings_ at the root,
one of which may be the actual largest reclaim opportunity — **never
runs while the user is navigating**, because the viewport queue is never
empty for long. The user who goes looking for space by drilling in gets
ambient floors **only along the path they drilled**, and the biggest
dark-segment directory elsewhere in the tree stays literally unmapped —
invisible — _because_ they looked somewhere else.

This is a direct contradiction of the product question the freeable
feature answers ("where can I reclaim?", §B-1). B maps largest-first
globally (§B-4-3, "no viewport feedback loop") and surfaces the biggest
floors first regardless of where the cursor is; the biggest reclaim
opportunity lights up early even if the user is looking elsewhere. C's
viewport-first scheduler structurally cannot do that: it optimises for
"numbers next to the cursor," which is the navigation-comfort goal, not
the discovery goal — and the two conflict exactly when the space isn't
where the user is currently looking, which is the case that matters.

### SERIOUS-5 — the single `computed_at_last` timestamp is a per-row lie, and the smeared mapping window widens the external-write overstate hole

C's staleness model is one field: `computed_at_last: SystemTime` — "most
recent chunk" (§3), rendered as "updated 14:02" (§6 selection card).
Research §7 is unambiguous that extent sharing flips on **any write by
any process** at any time (backup snapshot, another user's
`cp --reflink`, a scheduled `duperemove`, btrfs balance/defrag), and §1
that the flip is near-instant. So a stored exclusive floor becomes an
**overstatement** the moment an external process reflinks/dedups that
file after it was mapped (B concedes this direction of error, §B-11.3).

C makes this strictly worse than B on its own signature axis. B maps the
whole tree inside one ~2-minute window, so a single "mapped at HH:MM"
roughly describes every row. C maps **progressively across the entire
session** — row X at 14:00, row Y at 14:30 — but exposes **one**
timestamp, "most recent chunk." That timestamp is honest only for the
last chunk folded; for the 54 %-mapped directory the user is staring at,
some contributing files were consulted half an hour ago and their
`computed_at_last` reads as fresh. C's whole pitch is _per-row coverage
honesty_ ("54 % mapped" per row, §6), yet its staleness field is
_not per-row_ — the ambient number can carry a session-length stale
window while displaying a minutes-old timestamp. The overstate hole B
admits is, in C, both larger (the ambient map lives and is consulted for
the whole session, not landed-then-ignored) and mis-labelled (one
timestamp for N mapping times).

### SERIOUS-6 — C ships the first navigation-history-dependent rendered number, against the tool's reproducibility grain

§11.3 concedes it: "two identical scans in two sessions show different
segments at t+10 s depending on where the user wandered." The rest of the
tool is built to be reproducible — folds are `(fingerprint, epoch)`-keyed
and cached deterministically (`state.rs:985-987`, `ui.rs:924-955`), the
view-change seq is documented to _not_ bump on nondeterministic live
updates (`state.rs:354-358`), sort tiebreaks are stable. C's ambient
floor layer is the **first** rendered quantity whose value depends on
navigation history rather than on the tree.

For a "≥" figure this is _defensible_ in isolation (both partial floors
are true lower bounds). But it is corrosive for a disk tool in practice:
two colleagues screenshotting the same directory report different bright
segments, and the only true explanation — "you two browsed in a different
order" — is an unsupportable answer for a numeric column in a tool people
use to compare and to file "why does it say X" questions. B's segments
are a function of the tree and the elapsed mapping time only; C's are a
function of the tree _and the user's path_, and that is a new and
unwelcome property to introduce specifically on the number that claims
to be the honest one.

### ANNOYING-7 — per-row coverage honesty multiplies the annotation burden by the viewport, where B carries one global gauge line

C's honesty mechanism is per-row: a not-fully-mapped row renders "≥ 1.2
GiB excl · 54 % mapped" plus a dim coverage tip in the bar (§6). In a
~40-row viewport mid-trickle that is up to 40 distinct coverage
percentages, each changing over time as chunks land, each a number the
user must learn to _discount_ ("54 % mapped" means "this could grow").
B carries **one** line — "mapping extents… N %" (§B-6) — and otherwise
clean bars: a single global progress signal versus forty per-row
qualifiers. C itself flags the sharpest edge (§11.2): a user reads "no
segment" as "nothing exclusive here," a wrong inference C's own UI
invites, and the per-row tip is a weak counter-signal against the strong
"a bar with no bright part means empty" frame. The honesty tax scales
with the row count and animates under the eye; B's is O(1) and static.

### ANNOYING-8 — fill-in flicker lands on the rows the user is actively reading

Because viewport rows are mapped **first** (§4-1), C's bright segments
_appear over time on exactly the rows the user is staring at_ — the bar
landscape shifts under the cursor as chunks land (§11.4 concedes it; the
eased-animation machinery, `ui.rs:485-487`, `motion.is_active()`, only
softens the transition, it does not remove the fill-in). This is a
per-navigation event: land on a directory, watch its rows' segments grow
in for ~0.1 s. B has a single "floors landed" transition for the whole
tree and thereafter a stable landscape. Attack-b's amendment 6 pushed
for stable segment presence precisely to avoid the eye-drawing shift; C
re-introduces it as its "signature" (§6) and books the animation system
to paper over it, which is motion cost on the hot path C is elsewhere
trying to keep cool (FATAL-1).

### ANNOYING-9 — the long-lived mapper is a lifecycle shape the codebase does not have, in the boundary it deliberately kept simple

Every off-UI-thread pattern in the tree today is **one-shot + discard**:
the freeable sweep (`ui.rs:737-752`, single `Ledger`), the filter fold
(`ui.rs:881`+, single `FilterResult` guarded by `(fingerprint, epoch)`
and thrown away whole on mismatch, `ui.rs:1008-1020`). The concurrency
budget of the post-scan engine/UI boundary is "spawn, receive one value,
fold or discard." C introduces a **persistent streaming producer** into a
**persistent mutable accumulator**, with cancellation semantics
(generation-counter supersession, §4-1), navigation-driven re-priming,
per-dir coverage arithmetic that must stay in lockstep with `dir_floor`
across removals (§7: numerator and denominator "shrink together"), and
media-aware throttling for HDDs (§11.6, "more scheduler still"). This is
C's self-identified "the scheduler is the design" (§11.1) — and it is
several new bug surfaces (the FATAL-2 race is one instance) in the one
region the project has kept to a single-writer, one-shot discipline.
Against B's "one pass, biggest dirs first," C's incremental machinery is
a large permanent complexity draw for the conditional saving of SERIOUS-3.

### COSMETIC-10 — the concessions C already books are real losses, not neutral trades

- **No `SortKey::Exclusive`** (§6): C drops the sort key B offers because
  a partially-filled floor would rank directories by _how much has been
  mapped_ as much as by exclusivity. Correct call — but it means C
  delivers _less_ ambient capability than B while costing more, and a
  session that wants the sort "should prefer B" (§6, C's words). A design
  whose own doc routes the power user to the competitor is not winning
  the comparison.
- **Flat-mode / breakdown / filter floor sums** are "only honest at
  100 % coverage" and labelled "partial" before that (§6). The most
  hostile consumer (the global flat list, per HANDOFF value order) gets
  the least trustworthy floors under C, exactly when the trickle hasn't
  converged — i.e. in C's own target short session.
- **HDD viewport bursts** (§11.6): navigation-triggered ioctl bursts make
  _browsing_ acquire IO cost on spinning media — the regression
  browse-during-scan was built to avoid — needing a media-aware throttle
  that reuses the scan's rotational detection. More scheduler to buy back
  a problem B does not create (B's single background pass is trivially
  rate-limitable and never coupled to keystrokes).

## Survived

- **The semantics** (§2) are sound and are **B's**, verbatim: exclusive
  bytes = Σ `fe_length` over `SHARED`/`UNKNOWN`/`DELALLOC`-unset extents
  at `nlink == 1`; the LCA rule for fully-seen hardlink groups; the
  five-bucket oracle; no `FIEMAP_FLAG_SYNC`. The research
  (`freeable2-research.md` §1–4) backs every clause. None of this is a
  C advantage — C inherits it and §2 says so.
- **The oracle** (§5) is sound and is A/B's oracle unchanged, including
  the good decision to **never reuse mapper data** for action-grade
  numbers ("the mapper's data is navigation-grade only … the oracle
  always re-FIEMAPs"). This correctly firewalls the freshness problem
  (research §1, 14 µs unlink→clear) out of the action path — but it also
  means C's ambient floor buys _nothing_ for the moment that actually
  matters, which sharpens SERIOUS-3.
- **"A partial exclusive sum is a lower bound"** is true **in the
  absence of concurrent deletion** — the pure math (Σ over a subset of
  `SHARED`-unset, `nlink==1` files ≤ Σ over all ≤ truth) holds. FATAL-2
  is not a flaw in the arithmetic; it is that C's own streaming-mapper +
  removal concurrency violates the precondition the arithmetic needs.
- **Tier model** (§8) and **dump/diff isolation** (§9) are B's, correct,
  and uncontested — including the right calls on ZFS (tier Z, show
  nothing) and the total privilege wall (no `TREE_SEARCH_V2`).
- **The +8 MB coverage vec is honestly priced** (§3): C states it carries
  more RSS than B, up front. Credit for candour; it is still a cost B
  doesn't pay for a benefit that's inert on ext4.

## Amendments that would make it correct (not cheap)

If pursued despite the verdict, all are mandatory:

1. **Bridge the mapper channel into the render loop's wait** (self-pipe/
   eventfd registered beside the terminal fd, or migrate to
   `crossbeam::select!` over terminal-events + channel), so the trickle
   is genuinely channel-woken and the loop can idle between deltas —
   otherwise FATAL-1 stands and the battery pitch is false. This is a
   rework of `ui.rs`'s core `event::poll` primitive (`ui.rs:506-554`),
   not in C's phasing.
2. **Epoch-tag every floor delta and validate on fold**: reject deltas
   whose target node is tombstoned (`tree.rs:462-464`) or whose epoch was
   superseded (`state.rs:1125-1131`), and reconcile the partial
   `apply_removal` subtraction against late arrivals — otherwise FATAL-2
   stands and the floor overstates. This is the streaming-accumulator
   analogue of the one-shot `(fingerprint, epoch)` guard, which the
   codebase does not currently have.
3. **A discovery-preserving scheduler**: viewport priority must not
   starve the largest-floor-first breadth pass, or C answers "where I'm
   looking" instead of "where the space is" (SERIOUS-4). Any fix that
   guarantees breadth coverage converges toward B's global ordering is,
   by construction, most of B.
4. **Per-row (or per-chunk) staleness**, not one `computed_at_last`, so
   the ambient timestamp doesn't lie about minutes-old rows (SERIOUS-5).
5. **Media-aware throttle** coupling mapper IO to rotational detection so
   navigation on HDDs doesn't acquire IO cost (§11.6) — more scheduler.

Once 1–5 exist, C's scheduler is a large parallel subsystem with two
former-fatal races guarded by new code, delivering — on ext4, nothing;
on btrfs past ~2 min, B's exact end state; in the short-session corner, a
patchier and non-deterministic version of B's display at a worse battery
profile. At that point B delivers the same honest floor for less surface,
without the overstate race, and without the never-quiescent loop.
**Kill for phase 2; ship B (ambient floors where discovery wants them) or
A (no ambient number at all). C's viewport-first bet loses to both.**
