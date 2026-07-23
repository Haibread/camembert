# Adversarial review — Option B (eager exclusive floor + selection oracle)

> Verdict: **SURVIVABLE WITH AMENDMENTS**, but not the amendments B expects.
> The one thing this review was tasked to kill hardest — the additivity proof
> of the exclusive floor — **survives cleanly**, including under bookend
> extents, self-reflinks, and partial sharing (see "What survives"). What does
> *not* survive is the **"a number that cannot lie"** framing that the whole
> option is built around. The floor over-states physical reclaim on compressed
> filesystems (the *default* btrfs config), over-states after any external
> reflink/dedup/snapshot, and can over-state on pre-6.1 kernels — three lying
> directions the thesis claims are impossible. And it is uninformative
> (uniformly 0) on exactly the shared-heavy trees phase 2 exists for. Amended,
> B collapses to what it honestly is: **an exact oracle at the moment of
> action, plus an opt-in conservative lower bound that is trustworthy only on a
> quiescent, uncompressed, modern-kernel tree** — a much narrower claim than
> "the number that lives everywhere."

Facts cross-checked against `freeable2-research.md` (§n), `freeable-attack-b.md`
(phase-1 nemesis), and code: `camembert-core/src/freeable.rs`,
`camembert-core/src/tree.rs`, `camembert-core/src/scan/hardlink.rs`,
`camembert-core/src/delete.rs`, `camembert-core/src/flat.rs`,
`camembert/src/ui.rs`, `camembert/src/ui/state.rs`. Severity tags: **[FATAL] /
[SERIOUS] / [ANNOYING] / [COSMETIC]**.

---

## What survives (credit first, because it is load-bearing)

**The additivity proof of the exclusive floor is sound.** This was the
designated kill target; it holds, and it holds for a subtle and *good* reason.
The floor's defining predicate — an extent with `FIEMAP_EXTENT_SHARED` unset —
is itself the cross-file disjointness guarantee. `SHARED` unset means the extent
has exactly one reference in the *whole filesystem*, so no other file (scanned
or not) can also count it. Therefore `Σ floor(children) == floor(parent)` with
zero double-counting, by construction, with no inclusion-exclusion needed. Every
counterexample the brief asks me to construct fails to break it — and each one
fails *safely* (toward under-count), never toward over-count:

- **Extents shared between a file and itself at two offsets** (self-reflink,
  intra-file dedup): btrfs's `SHARED` check walks backrefs; two references,
  even from the same inode, make the extent report `SHARED` **set**, so it is
  *excluded* from the floor. Under-count, safe. Additivity untouched.
- **Bookend extents** (the pointed question): after a mid-extent COW overwrite,
  one physical extent E is referenced by a head + tail bookend from the *same*
  inode. btrfs's shared check is same-inode-aware — it reports those bookends as
  **not shared** (no *other* inode references E), so the floor counts each
  bookend's *logical* `fe_length`. Deleting the file frees the whole physical E
  (both bookends drop, refcount → 0), which is *more* than the summed logical
  bookend lengths minus the overwritten hole. Floor under-states physical
  reclaim — safe. Cross-file disjointness is intact because a bookend that were
  *also* referenced by a second inode (reflink + partial COW) flips `SHARED`
  set on both and drops out. There is no construction where two *different*
  scanned files both count one `SHARED`-unset extent.
- **Partially-shared / misaligned reflink ranges**: any physical extent with a
  second referencer reports `SHARED` set at extent-item granularity → excluded.
  Safe.

So the invariant B wants its tests to assert — `floor(dir) == Σ floor(children)
+ Σ LCA-landed groups` — is genuinely a subtree-sum with the same shape as
`ta`/`td` (`tree.rs:677` `apply_delta`), and it is *correct*. This is the real
strength of the option and it should be stated with confidence.

**Other things that hold:**

