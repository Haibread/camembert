# Freeable phase 2 — options dossier for the co-design session

**Status: draft — awaiting adversarial review and the co-design
session.** Synthesizes the research pass
([freeable2-research.md](freeable2-research.md)) and three design
proposals pushed to their limit
([A: selection oracle only](freeable2-option-a-selection-oracle.md),
[B: eager floor + oracle](freeable2-option-b-eager-floor.md),
[C: viewport trickle + oracle](freeable2-option-c-viewport-trickle.md)).
Addresses HANDOFF "Suggested next steps" §1 (btrfs
`FIEMAP_EXTENT_SHARED` + hardlink siblings), the reserved in-bar bright
segment ([tui-design.md](tui-design.md) reservation 2), and the debt
phase 1 explicitly left open: [freeable-decisions.md](freeable-decisions.md)
D8 reserved "their own per-entry channel" for these sources, and
[freeable-attack-b.md](freeable-attack-b.md) is the standing argument
this dossier must answer, not dodge.

## Problem statement

The lie phase 2 corrects: every du-style tool (camembert's own tree
included) shows an entry's *size* as if deleting it would free that
much. On btrfs/XFS, reflinks, snapshots and dedup make that false —
a shared extent frees nothing until its last referencer goes; a
hardlinked inode frees nothing until its last link goes. The research
confirmed the mechanics end to end, unprivileged: the `SHARED` bit
covers reflink/snapshot/dedup identically (§1), per-file FIEMAP costs
~1 s per 100k files with a fragmentation-driven tail (§1), and
physical-address correlation within a known file set reproduces
`btrfs fi du`'s exclusive/set-shared split without root, scoped to
scanned files (§3). What remains is design: **which honest number
appears on which surface, when is it computed, and how does it answer
the non-additivity argument that killed per-directory freeable in
phase 1.**

## The non-additivity answer (the heart, shared by every option)

Attack B established: freeable-on-delete is a function of *which nodes
are co-selected* — `freeable(A ∪ B) ≠ freeable(A) + freeable(B)` — so
an additive per-directory scalar over-counts (the snapA/snapB +90/+90/
+180 example) and a per-dir "delete-this-alone" scalar doesn't sum.
Phase 1 concluded no per-entry number could be shown. Phase 2's new
element is that **two quantities survive the attack**, one per
register:

