# Option C — viewport-driven trickle floor + selection oracle

> Proposal by a design agent instructed to push the "pay for extent
> truth where the user is looking, never for the whole tree up front"
> angle as hard as it honestly can. Not a decision. Facts referenced
> from [freeable2-research.md](freeable2-research.md) (§n) and phase-1
> decisions ([freeable-decisions.md](freeable-decisions.md), D-n).
> Written to be attacked stand-alone.

## 1. Pitch

Option B pays the whole extent bill at scan end — ~1 s per 100k files,
~2 minutes at the 10 M design target (§1, §3) — for ambient floors most
of which nobody will ever look at. Option A pays nothing and shows
nothing ambient. C's bet is the product's own motto, "instant
navigation, numbers settle in behind you", applied to phase 2:
**extent mapping is demand-driven**. The directory the user is looking
at gets mapped first (the ~40 visible rows and their subtrees, biggest
first); an idle-priority trickle fills in the rest of the tree behind
them; a user who scans 10 M files, glances at the top two levels and
quits has paid for the top two levels, not for two minutes of ioctl
churn.

The numbers C shows are the same two honest quantities as B — the
**additive exclusive floor** ambiently (in-bar bright segment,
selection card) and the **selection oracle** exactly at action time —
so C inherits B's entire answer to the non-additivity attack (see §2).
What changes is *when* floors exist: progressively, viewport-first,
with per-row coverage honesty instead of a single "mapping… N %"
progress line.

The load-bearing property that makes progressive display honest: **a
partial floor is a valid floor.** Σ exclusive bytes over any *subset*
of a subtree's files ≤ the full floor ≤ true freeable. Every
intermediate state C ever renders is a true statement with "≥"
semantics; coverage marking tells the user *how much* of the subtree
has been consulted, not whether the number is trustworthy (it always
is — it is just possibly far below its final value).

## 2. Semantics

Identical to Option B's — restated here so this document stands alone:

- **Exclusive bytes of a file**: Σ `fe_length` over FIEMAP extents
  with `SHARED`, `UNKNOWN` and `DELALLOC` unset, counted only when
  `st_nlink == 1`; no `FIEMAP_FLAG_SYNC` ever (7.3× cost, unbounded
  writeback tail, §1); unit = allocated logical bytes, same as the
  `disk` column (§2; compression wording in §6).
- **Floor of an entry**: guaranteed freed-at-least on deleting exactly
  this entry. Files with `nlink > 1` floor at 0; a hardlink group
  wholly inside a subtree *and* fully seen by the scan
  (`group.len() == st_nlink`, §4) contributes its exclusive bytes at
  the group's LCA directory. Floors are additive (each extent has
  exactly one scanned owner): `floor(dir) == Σ floor(children) +
  LCA-landed groups` — the invariant tests assert.