- **Side-map isolation** (`FloorMap` as parallel `Vec`s, zero changes to the
  32-byte `Node`, dump, diff) mirrors the existing `excluded` / `tombstones`
  side-set idiom (`tree.rs:322`, `:328`) and the phase-1 D8 pattern. No new
  concurrency surface: written single-owner post-scan, exactly like the
  `/proc` ledger (`freeable.rs`).
- **Oracle epoch/fingerprint stamping** to discard stale results mirrors the
  existing `fold(tree, patterns, cap, epoch)` / `snapshot(epoch)` pattern
  (`flat.rs:596`, `:878`) — the right idiom, already proven in the codebase.
- **`--no-fiemap` presence-semantics** matches `NO_PROC_SWEEP` plumbing
  (`ui.rs:174`, `:270`). Consistent (but see finding 6 for a scope bug).
- **Not in dumps / not in diff** is the correct call: extent sharing is *more*
  volatile than phase-1 `/proc` state (research §7), and D7's capability
  argument applies verbatim.

---

## Findings

### 1. "Cannot lie" is false on compressed filesystems — the default btrfs config [SERIOUS]

The floor is defined as `Σ fe_length` over `SHARED`-unset extents (proposal §2),
and `fe_length` for a compressed (`ENCODED`) extent is the **logical**
(uncompressed) length — research §2 nails this down: "neither" FIEMAP field
exposes the on-disk compressed byte count unprivileged. The *physical* space
freed by deletion is the compressed footprint, which is **smaller** — up to the
compression ratio (research measured ~30:1 on trivially-compressible data).

So on a `compress=zstd` mount, "deleting this frees **at least** ⟨floor⟩" is
**false as a statement about disk space**: the floor is a lower bound on
*logical* bytes, which is an *upper*-ish bound on *physical* reclaim. The one
direction the entire thesis promises impossible — over-stating freed disk space
— is exactly what the floor does here, systematically, on the most common btrfs
configuration (the research machine's own default is `compress=zstd:1`;
openSUSE, Fedora Workstation, and many others enable it out of the box).

§8's mitigation is a prose caveat line ("physical reclaim may be smaller"). That
does not rescue a **number** that is simultaneously used as a `SortKey`, a
bright-bar fill fraction, and a delete-confirm headline. A user sorting by
`excl` to find "biggest guaranteed reclaim" ranks by a logical figure that can
be 30× the real disk win, with the disclaimer in a different glance. This is the
phase-1 attack's finding-2 spatial-divorce problem (`ui.rs` gauge vs. table live
in different regions) reincarnated on a per-row number.

**Concrete**: a 200 MiB all-zero file on `compress=zstd` frees ~7 MB on disk
(research §2, measured `df` delta) but shows `excl ≥ 200 MiB`. Sort by `excl`,
it floats to the top; delete it, reclaim 7 MB. "Correct where others lie" just
lied by 28×.

**Amendment**: on any device whose mount options contain `compress`, the floor
is **not** a `≥`-on-disk guarantee. Either (a) drop the `≥` framing and the
sort key on compressed devices and relabel the figure as "logical (uncompressed)
size, not disk reclaim", or (b) suppress the ambient floor entirely on
compressed devices and keep only the oracle's honestly-hedged range. The
existing `disk` column shares this blind spot, true — but phase 1 never
stamped it "the number that cannot lie."

### 2. The ambient floor over-states after any external write; staleness has a lying direction [SERIOUS]

The deletion epoch (`flat.rs:421` `epoch`, bumped by in-app `apply_removal`,
`tree.rs:504`) tracks **only camembert's own deletions**. Research §7 is
explicit: extent sharing flips from "any write anywhere on the filesystem by
any process" — a backup job's snapshot, another user's `cp --reflink`, a
scheduled `duperemove`/`bees` run, btrfs balance/defrag. On exactly the btrfs
systems phase 2 targets, an automatic dedup or snapshot pass touching a
scanned file **between the eager pass and the moment the user looks** is the
norm, not the exotic case.

Direction of the resulting error is the fatal part:

