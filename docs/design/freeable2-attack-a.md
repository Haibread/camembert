# Adversarial review — Option A (selection oracle only)

> Verdict: **SURVIVABLE WITH AMENDMENTS — but the flagship surface is
> undeliverable as specified.** The correctness core is sound and
> research-backed, the isolation claim holds in code, and the "no scalar
> per entry exists" answer to non-additivity is genuinely the cleanest of
> the three. What does *not* survive is the confirm-modal contract: A's own
> "advisory, never blocking" escape hatch guts the one surface it calls its
> biggest win, on exactly the large selections phase 2 exists for. That is
> a redesign, not a footnote. Two more serious holes (the "frees at least"
> verb on compressed mounts, and the thesis's dark UI home for directories)
> land on top. A is the honest option *for small selections*; it oversells
> itself as the honest option full stop.

## The through-line flaw

A's pitch rests on one move: *compute the exact answer at the moment
freeable matters — selection, marking, delete-confirm.* The confirm modal
is named the flagship (§5, §7: "the single biggest user-visible win, and
it ships in slice 1"). But "the moment it matters" for a consequential
delete is a *large* selection, and A's cost table (§9) prices a 1 M-file
marked dir at **~10 s** and a 500k-file dir at tens of seconds. A already
sees this and reaches for the escape hatch: §4 says the modal is "filling
the modal asynchronously — **advisory, never blocking confirmation** (D6
precedent)."

Follow that to its consequence in the actual code and the feature
evaporates precisely where it was sold.

Verified against the code:

- `open_delete_confirm` (ui.rs:1712) computes **everything synchronously**
  — `hardlink_files_in` and `pre_deletion_open_warning` — *before* calling
  `ui.open_confirm(hardlinks, open_warning)` (ui.rs:1730).
- `ConfirmState` (state.rs:139) is **immutable**: two fields,
  `hardlink_files` and `open_warning`. No channel, no epoch, no
  "computing" slot.
- `draw_confirm_modal` (ui.rs:3522) renders **purely** from that immutable
  `ConfirmState`. The confirm modal has never updated after it opens.
- While the modal is up, `handle_key` (ui.rs:1096–1105) routes **every**
  key to it: `y` calls `execute_deletion` *immediately*; **any other key
  cancels**. There is no wait state.

So "fill the modal asynchronously" is not a small addition to an existing
pattern — it is a **new machine that does not exist**: a mutable oracle
slot on `ConfirmState`, a receiver polled in the render loop with a
(fingerprint, epoch) guard, a `needs_frequent_polling` clause, a
redraw-on-arrival path, and a "computing…" render state — plus thread
cancellation so a cancelled modal doesn't leave tens of seconds of ioctls
running against a modal that's already gone. The two precedents A cites are
the wrong shape: the **filter fold** (ui.rs:876+) updates the *main table*,
never a modal; and **D6's open-warning** — the one precedent that touches
*this* modal — is deliberately **synchronous** (ui.rs:1758–1761: "a modal
open blocking for one sweep's worth of time (~25 ms) … is a fair trade").
A invokes "D6 precedent" for "advisory, never blocking" while needing the
exact *opposite* of D6's actual synchronous implementation.

And the semantics A borrows from D6 are what kill it. "Advisory, never
blocking" means `y` works the instant the modal opens (ui.rs:1100). On a
500k-file marked dir the oracle has not landed, so the user either:

1. stares at "computing…" for tens of seconds — the confirm flow now has a
   multi-second dead zone that D6's 25 ms sweep never introduced; or
2. presses `y` and deletes with **no range shown at all** — the flagship
   honest-range surface is *absent* exactly when the selection is biggest
   and the decision most consequential.

This is the same fail-open race the sweep-option review flagged for the
open-file warning (freeable-attack-a.md finding [4]) — but strictly worse.
That warning is 25 ms and races almost never; A's oracle is
seconds-to-tens-of-seconds and races **almost always** on the large
selections that motivate a careful delete. A's biggest claimed win is a
coin flip whose odds get worse the more the number matters.

## Findings (severity-ranked)

### SERIOUS — the flagship confirm-modal range is undeliverable for large selections [1]

Detailed above. The correctness of the *number* is not in question; its
*availability at the decision moment* is. Synchronous fill freezes the UI
for tens of seconds (violating slice-5 quiescence and every latency budget
in the codebase — the D6 comment's own ~50 ms threshold). Asynchronous
fill requires unbuilt modal-refresh machinery **and** hands the user a
`y` key that confirms before the result exists. Either way, "the exact
answer at the moment it matters" is not what ships for the deletions that
matter most.

Concrete: mark `~/.cache` (say 400k files across a browser profile, a
package cache, and build artifacts), press `D`. Under the synchronous
reading the TUI is wedged for ~5 s with no paint. Under the async reading
the modal says "computing…"; the admin who does this daily hits `y` on
muscle memory at t+300 ms and the "frees at least X, up to Y" line — the
thing that was supposed to replace phase-1's optimistic estimate — never
rendered. The optimistic estimate A set out to kill is replaced by
*nothing*.

Amendment (mandatory, and it is a redesign): decide explicitly what the
modal shows when the oracle is not ready. Options, none free: (a) a
**blocking spinner with a hard cap** (compute for ≤ ~200 ms, then show
"selection too large for an exact figure — deleting N entries, M on disk"
and let the user proceed on the coarse figure); (b) **pre-compute in the
basket** so the modal only ever *displays* an already-settled result, and
refuse to open the modal until the basket oracle has landed (turns the
race into a basket-level wait the user controls); (c) accept that for
selections above the auto-cap the confirm modal shows the same size-only
information it does today and *say so*. What A cannot do is claim the
honest range as a shipped slice-1 win while its delivery mechanism is
"advisory, never blocking" over a tens-of-seconds computation.

### SERIOUS — "frees at least X" is an overstatement on the dev machine's own default mount [2]

A's headline verb is a **lower bound**: "frees at least ⟨1⟩" (§2 bucket 1,
guaranteed = extents with `SHARED` unset). But bucket-1 bytes are summed
from `fe_length`, which the research establishes is the **logical
(uncompressed)** extent length on a compressed file — the kernel does not
expose the on-disk compressed byte count to unprivileged FIEMAP
(research §2, confirmed live: `btrfs fi du` reports full logical bytes for
a 30:1-compressible file). The reference machine mounts `/home`
`compress=zstd:1` (research header). So on the *very filesystem this
feature was researched on*, "frees at least 8 GiB" can correspond to a
real reclaim of a few hundred MB. A lower-bound claim that overstates the
floor by up to the compression ratio is not a lower bound — it is the
optimistic estimate A's whole pitch says it refuses to ship.

A's mitigation (§5 "Compression caveat") is a footer line — "physical
reclaim may be smaller" — that **directly contradicts the headline verb**
sitting two lines above it. "Frees at least X … but actually maybe much
less" is incoherent on its face. The caveat is worded, not fixed; and
because the same `st_blocks`-based blind spot already lives in the `disk`
column (research §2, open question 3), A's "same unit as the disk column"
defense means the oracle *inherits* the overstatement rather than
containing it.

Amendment: on any `compress`-mounted device, either drop the "at least"
framing entirely (say "frees up to X, physical reclaim not observable
unprivileged") or gate the guaranteed floor behind the root-only
`TREE_SEARCH_V2` path A explicitly rejects (§6) — in which case
unprivileged runs on the majority btrfs configuration have *no* true
lower bound to show, which is itself a finding about how much of A's
contract survives without root.

### SERIOUS (product) — the thesis's UI home is dark exactly where the story lives [3]

A self-identifies this (§11.1, §11.2) as a "press point"; verification
says it is heavier than that. The motivating scenario — "a snapshot-heavy
tree, 90 % of a subtree is shared, where can I reclaim?" — is a
**directory-level** question. A shows *nothing ambient on directory rows*:
the selection card auto-computes only **file** rows (§4), and the reserved
in-bar bright segment lights only on rows the oracle "actually measured
this epoch" (§5) — i.e. marked rows and the measured selected file row.
The one place the libérable-≠-taille thesis has to surface to be
discovered — a directory bar that looks suspiciously dark because its bytes
are mostly shared — is precisely where A renders nothing until the user
already suspects the answer and marks it.

Contrast with the sweep option, which at least paints a scan-end toast
(freeable-attack-a.md finding [7] and freeable_panel::should_toast,
ui.rs:635). A has no ambient hook at all for the aggregate case: no
column, no sort key, no dir-row segment, no toast (there is nothing to
toast — no number exists until a selection does). "Is a thesis whose UI
home is empty until the user marks something actually surfaced?" (§11.1) is
the right question, and on the evidence the answer is *no, for directories*
— which is where the thesis is true and interesting. This does not make
the numbers wrong; it makes the feature findable only by users who already
know to look, which is close to defeating the point of building it.

Amendment: A needs *some* ambient directory-level signal, which forces one
of the exact things A prides itself on refusing — a cheap per-dir lower
bound (a shared-extent hint), or at minimum a dir-row "some of this is
shared — press x" affordance that is not gated on the user first marking.
Every such amendment reintroduces a fraction of what A killed, and that
tension (purity vs discoverability) is the honest cost of the
selection-only stance, not a detail.

### MODERATE — selection-card `x` on a directory under a filter reproduces the exact trap FilterActive exists to prevent [4]

A dismisses the filter interaction in one line (§5: "the oracle follows the
basket … No per-group or per-filtered-dir floors exist to compose —
nothing to do"). That is true for the **basket** — but only because
`MarkRefusal::FilterActive` (state.rs:114–120) *forbids marking a directory
under an active filter* precisely to avoid the "42 MB shown / 300 GB
deleted" mismatch. A's own new surface reopens the hole the basket closed:
the selection card offers dir rows "press x for exact freeable" (§4, §5),
and nothing about a filter suppresses the cursor landing on a dir row. Press
`x` on a directory while a filter is active and the oracle FIEMAPs the
**whole subtree** — matched and unmatched files alike — showing a freeable
figure for content the filtered table is actively hiding. That is the
inverse of the same trap, on the same rows, that FilterActive was written
to prevent.

Amendment: under an active filter, the dir-row `x` affordance must either
be refused (mirroring FilterActive) or scoped to the filtered subset (extra
intersection machinery the doc does not mention). "Nothing to do" is wrong.

### MODERATE — auto-oracle on cursor motion fights slice-5 quiescence, and the µs rate is a lab number [5]

A's auto-figure for file rows (§4) runs "one open+FIEMAP+close … debounced
with cursor motion." Verified cost of that promise against the loop: the
UI is deliberately quiescent — `IDLE_POLL = 3600 s` (ui.rs:121), and the
loop only polls at `FRAME` (33 ms) when `needs_frequent_polling` says so
(ui.rs:764–779, keyed on scan/motion/flash/toasts/sweep/palette). An
auto-oracle needs its own clause there (debounce pending *or* oracle in
flight) or the card never updates; adding it means **every cursor
keystroke onto a file row flips the UI from 0-CPU idle into 33 ms-frame
polling** for the debounce+oracle window. That is a novel per-navigation
side effect on the hot path, exactly the concern A raises about itself
(§11.3) and does not resolve.

Worse, the "6–15 µs, imperceptible" figure (§9) is one warm-cache btrfs
SSD (research §1). On a cold cache the first FIEMAP forces btrfs backref
resolution the scan never did (research §3: `btrfs fi du` is 6–7× slower
than `du` *because* of the per-extent SHARED check), and on a network
filesystem a single open+ioctl is milliseconds, not microseconds — so
per-keystroke card figures become visibly laggy on precisely the
mounts (NFS, slow HDD) where users most want to know what's reclaimable.
The card figure needs a hard latency budget and a graceful "measuring…"
state, not an unqualified "instant."

### MODERATE — the range is honest at compute time, not at deletion time [6]

A frames freshness as its "quiet superpower" (§4): "never shows a number
older than the last debounce window." True at *computation* time — but the
decision the number informs is the deletion, and deletion is not
instantaneous. For a 500k-file selection the unlink syscalls themselves
take seconds, during which any external write, snapshot, or `duperemove`
run (research §7 — the same volatility A cites as its strength) can flip a
bucket-2 "up to" extent into pinned-forever, or a bucket-1 "guaranteed"
extent into shared. The oracle's range was honest when computed and stale
when reclaim actually happened. This is inherent and shared with every du-
class tool — but A *specifically* sells "fresh at computation time" as
eliminating staleness ("there is nothing stale to display"), which
overclaims: it eliminates staleness *of the displayed number*, not of the
number's relationship to the reclaim the user is about to trigger. Worth
one honest sentence rather than a superpower.

### MODERATE — marks carry no identity; "extract (dev, ino, nlink, disk)" hides a tree walk and a re-stat [7]

§4's execution sketch says the thread extracts "paths + `(dev, ino, nlink,
disk)` for ~10k files." Verified: `MarkedEntry` (state.rs:93) carries only
`{ node, path, is_dir, disk }` — **no `(dev, ino)`**, and a marked
directory is a *single* entry "its subtree implied, there are no
per-descendant marks" (state.rs:91). Tree nodes also drop `(dev, ino)`
after scan time (freeable.rs D8 note; ui.rs:1738–1740: "Tree nodes don't
carry (dev, ino) past scan time … `Node` is a packed 32 bytes"). So the
oracle cannot "extract" `(dev, ino)` — it must (a) walk the frozen tree
under the RwLock read guard to expand each marked dir into its descendant
file nodes (the `hardlink_files_in` walk at delete.rs:247–275 is the
existing template), (b) rebuild each path, and (c) **open+FIEMAP each file
fresh**, which is also where it re-learns `(dev, ino)`. That fresh open is
a second TOCTOU surface (the file may have changed or vanished since scan —
handled as bucket 5, fine) and it is the real per-file cost, not a cheap
field extraction. The design is buildable, but §4 undersells the work as a
field read when it is a subtree expansion plus a syscall per file.

Note the one claim here that *survives*: A relies on the hardlink registry
holding both `group.len()` and `st_nlink` (§2, §5). Verified — `ScanOutcome`
retains `hardlink_links: Vec<HardlinkLink>` and exposes it via
`hardlink_links()` (scan.rs:612, 734), and `HardlinkLink` carries `dev,
ino, nlink` (hardlink.rs:24–29). The `(dev, ino)` grouping in
`reattribute` (hardlink.rs:35) is the template for the per-selection subset
check. The hardlink-range semantics A describes are implementable from data
that genuinely exists post-scan. Credit where due.

### MINOR — "zero memory" is true; "zero cost" is the claim that matters and it isn't [8]

The `OracleResult` is `O(|S|)`, transient — the "memory at rest: 0" row
(§9) is accurate for camembert's own RSS. But repeated marks re-FIEMAP the
whole selection every debounce window (A's own §11.6), and each FIEMAP
forces btrfs backref resolution (research §3). A user toggling marks inside
a 50k-file dir pays ~0.5 s of open+ioctl+backref *per toggle*, hammering
the kernel dentry/inode caches and evicting other page-cache — none of
which shows up in camembert's RSS but all of which the user feels. "Zero
memory at rest" is a narrow true claim standing in for a "cheap" impression
the repeated-work pattern does not earn. The self-identified incremental
per-(dev,ino) cache (§11.6) is the fix, but as A notes it reintroduces the
staleness A's superpower is having none of — so the honest position is
"repeated marks are not free," stated, not a zeroed table cell.

## What survived the attack (genuinely)

- **The non-additivity answer is the cleanest of the three.** "No scalar
  per entry exists" (§2) is not a dodge — it is the correct structural
  reading of research §3/§4, and it means A never renders
  `freeable(A)` and `freeable(B)` as comparable static columns, so the
  trap freeable-attack-b killed cannot recur. This is A's strongest move
  and it holds.
- **The unprivileged FIEMAP-correlation core is real and root-free.**
  Research §3 confirms `btrfs fi du`'s own set-shared math over a known
  file set works unprivileged; A's bucket model (exclusive / selection-
  shared / held-elsewhere / shared-outside / unknown, §2) is a faithful
  and honest encoding of what that primitive can and cannot see, including
  the merged 3/4 bucket A correctly identifies as unavoidable without a
  scan-wide extent map.
- **The isolation claim is true in code.** A new `freeable2.rs` needs
  nothing from `Node`, `tree.rs`, snapshots, dump, or diff (§3), and the
  post-scan data it consumes (`hardlink_links()`, the RwLock read guard,
  `live_dir_paths`) already exists and is already accessed exactly this way
  by the filter fold (ui.rs:881+) and the open-warning (ui.rs:1762). The
  spawn+channel + chunked-read-guard execution model is a real, in-tree
  pattern.
- **Delalloc handling is honest.** Using FIEMAP *without* `FLAG_SYNC` (§2
  bucket 5) means dirty/unallocated extents report `UNKNOWN`/`DELALLOC` and
  fall out of every figure (research §1) — a genuine lower bound, no
  overstatement, and it dodges the 7.3× cost and unbounded writeback tail.
  This is the right call and it is correctly specified.
- **The ext4 / ZFS tiers are sound.** ext family → hardlink-only, extents
  "no sharing" (research §5: no reflink); ZFS → refuse rather than offer a
  tier-H figure block cloning could falsify (research §5, `openzfs#16024`).
  Both match the research and the "show nothing rather than invent" stance.
- **TOCTOU with a concurrent deletion is a non-issue.** The chunked read
  guard (drop the lock before ioctl) means a confirmed deletion's write
  lock never waits on a long selection, and the epoch bump
  (`bump_flat_epoch`, ui.rs:1886) invalidates a stale `OracleResult` by
  construction — "nothing ambient to keep consistent" (§7) is vacuously
  true and correct.

## Verdict: SURVIVABLE WITH AMENDMENTS — flagship surface excepted

A's skeleton is honest, its blast radius is as small as advertised
(confirmed in code), and its answer to non-additivity is the best of the
three options. It is not killable at the architecture level. But it does
**not** survive as written:

- Finding [1] is not a wart to patch — it is a structural mismatch between
  A's central promise ("exactness at the moment it matters") and the fact
  that *the moment that matters most is a large, slow selection*. A's own
  escape hatch ("advisory, never blocking") converts its flagship win into
  a fail-open race that gets worse as the number gets more important. This
  must be **redesigned** (blocking cap, or basket-precomputed modal, or an
  admitted size-only fallback), not footnoted.
- Finding [2] makes A's headline verb ("frees at least") false on the
  majority btrfs configuration — including the one it was researched on.
- Finding [3] leaves the thesis undiscoverable for the directory-level
  case that is the whole reason the thesis is interesting.

Required amendments, in severity order:

1. **Redesign the confirm-modal contract.** Pick and specify what shows
   when the oracle isn't ready; do not ship "advisory, never blocking" over
   a tens-of-seconds compute. [1]
2. **Drop or root-gate the "at least" floor on `compress` mounts;** stop
   the caveat line from contradicting the headline. [2]
3. **Give directories an ambient discovery hook** (a dir-row shared hint or
   an ungated "press x" affordance), accepting that this reintroduces a
   sliver of the per-dir signal A prides itself on refusing. [3]
4. **Refuse or filter-scope the dir-row `x` oracle under an active filter**,
   mirroring `MarkRefusal::FilterActive`. [4]
5. **Budget the auto-card oracle** (latency cap, "measuring…" state, a
   `needs_frequent_polling` clause) and re-price it for cold-cache / NFS /
   HDD, not one warm SSD. [5]
6. State the compute-time-vs-delete-time gap [6]; correct §4's "extract
   (dev, ino, …)" to "walk the subtree and open each file" [7]; and stop
   letting "0 memory at rest" stand in for "cheap" [8].

With 1–3 done, A is the honest, minimal option it claims to be — for
selections small enough that "exactness on demand" is actually on demand.
Without them, its flagship surface is absent exactly when it matters, its
lower bound isn't one, and its thesis has no home on screen until the user
already knows the answer.
