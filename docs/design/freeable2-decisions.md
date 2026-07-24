# Freeable phase 2 — decisions (2026-07-23, delegated session)

Outcome over the options dossier and the three
attack reports (condensed below in this file's own "Condensed
reasoning trail" section; originals in git history). **The user delegated this session's
choices ("prend les choix recommandés") — the post-attack recommended
option was adopted with the attacks' amendments folded in.** Settled;
reopening one requires a new element.

## D1 — Shape: Option B amended, oracle-first; C rejected

Two quantities, per the survey — the **selection oracle** (exact at
action time) and the ambient **exclusive floor** (additive by
SHARED-unset disjointness; the attack confirmed the proof under
bookends, self-reflinks and partial sharing). Option C is rejected
(two fatals: no channel-woken UI loop exists, and its incremental
floor can overstate during the deletion window). Implementation is
oracle-first: slice 1 ships the oracle alone (Option A's shape),
slice 2 adds the eager floor + in-bar segment.

## D2 — Units and wording: allocated-logical, "exclusive", never a lie

All phase-2 figures are **allocated-logical bytes** — the same unit as
the existing `disk` column, which shares the same compression blind
spot on btrfs (st_blocks is logical too; research §2). Vocabulary is
`btrfs fi du`'s: "exclusive" / "shared", never "you will get back
exactly X". On mounts with a `compress` option (detected via
/proc/self/mountinfo, mechanism already in scan/media.rs) every
freeable-2 surface carries one caveat line ("compressed mount:
physical reclaim may be smaller"). A floor of 0 on a nonzero file
renders as **"fully shared"**, never as an empty/absent figure (the
under-statement trap: 0-exclusive is the feature's most informative
answer, not its null case). No `SortKey::Exclusive` in phase 2.

## D3 — Ambient floor lifecycle

Computed **off-thread after scan end** (after the phase-1 sweep;
sequenced, never concurrent with it), whole-value epoch-stamped (C's
lesson: no incremental mutation), invalidated and recomputed on in-app
deletion epochs; external filesystem writes are acknowledged, not
tracked — every ambient surface shows "as of <computed-at>" and the
gauge line's tooltip/footer says external dedup/snapshots are not
watched. Opt-out `--no-fiemap` (env `NO_FIEMAP`): disables floor AND
oracle; on btrfs the bar segment then shows nothing (never the
disk-size fallback — that reintroduces the lie). Ambient floor is
gated on kernel ≥ 6.1 (SHARED false-negatives before; uname check);
the oracle still runs on older kernels with a caveat line. Memory
budget: one u64 side map per entry (~48-64 MB @ 10 M) documented.

## D4 — Selection oracle and the confirm modal

The oracle FIEMAPs the selection and buckets bytes: exclusive /
shared-within-selection (freed if the whole selection goes — the
research-validated physical-address correlation, scanned-files scope)
/ shared-outside / unknown. It runs **incrementally at mark time**
(marking is the intent signal; cost spreads across the session), so
the confirm modal usually opens with a ready figure. `ConfirmState`
gains an async slot: when the oracle is still computing, the modal
shows the size line plus "estimating actual reclaim…" with a spinner
and **updates in place** when the result lands (the modal's
never-updates contract is redesigned — attack A [1]); `y` stays live
the whole time and acts on whatever is known, the wording makes that
explicit. The modal line quantifies what phase 1 only said
qualitatively: "frees N exclusive (+ M shared within the marked set;
K shared elsewhere will not be freed)". Hardlinks: a file counts as
freeable only when all its in-tree links are inside the selection and
nlink shows no out-of-tree links; otherwise it lands in
shared-outside with the hardlink wording.

## D5 — Filesystem tiers

btrfs and xfs: FIEMAP path. ext4 & friends: hardlink-only tier (the
D4 hardlink rule, no extent claims). ZFS: no figures at all, one
honest line ("ZFS exposes no per-file sharing — nothing shown rather
than a guess"). Detection by statfs magic, reusing the scan's
existing mechanism.

## D6 — Boundaries

New `camembert-core/src/fiemap.rs` (ioctl + floor pass + oracle);
freeable.rs (phase 1) untouched except confirm-modal integration
points in the UI. No dump keys, no diff impact, arena untouched (side
maps only). Integration tests are real: reflink fixtures on the dev
machine's btrfs (cp --reflink + FIDEDUPERANGE), guard-skipped on
non-btrfs CI runners.

## Condensed reasoning trail

### Research

Gathered unprivileged on this machine's real btrfs (`compress=zstd:1`,
kernel 7.1.4). **FIEMAP mechanics** (§1): `FS_IOC_FIEMAP` is read-only,
needs a pagination loop (a fixed-size single call silently truncates);
`FIEMAP_EXTENT_SHARED` (`0x2000`) fires identically for reflinks,
snapshots, and `FIDEDUPERANGE` dedup — no way to distinguish the cause.
It clears near-instantly (14.1 µs) when the last other referencer is
unlinked, no commit wait. `FIEMAP_FLAG_SYNC` costs ~7.3× more (forces
writeback) and has unbounded tail latency — never used; unflushed data
reports delalloc/unknown honestly instead. Per-call cost ≈ 6–15 µs
warm-cache, roughly linear in extent count, ⇒ ~0.6–1.5 s per 100k files.
**Compression trap** (§2): `fe_length` on a compressed extent is the
*logical* length; neither FIEMAP nor `st_blocks`/`du`/`btrfs fi du`
exposes real compressed bytes unprivileged (only root-only
`TREE_SEARCH_V2` does) — a 200 MiB all-zero file frees ~7 MB physically
but every unprivileged tool reports 200 MiB. **Inclusion-exclusion**
(§3): the root-only ioctls (`TREE_SEARCH_V2`, `LOGICAL_INO`, even
`subvolume show/delete` on one's own subvolume) are all EPERM on this
machine. But bare FIEMAP physical-address correlation *within a known,
already-visited file set* reproduces `btrfs fi du`'s exclusive/set-shared
split unprivileged — the load-bearing finding — bounded to files the
scan itself visited; anything shared with an unvisited snapshot/subtree
stays invisible, undetectable, not double-countable. **Hardlinks**
(§4): the scan's existing `HardlinkLink` registry (`group.len()` vs
`nlink`) already answers "does the whole scan see every link"; a
per-*selection* answer needs the subset of a group inside the candidate
set compared against the full group, not against `nlink` alone — same
structural non-additivity as extents. **Other filesystems** (§5): XFS
reflinks the same way as btrfs; ZFS's Block Cloning (2.2+) is pool-level
with no per-dataset or per-file API even for ZFS's own tooling — "show
nothing" is right on stronger grounds than "no reflinks". ext4 has no
reflink, hardlink-only is exact there. **Prior art** (§6): `btrfs fi du`
itself documents the non-additivity problem and is the closest existing
unprivileged reference implementation; nothing found (`compsize`
excepted, and it needs root) does better. **Staleness** (§7): extent
sharing is volatile in a strictly stronger sense than phase 1's `/proc`
state — any process's write anywhere can flip it, invisibly. Six open
questions were handed to the design pass: inclusion-exclusion scoping
honesty, lazy-vs-eager evaluation, whether the compression gap is in
scope, whether a root-only precision mode is worth reserving, hardlink
per-selection semantics, and whether this machine's stricter-than-documented
subvolume permissions are distro-specific.

### Options

**Problem**: every du-style size implies "deleting this frees this
much," which is false wherever reflinks/snapshots/dedup/hardlinks apply.
Phase 1 already ruled out a per-directory additive scalar (attack-b's
non-additivity argument) — phase 2 needed two quantities that each
survive that argument in their own register: an ambient, additive,
understatement-only **exclusive floor**, and an exact, action-time
**selection oracle**. All three options ship the oracle; they differ on
whether/when an ambient floor exists.

- **A — selection oracle only.** Core idea: take the research's
  "freeable is a function of co-selection" finding at face value and
  ship *no* ambient number at all — compute exactly, on demand, at
  selection/mark/confirm time. Decisive pro: the cleanest possible
  answer to non-additivity (no scalar per entry ever exists to be
  wrong). Decisive con: the reserved in-bar bright segment and "where
  can I reclaim?" stay dark until the user already suspects and marks —
  the thesis's UI home is empty exactly where it matters. Lost outright,
  but its shape survives as slice 1 of the winner.
- **B — eager floor + oracle.** Core idea: the exclusive floor (bytes
  in `SHARED`-unset extents, `nlink==1`, plus fully-seen hardlink
  groups landed at their LCA) is additive by construction and can only
  understate, so it's safe to compute for every entry, once, off-thread
  at scan end, and show everywhere (bar segment, card, sort key).
  Decisive pro: fills the ambient discovery surface with a number that
  is safe in its own register and is complete/reproducible. Decisive
  con (per attack-b): "cannot lie" oversold — logical-not-physical on
  compressed mounts, stale-can-overstate after external writes, uniform
  zero on exactly the shared-heavy trees the feature targets. **Won**,
  amended: shipped as an ambient floor with explicit `≥` wording, a
  kernel gate, and no promise beyond "at least."
- **C — viewport trickle + oracle.** Core idea: same two quantities as
  B, but map extents demand-driven (viewport first, idle trickle
  behind), so a short session on a huge tree pays only for what was
  looked at. Decisive cons (per attack-c, both fatal): the "channel-woken,
  idle-quiescent" claim is false against the actual render loop (only a
  terminal-fd poll exists; a continuous trickle pins the loop at 33 ms
  for the whole session — worst on the exact short session C targets);
  and the "partial floor can only understate" invariant breaks once an
  in-flight chunk's delta lands on an ancestor after a deletion already
  subtracted that ancestor's partial floor, making the floor overstate —
  the one direction forbidden. **Rejected outright** (D1) on both fatals;
  none of C's letter survives beyond "the mapper idea is not worth its
  concurrency."