- external reflink/dedup of a previously-exclusive file → that extent becomes
  `SHARED` → **true floor drops → stored floor over-states**. The always-visible
  bright segment, `excl` column, and sort key now silently over-promise, with
  only a passive "extents mapped at 14:02" timestamp as a hint. No inotify, no
  re-FIEMAP, no epoch bump — nothing tells camembert or the user.
- external deletion of the *other* sharer → previously-`SHARED` extent becomes
  exclusive → stored floor under-states → safe.

B's defense is "navigation-grade vs action-grade": the oracle re-reads at action
time so no *actionable* figure is stale. But that concedes the point — the
ambient surfaces (the ones §2 calls "the number that lives everywhere") **are
navigation-grade and can silently over-promise indefinitely**. The distinction
is a fig leaf: users do not read a bar as "provisional until I mark it." This is
B's own press point 3, and it deserves to rank *higher* than the doc frames it,
because the triggering events (auto-dedup, snapshot backups) are precisely
correlated with the target filesystem.

**Amendment**: the ambient floor needs an honest staleness story stronger than
a timestamp — at minimum, a visible "may be stale" affordance once
`computed_at` ages past some window, and no sort authority for a figure with
no freshness guarantee. Or: accept that the floor is action-grade-only and drop
the ambient surfaces, which is most of B's pitch.

### 3. The under-statement trap: the floor is least informative exactly where phase 2 is needed [SERIOUS]

This is the inverse of phase-1's lie and it is **structural, not cosmetic**.
Consider the floor's information content as a function of how much sharing a
tree has:

- **No sharing** (ordinary tree): every `nlink==1` file has `floor == disk`.
  The `excl` column and bright bar segment then **duplicate** the existing
  `disk` column and the full-length bar (`ui.rs:2601`, `:2559`) — zero new
  information, just visual noise.
- **Heavy sharing** (the reflink/snapshot trees phase 2 exists to illuminate):
  `floor == 0` almost everywhere. Research §3's own experiment: `A.bin`/`B.bin`
  mutually reflinked, each floor **0**, union **4 MiB** (the whole size). A
  directory holding a reflinked pair shows floor 0 at every level, yet deleting
  it frees everything. The bright segment is blank; the `excl` column is a
  column of `≥0`; and — the trap — a user scanning for reclaimable space reads
  "≥ 0" as **"nothing here to reclaim"** when this is the single biggest win in
  the tree.

So the floor delivers real signal only in the *middle band* (partially-shared
trees). Its usefulness is **anti-correlated with the feature's reason to
exist**. The biggest-reclaim opportunity in a snapshot-heavy tree is invisible
in every ambient surface and surfaces only if the user already suspects it and
manually marks + runs the oracle — which requires them to already know where to
look, defeating the "ambient number that guides you" pitch.

The sort key makes it worse (B half-admits, press point 5): a 90 GiB reflinked
snapshot pair sorts to the **bottom** on `excl`, hiding the largest reclaim
under the smallest ambient number. That is a real "the tool buried the answer"
failure.

