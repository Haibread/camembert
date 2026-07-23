# Freeable phase 2 — decisions (2026-07-23, delegated session)

Outcome over the [options dossier](freeable2-options.md) and the three
attack reports ([a](freeable2-attack-a.md), [b](freeable2-attack-b.md),
[c](freeable2-attack-c.md)). **The user delegated this session's
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