**Recommendation adopted** (dossier §"Recommendation", accepted by the
delegated session): Option B, implemented oracle-first — slice 1 ships
the oracle alone (Option A's shape), slice 2 adds the eager floor and
in-bar segment. `SortKey::Exclusive` was deferred out of phase 2
entirely (attack-b's sort-authority trap against a `≥0` snapshot pair
sorting to the bottom was judged not worth re-litigating yet).

### Attack findings

#### Attack A (Option A — selection oracle only; verdict: survivable with amendments, flagship surface excepted)

1. **[1] Flagship confirm-modal range undeliverable for large selections** — "advisory, never blocking" leaves the modal either frozen for tens of seconds or offering `y` before any range renders, exactly on the large deletions where the range matters most. Resolved: D4 redesigns `ConfirmState` with a mutable async slot — the modal opens immediately, shows "estimating actual reclaim…" with a spinner, updates in place when the oracle lands, and `y` stays live throughout, acting on whatever is known.
2. **[2] "Frees at least X" overstates on the dev machine's own compressed mount** — bucket-1 bytes are logical (`fe_length`), not physical, so "at least" can be off by the compression ratio. Resolved: D2 drops absolute-promise wording for `btrfs fi du`'s vocabulary (exclusive/shared, never "you will get back exactly X") plus a mandatory caveat line on any `compress`-mounted device.
3. **[3] Thesis's UI home is dark exactly where the story lives** — no ambient signal on directory rows means a snapshot-heavy tree gives no hint to reclaim without the user first marking. Resolved by the shape decision itself (D1): Option B was chosen precisely to fill every row's bar segment ambiently, not just measured/marked rows.
4. **[4] Selection-card `x` on a directory under an active filter reproduces the FilterActive trap** — nothing suppresses FIEMAPing a whole (filtered) subtree via the card. Not separately re-addressed in D1–D6: the shipped design (B) triggers the oracle from mark/basket/confirm only, not a cursor-driven selection-card affordance, so this specific trigger vector was not carried forward as specified.
5. **[5] Auto-oracle on cursor motion fights slice-5 quiescence; the µs rate is a lab number** — per-keystroke ioctls would flip the idle-quiescent loop hot, and warm-NVMe timings don't hold on cold cache/NFS/HDD. Same disposition as [4]: the cursor-triggered auto-figure was not adopted; B triggers the oracle only at mark/basket/confirm time.
6. **[6] The range is honest at compute time, not at deletion time** — external writes during a large delete's own unlink window can flip buckets after the number was shown. Accepted as an inherent, disclosed limitation: D3's "as of \<computed-at\>" wording and explicit non-tracking of external filesystem writes state this rather than hiding it.
7. **[7] "Extract (dev, ino, nlink, disk)" hides a real subtree walk and a fresh open+FIEMAP per file** — marks and tree nodes don't carry `(dev, ino)` past scan time. Accepted as an implementation correction, not a design change: the oracle must walk the frozen tree under the read guard and re-learn `(dev, ino)` via a fresh open, which is also where TOCTOU (handled as the unknown bucket) is caught.
8. **[8] "Zero memory at rest" is true; "zero cost" is the claim that matters and it isn't** — repeated marks re-FIEMAP the whole selection every debounce window, hammering dentry/inode caches invisibly to camembert's own RSS. Accepted as a disclosed cost of on-demand recomputation; no incremental cache was adopted (it would reintroduce the staleness the on-demand model avoids).

#### Attack B (Option B — eager exclusive floor + oracle; verdict: survivable with amendments; additivity proof survives cleanly)

1. **"Cannot lie" is false on compressed filesystems** — `fe_length` on `ENCODED` extents is logical, not physical; a 200 MiB all-zero file on `compress=zstd` shows `excl ≥ 200 MiB` but frees ~7 MB. Resolved by D2: no `SortKey::Exclusive` in phase 2 (removing the sort-authority failure mode) and vocabulary that never claims exact reclaim, plus the mandatory compression caveat line.
2. **The ambient floor over-states after any external write; staleness has a lying direction** — an external reflink/dedup after the pass makes the stored floor stale-high, with only a passive timestamp as a hint. Partially resolved: D3 acknowledges external writes are "not tracked" and mandates every ambient surface show "as of \<computed-at\>" — disclosed, not fixed.
3. **The under-statement trap: the floor is least informative exactly where phase 2 is needed** — heavy-sharing trees show floor 0 everywhere, which reads as "nothing to reclaim" on the biggest opportunity. Mitigated by D2: a floor of 0 on a nonzero file renders explicitly as **"fully shared"**, never an empty/absent figure, turning the degenerate case into the feature's most informative answer rather than a silent zero.
4. **The oracle's bucket 3/4 separation is claimed to be helped by the eager pass, but the eager pass stores nothing that helps** — an internal contradiction between the data model (no address map kept) and the oracle's own claim. Rejected: the shipped design does not claim the eager pass helps this split; D4's oracle merges buckets 3/4 into one honest "shared elsewhere, won't be freed" line, adopting Option A's simpler merged contract instead.
5. **The "cannot over-state" guarantee is modern-kernel-only and undocumented as such** — FIEMAP's SHARED check had correctness bugs before the ~5.17–6.1 backref-cache rewrite. Resolved: D3 gates the ambient floor on kernel ≥ 6.1 (checked via `uname`); the oracle still runs on older kernels but with a caveat line.
6. **`--no-fiemap` on a btrfs device reintroduces exactly the lie** — falling back to tier-H's `floor = disk` on a device that's still reflink-capable is only honest on ext4, not btrfs. Resolved verbatim: D3 states `--no-fiemap` disables floor and oracle both, and on btrfs the bar segment then shows nothing — "never the disk-size fallback, that reintroduces the lie."
7. **Eager-cost numbers are NVMe-warm best case; wrong on HDD and battery** — no device-class or power gating in the proposal, default-on for every tier-F device. Not resolved in D1–D6: accepted as a named, bounded, cancellable, off-thread cost; left to revisit with field data rather than gated by device class.
8. **Memory arithmetic is right but optimistic on directory count** — the 48 MB estimate assumes ~10% directories; source/dev trees run 20–30%, pushing the true cost to ~60 MB. Resolved: D3 states the range explicitly as "~48–64 MB @ 10M," not a single point estimate.
9. **LCA retraction on partial group deletion is a bug-prone incremental path; prefer recompute** — the proposed incremental retraction re-derives LCAs against a mutating registry and a growing tombstone set, a rich bug surface, and can under-count. Resolved: D3's floor is invalidated and recomputed (never incrementally mutated) on in-app deletion epochs — pure re-aggregation over surviving nodes, confirmed in the shipped `reaggregate_floor` implementation.
10. **The two-tone in-bar segment breaks the identity-color invariant** — a bright/dim split introduces a second color competing with the "bar color == name color == wheel slice color" rule. Resolved: the shipped theme renders the bright segment as the *same* identity hue lightened toward white (bold-only fallback on ANSI-16/mono), never a second color.
11. **The doc misquotes the existing confirm UI it claims to fix** — phase 1's actual modal is a plain disk-bytes prompt plus a qualitative hardlink advisory, not an "optimistic freed estimate" number. Accepted as a documentation correction; D4 describes the real replacement accurately: the oracle quantifies the previously-qualitative hardlink caveat into "frees N exclusive (+ M shared…; K elsewhere won't be freed)."