**Amendment**: the ambient floor cannot be the primary "where can I reclaim"
surface on shared-heavy trees. Either surface a *second* ambient signal (e.g.
"this subtree has N GiB shared-within-itself — mark it to see the reclaimable
union"), or accept the floor is a supporting figure and make the oracle the
headline discovery tool, not the floor.

### 4. The oracle's bucket 3/4 separation is claimed to be helped by the eager pass, but the eager pass stores nothing that helps [SERIOUS]

Internal contradiction between §3 and §5. §3 (data model) states the eager pass
"keeps **no** extent-address map … the oracle re-FIEMAPs its selection
instead." §5 step 2 then says: "When the eager pass ran … it recorded per-file
*shared bytes* totals scan-wide, which lets the oracle bound bucket 3."

A per-file shared-bytes **scalar** cannot separate bucket 3 ("held by a scanned
file *outside* the selection") from bucket 4 ("held by a snapshot/excluded
subtree *outside the scan*"). Knowing file F has 90 GiB of shared bytes tells
you nothing about *who* F shares with or *whether that partner is inside your
selection*. Only physical-address correlation answers that — the exact map §3
deliberately refuses to store (correctly, for RSS and staleness reasons).

Consequence: buckets 3 and 4 **stay merged in every slice**, degrading to
"shared with something — your selection, another scanned file, or a snapshot
this scan can't see." The oracle's headline "up to Y" ceiling therefore always
includes bytes that may be *wholly* held by an invisible snapshot. "Up to 90
GiB" can correspond to an actual reclaim of **0** (a snapshot holds all of it).
It is not technically a lie — a ceiling is a ceiling — but it is a **useless
ceiling presented at the moment of action**, which is where B promised
exactness. §11 press point 4 half-concedes this ("the weakest mechanism") but
§5 step 2 papers over it with a claim the data model contradicts.

**Amendment**: strike the §5-step-2 claim that the floor pass helps bucket 3/4.
State plainly that unprivileged, bucket 3 and 4 are inseparable without either
root (`LOGICAL_INO`, EPERM per research §3) or a marked-adjacent address map
(the slice-3 work nobody has committed to). Cost the "up to Y" number honestly:
it is an over-approximate ceiling, useful mainly when it is *small* (little
shared) and near-useless when it is large.

### 5. The "cannot over-state" guarantee is modern-kernel-only and undocumented as such [SERIOUS]

The floor's safety rests entirely on FIEMAP never reporting `SHARED` *unset*
for a genuinely shared extent (a false-negative would make the floor
over-state). Research validated this on kernel **7.1.4** only. The FIEMAP shared
check had real correctness bugs under concurrent COW on older kernels, fixed by
the backref-cache rewrite around the 5.17–6.1 era. On the stable-distro kernels
much of the target user base runs (5.10, 5.15), a false-unset `SHARED` under a
concurrent writer is possible → the floor over-states.

The whole "cannot lie" edifice thus silently assumes a kernel floor version that
is never stated or checked.

**Amendment**: name a minimum-kernel assumption for the `≥` guarantee, or
detect it, or downgrade the framing to "conservative on modern kernels." At
minimum this belongs in the dossier decisions next to the privilege-wall notes.

### 6. `--no-fiemap` on a btrfs device reintroduces exactly the lie [ANNOYING]

§4/§8: `--no-fiemap` "skips the pass and the oracle's extent tier (hardlink tier
still works)", and the tier-H row defines `floor = disk` for `nlink == 1`. But
tier H is only *honest* on ext4 because ext4 has no reflink (research §5). Apply
tier-H semantics to a **btrfs** device (which is what `--no-fiemap` does — you
skip FIEMAP but the device is still btrfs) and a fully-reflinked file shows
`floor = disk` instead of the true `floor = 0`. The opt-out therefore makes the
floor *less* honest on btrfs than doing nothing.

**Amendment**: `--no-fiemap` must degrade extent-capable devices (btrfs/XFS) to
**tier-Z "no floor"**, not tier-H disk-based floors. You cannot claim a disk
floor on a filesystem where you deliberately declined to check sharing.

### 7. Eager-cost numbers are NVMe-warm best case; the default is wrong on HDD and battery [ANNOYING]

The 6–15 µs/file and "~2 min at 10 M" figures are measured on warm-cache NVMe
(research §1). The per-file cost is `open(O_RDONLY|O_NOFOLLOW) + FIEMAP-loop +
close` (§4 step 2). On a **cold HDD**, that is a metadata seek per file: 10 M
files is not 2 minutes, it is tens of minutes to hours, and it **evicts the
user's page cache** (B's own press point 2 admits the noise) and drains
**battery** for ambient data many sessions never open. The scan engine already
tiers HDDs down to fewer workers; the eager FIEMAP pass has no such gating in
the proposal — it is default-on for every tier-F device.

Good news, verified: it does **not** delay the dump write or the phase-1 sweep.
The dump/`finalize_hardlinks` runs on the finalize path *before* the arena is
published (`ui.rs:430`), and the `/proc` sweep is a separate ~23 ms thread
(`freeable.rs` header; `spawn_freeable_sweep`, `ui.rs:737`). The eager pass is
an independent post-scan thread — it blocks nothing user-visible. It just burns
a core (and a disk, and a battery) for minutes.