- **Oracle buckets** for a fixed selection S: (1) exclusive —
  guaranteed freed; (2) shared only within S as far as the scan can
  see — "up to", never promised (invisible external referencers are
  undetectable unprivileged, §3); (3) held by scanned files outside
  S / (4) shared outside the scan — not freed, named with reasons;
  (5) unknown (delalloc, vanished, unopenable) — excluded, one honest
  line. Correlation is per-device (`st_dev`-scoped physical
  addresses), interval arithmetic at 4 KiB granularity (partial
  overlaps merged — `btrfs fi du`'s own documented subtlety, §3).
- **Why the floor doesn't lie** (the attack-b answer): it is additive
  with zero double-counting, its only error direction is
  understatement ("≥"), and the moment-of-action number is the
  oracle's fresh exact range. The snapA/snapB 90 GiB counterexample
  renders as floor 0 / 0 / 0 (all true) ambiently, and "at least 0,
  up to 90 GiB — shared only within your selection" when both are
  marked.

## 3. Data model

New module `camembert-core/src/freeable2.rs`; nothing in the 32-byte
`Node`, dump, or diff (phase-1 D8 pattern).

```rust
pub struct FloorMap {
    /// Per-node floor in 4 KiB blocks (u32, round-down = floor-safe);
    /// u32::MAX sentinel = not yet mapped / unknowable. Same layout as
    /// Option B — the options differ in fill strategy, not shape.
    node_floor: Vec<u32>,
    /// Per-DirId *partial* subtree floor (bytes): monotone
    /// non-decreasing as mapping proceeds; always a valid floor.
    dir_floor: Vec<u64>,
    /// Per-DirId coverage: bytes of the subtree's files consulted so
    /// far vs the subtree's total file bytes (drives the per-row
    /// coverage marker and the "extent map N %" line).
    dir_mapped: Vec<u64>,
    unknown_bytes: u64,
    unknown_files: u64,
    computed_at_last: SystemTime, // most recent chunk (staleness line)
}
```

Memory at the 10 M target: `node_floor` 40 MB + `dir_floor` 8 MB +
`dir_mapped` 8 MB ≈ **56 MB** — slightly *more* than B (the coverage
vec is C's own price), fully paid up front even if little gets mapped.
The alternative (allocate lazily per touched subtree) trades that for
fragmented bookkeeping; C proposes the dense vecs and says so.

## 4. Lifecycle — the demand-driven mapper

One background mapper thread, alive for the whole post-scan session,
fed by a priority queue:

1. **Priority 1 — viewport**: on every navigation event, the UI
   enqueues the current directory's visible rows (file rows directly;
   dir rows as "map this subtree, largest-`td` first"). Debounced with
   navigation (~150 ms) so flying through directories doesn't queue
   the world; superseded entries are cancelled (a generation counter
   per enqueue — the latest-wins nav-cell idiom).
2. **Priority 2 — idle trickle**: when the viewport queue is empty,
   the mapper walks the rest of the tree, largest directories first,
   until the whole tree is mapped (converging on Option B's end state)
   or the session ends.
3. **Mechanics per chunk** (shared with B): take the post-scan
   `RwLock` read guard, extract ~10k files' paths +
   `(NodeId, dev, ino, nlink, disk)`, drop the guard, then
   `open(O_RDONLY|O_NOFOLLOW)` + paginated FIEMAP + close per file
   (`ENOENT`/`EACCES`/`EOPNOTSUPP` → unknown; multi-link inodes once
   per `(dev, ino)` via the canonical link). Publish floor deltas up a
   channel; the UI thread folds them into `FloorMap` at its own
   cadence (channel-woken, idle-quiescent loop preserved — no 33 ms
   busy polling returns).
4. **Monotone honesty**: `dir_floor` only grows during mapping;
   `dir_mapped` tracks consultation. Both are rendered, so a
   half-mapped directory shows "≥ 1.2 GiB excl · 54 % mapped" — true
   at every instant.

Costs (§1, §3: ~6–15 µs/file raw, ~13.5 µs/file field rate): a visible
directory of 10k files is fully mapped ~0.1 s after the user lands on
it; a 1 M-file subtree ~10 s (trickling visibly); the whole 10 M tree
~2 min of idle time — the same total as B *if* the session lives that
long, and strictly less otherwise.

**Opt-out**: `--no-fiemap` (env `NO_FIEMAP`), same flag as B, disables
the mapper and the oracle's extent tier (hardlink tier remains, ~free).
Documented in `--help` + README in the same change.

## 5. Lifecycle — the selection oracle

Identical contract to Option B §5 (triggers: basket change debounced,
explicit `x`, delete-confirm; auto up to ~50k files; advisory
async fill-in in the confirm modal, never blocking — D6 precedent;
results stamped `(selection fingerprint, deletion epoch)`). One C-
specific wrinkle: the oracle *reuses* `node_floor` values mapped less
than a debounce window ago? **No** — rejected within this proposal.
The oracle always re-FIEMAPs its selection: sharing is volatile (§7,
unlink→clear in 14 µs §1) and the action-grade number must be fresh;
the mapper's data is navigation-grade only. The two never mix.

## 6. UI surface

- **In-bar bright segment** (tui reservation 2): bright fraction =
  `floor/disk`. Rows whose subtree is not fully mapped render the
  segment from the partial floor **plus a dim coverage tip** (final
  cell of the segment dimmed) and the selection card carries the
  "N % mapped" figure; fully mapped rows render clean. Unmapped rows
  (mapper hasn't reached them, tier Z, unknown) render today's plain
  bar. Presence rule is session-stable (tier-F/H session ⇒ segments
  appear as mapping arrives) — the *arrival* is progressive, which is
  C's signature and its main UX risk (attack-b amendment 6 pressed:
  no per-view flicker, but there is per-time fill-in).
- **Gauge line**: "extent map 34 % · mapping…" while the trickle runs;
  disappears at 100 %.
- **Selection card**: "excl ≥ 1.2 GiB of 4.5 GiB · 54 % mapped ·
  updated 14:02"; oracle detail replaces it for measured selections.
- **Column/sort**: `SortKey::Exclusive` is **not** offered in C.
  Sorting on a partially-filled floor ranks directories by *how much
  has been mapped* as much as by exclusivity — a coverage artifact
  promoted to ranking authority, exactly the attack-b finding 2 trap
  in a new costume. C accepts the loss and names it; a session that
  wants the sort key should prefer B (complete floors) for it.
- **Flat mode `t`**: file rows auto-prioritize like visible tree rows
  (they are the viewport). Breakdown/filter floor sums: only honest at
  100 % coverage; C defers them to "when the trickle completes" and
  labels them "partial" before that. Messier than B, stated plainly.
- **Compression caveat**: identical to B — one line on
  `compress`-mounted devices ("figures are allocated (uncompressed)
  bytes; physical reclaim may be smaller — not exposed to unprivileged
  users", §2). Same unit as `disk`, same pre-existing blind spot,
  worded not fixed.

## 7. Deletion integration

- Confirm modal: the oracle range replaces phase-1's optimistic
  estimate (as in B §7); open-file advisory unchanged.
- Floor lockstep: `apply_removal` subtracts the removed subtree's
  `dir_floor` **and** `dir_mapped` up the chain (both additive); LCA
  contributions of hardlink groups broken by a partial-group deletion
  are retracted along the old LCA chain (the delete flow already
  touches those groups for its warning). Same invariant test as B:
  floors re-derive exactly after any removal sequence. Coverage
  percentages stay consistent because numerator and denominator
  shrink together.

## 8. Filesystem tiers

Identical to B §8, restated: per unique `st_dev`, one `fstatfs`
(magics: btrfs `0x9123683e`, XFS `0x58465342`, ext family `0xef53`,
ZFS `0x2fc12fc1` unverified live — §5). Tier F (btrfs/XFS): full
mapper + oracle; first FIEMAP `EOPNOTSUPP` downgrades the device.
Tier H (ext family, other real fs): floors from registry data alone
(`nlink == 1` ⇒ floor = `disk`; LCA rule for groups) — **computed
instantly at scan end for the whole tree, no ioctls**, so tier-H
devices are always "100 % mapped" from the start; the mapper only ever
works tier-F devices. Tier Z (ZFS): nothing + the honest line ("ZFS
does not expose per-file sharing — no figure rather than a guess",
§5); the hardlink fallback is deliberately withheld on ZFS (block
cloning could make it overstate). Root-only precision
(`TREE_SEARCH_V2`): not in C; dossier decision, reject recommended
(§3's total privilege wall).

## 9. Dump / diff / CLI

Nothing in dumps (extent sharing is more volatile than `/proc` state,
§7; phase-1 D7's capability argument applies verbatim), nothing in
diff. CLI: `--no-fiemap`/`NO_FIEMAP` only.

## 10. Phasing

1. **Slice 1 — oracle** at the delete path (identical to Option A's
   core: confirm range + basket figure).
2. **Slice 2 — tier-H floors** (instant, no mapper) + in-bar segments:
   ships the reservation-2 segment on ext-family devices for the cost
   of a registry pass.
3. **Slice 3 — the mapper**: priority queue, viewport feed,
   coverage bookkeeping, trickle, gauge line.
4. **Slice 4 — composition**: flat-mode prioritization, filter/
   breakdown sums at full coverage.

## 11. Where to press (self-identified)

1. **The scheduler is the design.** Priority queue, navigation
   debounce, generation-counter cancellation, per-dir coverage
   arithmetic, monotone publish — C carries meaningfully more moving
   parts than B's "one pass, biggest dirs first". Every piece is a
   bug surface in the exact code region (post-scan UI/engine boundary)
   the project has kept deliberately simple. Attack the complexity
   budget first.
2. **Perpetual partiality.** On a 10 M-node tree a short session ends
   with most rows unmapped and the segment landscape *patchy* — some
   bars bright, some dark, distinguished only by mapper history. The
   per-row coverage tip mitigates; whether users read "no segment" as
   "nothing exclusive here" (a wrong inference C's own UI invites) is
   the honest-display question to press hardest.
3. **Non-determinism.** Two identical scans in two sessions show
   different segments at t+10 s depending on where the user wandered.
   The product has so far kept rendered numbers reproducible
   (bit-identical folds, deterministic tiebreaks); C's ambient layer
   is the first navigation-history-dependent display. Is that
   acceptable for a "≥" figure?
4. **Fill-in flicker vs amendment 6.** Attack-b demanded stable column
   presence; C's segments *appear over time* on rows the user is
   staring at (the eased-animation machinery can soften it — 150 ms
   fills — but the bar landscape still shifts under the eye in a way
   B's single "floors landed" transition does not).
5. **The savings may be illusory.** The idle trickle converges on B's
   full pass anyway in any session longer than ~2 minutes at 10 M
   (much sooner on typical trees) — so C's win is confined to short
   sessions on huge trees, and its cost (scheduler + coverage + UX
   risks 1–4) is permanent. Quantify the actual short-session
   population before buying this.
6. **Viewport pressure on spinning disks.** Navigation-triggered ioctl
   bursts against an HDD (the 2-thread media tier) can make *browsing*
   feel like it now has IO cost — the exact regression
   browse-during-scan was designed to avoid. Needs a media-aware
   throttle (reuse the scan's rotational detection), which is more
   scheduler still.