#### Attack C (Option C — viewport-driven trickle floor + oracle; verdict: kill for phase 2 — two independent fatals)

1. **FATAL-1 — the "channel-woken, idle-quiescent loop" claim is false against the real render loop** — `event::poll` only watches the terminal fd; a continuous trickle keeps `needs_frequent_polling` true for the whole mapping window, pinning the loop at 33 ms hardest on exactly the short laptop session C is built for. Rejected: cited verbatim in D1 as one of the two fatals that killed Option C outright.
2. **FATAL-2 — the floor can overstate, the one forbidden direction, once a long-lived streaming mapper's in-flight delta outlives a deletion** — a chunk covering a just-deleted subtree can land after `apply_removal` already subtracted that subtree's partial floor, double-subtracting the ancestor's true count into overstatement. Rejected: cited verbatim in D1 as the second fatal ("its incremental floor can overstate during the deletion window").
3. **SERIOUS-3 — the win is confined to a narrow btrfs × short-session × huge-tree × quit-early corner; the cost is permanent** — inert on ext4 (tier H is already instant), converges to B on any session outliving the trickle. Rejected along with Option C as a whole (D1); the narrow-corner economics were part of the case against building the scheduler at all.
4. **SERIOUS-4 — viewport priority inverts discovery: floors appear where the user already looked, not where the space is** — drilling a deep path starves the breadth trickle that would find the actual largest reclaim elsewhere. Rejected with Option C; B's largest-directories-first global ordering (adopted, D1/options recommendation) avoids this by construction.
5. **SERIOUS-5 — the single `computed_at_last` timestamp is a per-row lie** — one session-wide timestamp mislabels rows whose contributing files were mapped much earlier, worse than B's single honest "mapped at HH:MM" over one bounded pass. Rejected with Option C.
6. **SERIOUS-6 — C would ship the first navigation-history-dependent rendered number** — two identical scans in two sessions would show different segments depending on where the user wandered, breaking the tool's reproducibility grain. Rejected with Option C.
7. **ANNOYING-7 — per-row coverage honesty multiplies the annotation burden across the whole viewport** — up to 40 distinct, live-changing coverage percentages versus B's single global gauge line. Rejected with Option C.
8. **ANNOYING-8 — fill-in flicker lands on the rows the user is actively reading** — viewport-first mapping means segments visibly grow in under the cursor, reintroducing exactly the flicker attack-b's amendment 6 pushed to avoid. Rejected with Option C.
9. **ANNOYING-9 — the long-lived streaming mapper is a lifecycle shape the codebase does not have**, introduced into the post-scan engine/UI boundary the project has deliberately kept to one-shot spawn/receive/discard. Rejected with Option C; B's one-pass model was chosen partly because it fits the existing pattern.
10. **COSMETIC-10 — the concessions C already books are real losses, not neutral trades** — no sort key, only-honest-at-100%-coverage floor sums, HDD viewport IO bursts needing a media-aware throttle. Rejected with Option C.

Originals recoverable from git history.