**Amendment**: gate the eager pass on device class **and** entry count **and**,
ideally, AC power — not unconditional on tier F. Below ~50k files eager is
fine; above, prompt or default-off with a "map extents (~N s)" affordance,
mirroring the oracle's own large-selection threshold (§5 step 4).

### 8. Memory arithmetic is right but optimistic on directory count [ANNOYING]

`node_floor: Vec<u32>` at 10 M nodes = **40 MB** exactly (10 M × 4 B) — solid.
`dir_floor: Vec<u64>` at "8 MB" implies **1 M directories** (8 MB / 8 B). That is
~10% dirs, plausible for media trees but low for source/dev trees (`node_modules`,
`.git/objects`, sharded caches routinely hit 20–30% dirs). At 25% dirs, `dir_floor`
is 20 MB and the total is ~60 MB ≈ **13%** of the ~450 MB budget, not 11%. Not a
kill — state the range (48–64 MB) rather than the best-case point estimate.

### 9. LCA retraction on partial group deletion is a bug-prone incremental path; prefer recompute [ANNOYING]

`apply_removal` (`tree.rs:504`) tombstones a subtree and negative-deltas
`ta/td/tn/te` up the chain but **knows nothing about `dir_floor`** — the floor
lockstep is a *separate* mechanism the UI layer must drive (§7). The proposed
retraction rule (on removal, for every registry group intersecting the removed
set that is "no longer wholly present," retract its LCA contribution along the
old LCA chain) is the single fiddliest invariant in the option, and it
re-derives LCAs from a **mutating registry snapshot against a growing tombstone
set** — a rich bug surface. It also *under*-counts a group whose surviving links
are all still under the same ancestor (deleting the rest would still free the
inode, but the contribution is fully retracted). Floor-safe, but wrong.

The phase-1 attack-a review explicitly praised **wholesale recompute over
incremental negative-delta** for exactly this class of bug. And deletion changes
**no other file's `node_floor`** (surviving reflink siblings becoming more
exclusive is deferred to next session, floor-safe and documented, §7). So
`dir_floor` can be cheaply **re-summed from the unchanged `node_floor` over the
live (non-tombstoned) tree** for the touched subtree, avoiding the retraction
logic entirely.

**Amendment**: recompute `dir_floor` for affected subtrees post-deletion; drop
the incremental LCA-retraction rule.

### 10. The two-tone in-bar segment breaks the identity-color invariant [COSMETIC → ANNOYING]

The current proportion bar is a **single `Span`, single color**, whose fill
length encodes `disk/parent_disk` and whose color is the row's identity color —
and there is a hard invariant, "bar color == name color == wheel slice color"
(`ui.rs:2549`, `:2559`). A bright floor sub-segment requires splitting that
`Span` into bright + dim and introduces a **second color inside the
identity-colored bar**, competing with the identity semantic the design works
to keep stable across frames (the eased `bar_progress` reveal, `ui.rs:2560`).
The math is coherent (bright = `floor/parent`, dim = `(disk−floor)/parent`) but
this is more layout and more color budget than §6's "just recolors cells inside
`BAR_WIDTH`" implies — the phase-1 attack's finding 7 (bar/column stability) in
its color-identity form.

### 11. The doc misquotes the existing confirm UI it claims to fix [COSMETIC]

§7 and §10 repeatedly say slice 1 "replaces phase-1's optimistic freed estimate
('will free 12.3 GiB')." **Phase 1 shows no such number.** The actual modal
(`ui.rs:3536`) reads `"Delete N entries — {disk} on disk?"`, where `{disk}` is
`Σ marks.disk` (`state.rs:775`), followed by a purely *qualitative* hardlink
advisory: `"K hardlinked file(s) in the selection: space is only freed once
every link to an inode is deleted"` (`ui.rs:3553`).