1. **The exclusive floor** (ambient, per-entry): bytes in extents
   referenced by no one else (`SHARED` unset) on single-link inodes,
   plus wholly-contained fully-seen hardlink groups landed at their
   LCA. Additive *by construction* — every extent has exactly one
   scanned owner, so child sums equal the parent figure with zero
   double-counting — and its only error direction is understatement.
   Displayed with `≥` semantics under the label **excl** (deliberately
   `btrfs fi du`'s own vocabulary). The snapshot counterexample
   renders 0 / 0 / 0 — three true statements instead of three lies.
2. **The selection oracle** (exact, at action time): a fresh FIEMAP
   pass over one fixed marked selection with per-device physical-
   interval correlation (§3) and the hardlink subset check (§4),
   reported as a range — "frees **at least** X (exclusive), **up to**
   X+Y (shared only within your selection, as far as this scan can
   see)" — with what won't be freed named and reasoned, and an
   explicit unknown bucket. Never cached across selections, never
   compared across rows, stamped with `(selection, deletion-epoch)`.

The floor answers "where can I reclaim?" without ever over-promising;
the oracle answers "what does deleting *this* free?" exactly where the
user acts (basket, confirm modal — which finally corrects phase 1's
admittedly-optimistic hardlink freed estimate). Every option below
ships the oracle; they differ on whether/when the ambient floor
exists.

## Common ground (all options, settled by the research)

- Ground truth: FIEMAP with the mandatory pagination loop; no
  `FIEMAP_FLAG_SYNC` ever (7.3× cost, unbounded writeback tail —
  delalloc extents go to an honest *unknown* bucket instead, §1).
- Unit honesty: all figures are allocated **logical** bytes — the same
  unit as the existing `disk` column, which on compressed btrfs shares
  the same logical-not-physical blind spot it has had since the scan
  engine was built (§2). Phase 2 *words* the gap (one caveat line on
  `compress`-mounted devices, detected via mountinfo), it does not fix
  it — fixing it is impossible unprivileged (`TREE_SEARCH_V2` is
  EPERM, §3).
- Hardlink rule (§4): an inode's bytes count toward an entry only when
  the entry contains **every link the scan saw** and the scan saw
  **every link that exists** (`group.len() == st_nlink` — both facts
  already in the registry). Out-of-scan siblings ⇒ never counted,
  named in oracle output.
- Filesystem tiers, per unique `st_dev` (one `fstatfs`, the
  `classify_mount` idiom, magics §5): **F** btrfs/XFS → full extent
  machinery (`EOPNOTSUPP` downgrades); **H** ext family + other real
  fs → hardlink-only, near-free, exact on ext4 (no reflink exists
  there); **Z** ZFS → nothing plus one honest line ("block cloning is
  pool-level; no per-file API — no figure rather than a guess", §5;
  the hardlink fallback is withheld on ZFS because cloning could make
  it overstate).
- Oracle contract: per-device interval correlation at 4 KiB
  granularity; the invisible-external-referencer limit stated in the
  output (bucket 2 is "up to", never promised — §3's boundary);
  advisory async fill-in in the confirm modal, never blocking (D6);
  auto up to ~50k files, explicit with cost estimate above.
- Post-scan only, off the UI thread, chunked read-locks on the
  `Arc<RwLock<ScanOutcome>>` (the filter-fold idiom) so deletions
  never wait long; results epoch-guarded (query D5 pattern).
- Nothing in dumps (extent sharing is *more* volatile than `/proc`
  state — any process's write anywhere flips it, §7 — phase-1 D7's
  capability argument verbatim), nothing in diff, nothing in the
  32-byte `Node` (D8 isolation; side maps only).
- No new gauge headline: the root's floor approximates the whole tree
  (meaningless as a "freeable" banner), so D8's "gauge sums both"
  stays satisfied by phase 1's suffix alone; phase 2 is row-scoped.

## The real axis

What ambient layer exists above the shared oracle, and when it is paid
for:

| | A — oracle only | B — eager floor | C — viewport trickle |
|---|---|---|---|
| Ambient per-entry number | none | floor, whole tree | floor, where mapped |
| Evaluation model | on-demand per selection | one full pass at scan end, off-thread, trickle-published | demand-driven: viewport first, idle trickle fills |
| Reservation-2 segment | measured rows only (selection) | every row, once floors land | progressive, with per-row coverage tip |
| Cost at 100k / 10 M files | 0 until asked | ~1 s / ~2 min background | same totals, paid where/if looked |
| Memory at rest | ~0 | ~48 MB @ 10 M | ~56 MB @ 10 M (+ coverage) |
| Staleness surface | none (always fresh) | computed-at on floors; oracle fresh | per-row coverage % + computed-at; oracle fresh |
| "Where can I reclaim?" | unanswered ambiently | answered (dark segment = shared/cold lead) | answered where the user has been |
| Sort by excl | no axis exists | yes (complete floors, `≥` header) | refused (partial floors would rank coverage) |
| Determinism of display | per-selection | full and reproducible | navigation-history-dependent |
| New moving parts | oracle engine | + one pass + FloorMap + removal lockstep | + scheduler, cancellation, coverage bookkeeping |
| Answer to attack-b | no ambient scalar at all | additive floor that can only understate | same floor, partial-but-valid at all times |
| Main risk | thesis's UI home stays dark; no discovery | RSS + default-on 2-min churn at 10 M | complexity + patchy/non-deterministic display |

## Where each option genuinely wins

- **A** wins purity and price: zero ambient state, zero staleness,
  zero flags, and it is literally slice 1 of the other two — nothing
  it builds is ever unbuilt. Loses discovery: a snapshot-heavy tree
  gives no ambient signal, and tui-design's reserved segment stays
  dark until the user marks something.
- **B** wins the thesis surface: every bar shows its guaranteed
  fraction, the one ambient number that cannot overstate, complete and
  reproducible; sorting on it is defensible for the first time; floors
  compose with flat/breakdown/filter by plain summation. Loses on
  cost: +48 MB at the 10 M target and a default-on background pass
  that takes ~2 minutes there (~1 s at 100k — the typical case).
- **C** wins the big-tree short-session case — pay only where the user
  looks, converge on B's end state when idle. Loses simplicity and
  display determinism: a scheduler with cancellation and per-row
  coverage where B has one pass, segments that fill in under the eye,
  and a patchy landscape a short session never completes.

## Recommendation to challenge in session

**Option B — eager floor + selection oracle — implemented oracle-first
(slice 1 is exactly Option A), with `--no-fiemap` as the opt-out.**

Reasons, in thesis order:

1. **Only B fills reservation 2 with a number that cannot lie.** The
   in-bar bright segment was reserved as "the UI home of the
   libérable ≠ taille thesis"; a floor that is additive, guaranteed,
   and complete is the only candidate that survives attack-b *and*
   actually renders on every row. A leaves the thesis's home dark; C
   renders it patchily and non-deterministically.
2. **The floor's error direction is the one the product can keep.**
   "You will free at least this" is a promise; both failure modes that
   killed phase-1 option B (over-count, wrong-row authority) are
   structurally impossible for exclusive bytes. The under-sell on
   snapshot farms is real and disclosed — and corrected by the oracle
   at the exact moment it matters.
3. **The cost is honest and bounded.** ~1 s per 100k files off-thread
   at scan end is the phase-1 sweep pattern scaled up; trickled floors
   are valid at every instant (monotone), so nothing waits on
   completion. The 10 M worst case (~2 min background) is the price of
   the design target, opt-out-able, and a named session decision
   (below) rather than a buried default.
4. **C's savings don't buy its complexity.** The idle trickle
   converges on B's full pass in any session longer than the pass
   itself; C's win is confined to short sessions on huge trees, paid
   for with a scheduler, coverage bookkeeping, and the first
   navigation-history-dependent display in the product.
5. **Phasing contains the risk.** Slice 1 (the oracle at the delete
   path) is small, fixes a known phase-1 dishonesty (the optimistic
   hardlink freed estimate), and is common to all three options — the
   session can green-light it even while fighting about the ambient
   layer.

## Decisions needed in the co-design session

Each with the dossier's recommendation marked; the attack reports may
move these.

1. **The two-quantity model** — ambient *exclusive floor* (`≥`,
   additive, understates only) + action-time *oracle range* — as the
   answer to attack-b. **Rec: accept**; it is the load-bearing frame
   for everything below.
2. **Ambient tier: B (eager) vs C (trickle) vs A (none).** The axis
   table above. **Rec: B**, largest-dirs-first ordering, monotone
   trickle publishing.
3. **Eager-pass default**: on for tier-F devices with `--no-fiemap`
   (env `NO_FIEMAP`) opt-out, no size threshold — or auto-skip above
   N files with a "press to map" hint? **Rec: default-on, no
   threshold** (off-thread, cancellable, progress-lined); revisit with
   field data on the 10 M/2-min case.
4. **Hardlink LCA contributions in ambient floors** (groups wholly
   inside a subtree land at their LCA) with the partial-group-deletion
   retraction rule — or the degenerate variant (multi-link inodes
   ambient 0, oracle-only)? **Rec: LCA rule** (it is the honest
   generalization and the registry already holds the data); the
   degenerate variant is the documented fallback if the attack pass
   breaks the retraction invariant.
5. **`SortKey::Exclusive` + `excl` column** (attack-b finding 2
   re-litigated with a new element: a guaranteed floor is not a
   best-effort guess; header carries `≥`). Known trap: a 90 GiB
   snapshot pair sorts to the *bottom* (floor 0), hiding the biggest
   opportunity. **Rec: defer to slice 3** and decide with the attack
   reports on the table — ship segments + card first, sort later if
   it survives.
6. **Compression honesty**: figures stay in allocated-logical bytes
   (the `disk` column's unit); one caveat line on `compress`-mounted
   devices at oracle/card surfaces; the pre-existing `disk`-column
   blind spot is named in docs as out of scope for phase 2. **Rec:
   accept** — the alternative (a root-only true-bytes mode) is
   decision 7.
7. **Root-only precision mode** (`TREE_SEARCH_V2` compressed truth,
   `LOGICAL_INO` referencer lists, mirroring phase-1 D6's coverage
   honesty): design-reserve or reject? **Rec: reject for phase 2**
   — the privilege wall is total on the reference machine (§3), the
   n=1 distro data point cuts against relying on anything beyond bare
   FIEMAP (§3, open question 6), and a root mode forks every figure
   into two truth levels. Reopening needs a new element (e.g. an
   unprivileged kernel API or a second-distro datapoint changing §3).
8. **ZFS tier Z wording** ("ZFS does not expose per-file sharing — no
   figure rather than a guess") and the deliberate withholding of the
   hardlink fallback on ZFS (block cloning could make it overstate).
   **Rec: accept** — it is the settled "show nothing rather than
   invent" stance, now on stronger research grounds (§5).
9. **Oracle auto-run cap**: automatic up to ~50k selected files
   (≈ 0.5 s), explicit key with cost estimate above. **Rec: accept**,
   with the cap and estimate marked as measured-on-one-machine and
   recalibrated in the field (option A press-point 4).
10. **Confirm-modal integration**: the oracle range replaces phase-1's
    optimistic freed estimate; advisory async fill-in, never blocks
    (D6). **Rec: accept** — this is slice 1 and the single clearest
    honesty win.
11. **Dump/diff surface: none** (volatility + D7 capability argument);
    CLI surface: `--no-fiemap`/`NO_FIEMAP` only, documented in
    `--help` + README in the same change. **Rec: confirm.**
12. **Phasing**: slice 1 oracle-at-delete-path (common to all
    options) → slice 2 eager floor + segments + card + flag → slice 3
    composition (sort/column if accepted, flat/breakdown/filter floor
    sums, bucket-3/4 refinement). **Rec: accept** — slice 1 can start
    while the ambient-tier attack reports are still being fought over.