The *substance* is correct and I verified it: that on-disk sum **is** optimistic
for surviving hardlinks, and so is the post-delete `report.freed` — the delete
engine's own doc says "For hardlinks with surviving links elsewhere this
overestimates what the filesystem actually freed" (`delete.rs:112`), because
`apply_removal` subtracts a counted link's *full* size even when an extra link
survives (`tree.rs:548`). So the oracle's real win is **quantifying a
currently-qualitative caveat** ("space is only freed once…" → "frees at least
X, up to Y"). That is genuine value — but describe the anchor accurately, since
the design leans on it as justification for slice 1.

---

## On the phasing claim ("slice 1 is Option A in miniature, shippable alone")

Partly true, partly oversold. The **hardlink** half of slice 1 is genuinely
standalone and genuinely valuable: the subset check (`group.len()` vs `nlink`,
research §4; `HardlinkLink` already carries `nlink`, `hardlink.rs:24`) turns the
qualitative advisory into a number the delete engine cannot currently produce
(finding 11). Ship that.

The **extent** half of slice 1 is the *maximally degraded* oracle: without the
eager pass, §5 step 2's bucket 3/4 separation collapses (finding 4), so a marked
snapshot pair yields "frees at least 0, up to 90 GiB" — a band so wide it is
barely actionable at the exact moment B says exactness matters. And slice 1 is
not "Option A in miniature": it still needs tier detection (`fstatfs` per
device), the full FIEMAP pagination loop (research §1's truncation bug is a live
warning), physical-address interval arithmetic at 4 KiB granularity, and the
degraded-wording honesty machinery. That is most of the oracle engine minus
storage — the "miniature" framing undersells its cost.

---

## Recommendation

**Do not kill.** The additivity proof — the thing I was told to attack hardest —
is correct and is the best-reasoned part of the whole freeable2 dossier. The
oracle-at-the-delete/mark-path is a real, shippable improvement over phase 1's
optimistic hardlink accounting. But **stop selling B as "the number that lives
everywhere and cannot lie."** It lies (over-states physical reclaim) on
compressed filesystems, after external dedup/snapshots, and on old kernels; and
it is uninformative on the shared-heavy trees phase 2 targets. Amended, B is:
**"an exact-at-action oracle, plus an opt-in conservative logical lower bound
that carries an explicit `≥` and is trustworthy only on quiescent, uncompressed,
modern-kernel extent filesystems."** Propose and cost it as exactly that.

Amendments, priority order:

1. **Drop the "cannot lie" framing on compressed devices** (finding 1). Relabel
   the floor as logical/uncompressed there, and remove its sort authority.
2. **Give the ambient floor an honest staleness story** or make it
   action-grade-only (finding 2) — a timestamp is not enough when auto-dedup and
   snapshots are the target environment.
3. **Face the under-statement trap** (finding 3): the floor cannot be the
   primary reclaim-discovery surface on shared-heavy trees; give the oracle that
   role, or add a "shared-within-subtree" ambient hint.
4. **Strike the §5-step-2 claim** that the eager pass helps bucket 3/4 (finding
   4); cost the "up to Y" ceiling as the over-approximation it is.
5. **Name the minimum-kernel assumption** for the `≥` guarantee (finding 5).
6. **`--no-fiemap` degrades extent devices to no-floor, not disk-floor**
   (finding 6).
7. **Gate the eager pass** on device class + entry count + power (finding 7);
   correct the memory estimate to a 48–64 MB range (finding 8).
8. **Recompute `dir_floor` post-deletion** instead of incremental LCA retraction
   (finding 9).
9. **Fix the misquoted phase-1 anchor** (finding 11) and reserve real cells /
   accept the identity-color cost for any two-tone bar (finding 10).

The finding B nominated as its own weakest (bucket 3/4, press point 4) is indeed
weak and worse than admitted (finding 4). But the thing that actually guts B is
quieter and it is the same shape as the phase-1 attack's conclusion: B's reason
to exist as *ambient* data — "a floor that cannot lie, everywhere" — does not
hold. Its reason to exist as *action-time* data — an exact oracle at the delete
path — does.
