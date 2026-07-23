# Option B — eager exclusive floor + selection oracle (two tiers)

> Proposal by a design agent instructed to push the "the reserved in-bar
> segment deserves an ambient number, and there exists one that cannot
> lie" angle as hard as it honestly can. Not a decision. Facts referenced
> from [freeable2-research.md](freeable2-research.md) (§n) and the
> phase-1 ledger decisions ([freeable-decisions.md](freeable-decisions.md),
> D-n). Written to be attacked stand-alone.

## 1. Pitch

Phase 1 refused per-directory freeable numbers because the quantity is
non-additive: an additive per-dir scalar of shared-extent freeable
over-counts, and a "delete-this-alone" scalar doesn't sum
([freeable-attack-b.md](freeable-attack-b.md) finding 1). Option B's bet
is that this killed the wrong thing. There are **two** honest per-entry
quantities, and each is safe in its own register:

1. **The exclusive floor** — bytes in extents referenced by *no one
   else* (kernel `FIEMAP_EXTENT_SHARED` unset), for files whose inode
   has no other hardlink. Deleting the entry frees **at least** this.
   It is *additive by construction* (every extent is owned by exactly
   one scanned file, so child sums equal the parent's figure with zero
   double-counting), it can only **under**-state, and it is a per-entry
   fact — not a function of what else is co-selected. This is the
   number that lives everywhere: the reserved in-bar bright segment
   (tui-design reservation 2), the selection card, a sort key.
2. **The selection oracle** — a fresh, on-demand FIEMAP pass over
   exactly the files of one fixed marked selection, with
   physical-address interval correlation (the research-validated
   `btrfs fi du` mechanism, §3) and the hardlink subset check (§4).
   This is the *exact* number, computed only at the moment of action
   (marking, delete-confirm), never cached across selections, never
   compared across rows.

The floor answers "where can I reclaim?" ambiently without ever
promising more than deletion delivers; the oracle answers "what does
deleting *this* free?" exactly when the user is about to act. Phase 1's
attack demanded a number that does not lie; B's response is a floor that
*cannot* lie plus an exact figure scoped to the one context (a fixed
selection) where exactness is computable.

Evaluation model: **eager** — one full-tree FIEMAP pass at scan end, off
the UI thread, trickle-published (a partial floor is still a valid
floor, so intermediate publishes are honest — see §4).

## 2. Semantics — what every number claims, precisely

Vocabulary used by every surface of this option (and shared with the
dossier):

- **Exclusive bytes of a file**: Σ `fe_length` over FIEMAP extents with
  `SHARED` unset and `UNKNOWN`/`DELALLOC` unset, counted only when the
  file's `st_nlink == 1`. Delalloc/unknown extents are *excluded* (not
  yet allocated — claiming them would guess, research §1); no
  `FIEMAP_FLAG_SYNC` is ever used (7.3× cost, unbounded writeback tail,
  §1). Unit: allocated **logical** bytes — the same unit as the
  existing `disk` column (`st_blocks*512`, which btrfs also reports
  logically for compressed files, §2). See §8 for the compression
  wording.
- **Floor of an entry** (file or directory): guaranteed
  freed-at-least bytes if exactly this entry is deleted.
  - file, `nlink == 1`: its exclusive bytes;
  - file, `nlink > 1`: **0** (deleting one link frees nothing unless it
    is the last — the inode's bytes enter floors only via the LCA rule
    below);
  - directory: Σ floors of files in its subtree **+** the exclusive
    bytes of every hardlink group that is *wholly contained* in the
    subtree and *fully seen by the scan* (`group.len() == st_nlink`,
    §4), landed at the group's LCA directory and every ancestor above
    it.
- **Oracle buckets** for one fixed selection S — every allocated byte
  of S's files falls in exactly one:
  1. **exclusive** — `SHARED` unset: freed, guaranteed;
  2. **selection-shared** — `SHARED` set, ≥ 2 in-scan referencers, all
     inside S: freed **unless** an invisible out-of-scan referencer
     also exists (undetectable unprivileged, §3 — `LOGICAL_INO` is
     EPERM); reported as the "up to" component, never as a promise;
  3. **held elsewhere in the scan** — an in-scan referencer survives
     outside S: not freed;
  4. **shared outside the scan** — `SHARED` set but this file is its
     only in-scan referencer: a snapshot, excluded subtree, or other
     subvolume holds it (§3); not freed;
  5. **unknown** — delalloc extents, files that vanished or could not
     be opened since the scan: excluded from every figure, reported as
     one honest line ("N files / X unaccounted").
  Hardlinks map onto the same buckets: group fully inside S with
  `group.len() == nlink` → participates (FIEMAP once, via the canonical
  link); links outside the scan (`group.len() < nlink`) → bucket 4
  analog; links split across S's boundary → bucket 3 analog.
- **Oracle headline**: "deleting this frees **at least** ⟨bucket 1⟩,
  **up to** ⟨1+2⟩" — a range, with buckets 3/4 named as
  "won't be freed: held by ⟨files elsewhere / snapshots or files
  outside this scan⟩".

### Why the floor answers attack B instead of dodging it

Attack B's kill-shot example: `snapA`/`snapB` reflink-share 90 GiB; an
additive per-dir channel shows +90/+90 and +180 on the parent — three
lies. Under B: `snapA` floor 0, `snapB` floor 0, parent floor 0 — three
truths ("deleting this alone is guaranteed to free ≥ 0"), displayed
with "≥" semantics and the label **excl** (deliberately `btrfs fi du`'s
own term, §6). The 90 GiB appears exactly where it becomes true: mark
both, the oracle reports "at least 0, up to 90 GiB — 90 GiB shared only
within your selection". The floor's known cost is that it *under-sells*
snapshot-heavy trees; the direction of error is the one the thesis can
live with ("you will free at least this" is a promise the tool keeps;
"you would free this" was the one it couldn't). The oracle exists
precisely so the under-sell is corrected at the moment of action.

Additivity, stated as the invariant tests will assert: for any
directory, `floor(dir) == Σ floor(children) + Σ LCA-landed groups at
dir`. There is no aggregation model to get wrong — it is the same
subtree-sum shape as `ta`/`td`.

## 3. Data model

New module `camembert-core/src/freeable2.rs` (or `freeable/extent.rs`;
name at implementation). Zero changes to the 32-byte `Node`, to dump,
to diff — the phase-1 D8 isolation pattern.

```rust
/// Per-entry exclusive floors, computed by the post-scan extent pass.
/// A side artifact of one scan generation; rebuilt never, adjusted on
/// deletion (see §7), dropped with the session.
pub struct FloorMap {
    /// Per-node floor in 4 KiB blocks (u32: caps at 16 TiB/file; a
    /// larger file saturates and understates — floor-safe). Indexed by
    /// NodeId. Sentinel u32::MAX = unknown (unreadable, vanished,
    /// all-delalloc, or tier Z device). Directories hold 0 here; their
    /// floors live in `dir_floor`.
    node_floor: Vec<u32>,
    /// Per-DirId subtree floor in bytes (additive; maintained in
    /// lockstep with removals, §7).
    dir_floor: Vec<u64>,
    /// Bytes that could not be classified (unknown files + delalloc),
    /// per the honesty line.
    unknown_bytes: u64,
    unknown_files: u64,
    /// Wall-clock completion (staleness display) + coverage while the
    /// pass is still running.
    computed_at: SystemTime,
    complete: bool,
}
```

Memory at the 10 M-entry D4 target: `node_floor` 40 MB + `dir_floor`
8 MB ≈ **48 MB (+~11 % of the ~450 MB budget)** — the honest price of
per-entry ambient data, stated up front. Block-granularity rounding is
*down*, so stored floors never overstate. The hardlink LCA
contributions live only in `dir_floor` (a multi-link file's
`node_floor` is 0), so no per-group storage is needed beyond the scan's
existing registry.

The eager pass keeps **no extent-address map**: at 1–3 extents/file a
10 M-file tree would need hundreds of MB (§1), and extent sharing is
volatile (any process's write flips it, §7) so a cached address map
goes stale silently. The oracle re-FIEMAPs its selection instead —
which is not just cheaper in RSS but *fresher* at the moment it
matters (unlink→`SHARED`-clear is ~14 µs, §1).

## 4. Lifecycle — the eager pass

Runs once, at scan end, after canonical hardlink re-attribution — the
freeable-sweep slot (D4 precedent), on a dedicated thread.

1. **Arena access**: post-scan the `ScanOutcome` lives in
   `Arc<RwLock<…>>` (the filter-fold idiom, `ui.rs`). The pass takes
   the read lock in **chunks** (~10k files per acquisition: reconstruct
   paths + collect `(NodeId, dev, ino, nlink, disk)` for the chunk,
   drop the lock, do the ioctls lock-free), so a deletion's write lock
   waits at most one chunk extraction, never the full pass.
2. **Per file**: `open(O_RDONLY|O_NOFOLLOW|O_CLOEXEC)` by scan-time
   path → FIEMAP with the mandatory pagination loop (§1's truncation
   bug is a live warning) → classify extents → floor. `ENOENT` (moved/
   deleted since scan), `EACCES` (statable but not openable — FIEMAP
   needs an open fd, unlike the scan's `statx`), `EOPNOTSUPP` →
   unknown. Multi-link inodes: FIEMAP once per `(dev, ino)`, via the
   canonical link, results attributed per the LCA rule.
3. **Ordering**: largest directories first (subtree `td` descending) so
   the biggest floors land earliest; no viewport feedback loop (that is
   Option C's business).
4. **Publish**: per-chunk floor deltas go up a channel; the UI thread
   applies them to the `FloorMap` at its own cadence (the sweep/fold
   idiom — the post-scan loop is idle-quiescent and channel-woken, so
   no busy 33 ms polling resumes). **A partial floor is a valid
   floor** (Σ over a subset ≤ Σ over all ≤ truth), so trickled
   publishes are honest at every instant; the gauge area shows
   "mapping extents… N %" while `complete == false`.
5. **Cost** (§1, §3): ~6–15 µs/file measured raw; `btrfs fi du`'s field
   rate ≈ 13.5 µs/file. ⇒ ~1 s per 100k files, ~10 s at 1 M, ~2 min at
   10 M — all off-thread, cancellable (deletion-epoch check per chunk;
   `q` aborts). Long tail: one pathological 801-extent file costs
   ~1.2 ms (§1) — bounded by the pagination loop, no `SYNC` stalls.
6. **Opt-out**: `--no-fiemap` (env `NO_FIEMAP`, presence semantics like
   `NO_PROC_SWEEP`) skips the pass and the oracle's extent tier
   (hardlink tier still works); documented in `--help` + README in the
   same change.

Staleness: floors carry `computed_at` (rendered in the selection card
as "extents mapped at HH:MM"). They are **not** refreshed periodically:
external writes can flip sharing both ways (§7), but the number at the
moment of action is always the oracle's fresh read — the floor is
navigation-grade, the oracle is action-grade.

## 5. Lifecycle — the oracle

Triggered by: (a) the basket changing (debounced ~300 ms), (b) an
explicit key on the selected row (sketched `x`; final binding checked
against `keymap.rs`), (c) the delete-confirm opening (D4's pre-deletion
refresh slot). Runs on the fold thread pattern, result stamped
`(selection fingerprint, deletion epoch)` so stale results never render
(query D5 pattern).

Algorithm, per device (`st_dev` scopes physical addresses — a
`--cross-filesystems` scan must never correlate addresses across
devices):

1. FIEMAP every file in S (chunked locks as in §4). Extents with
   `SHARED` unset → bucket 1. `SHARED` set → collect
   `[fe_physical, fe_physical+fe_length)` intervals per file.
2. FIEMAP is **also** consulted for in-scan referencers *outside* S —
   but only those the scan flagged as candidates: without an eager
   address map, the oracle cannot know which outside files share with
   S. Resolution: the oracle FIEMAPs S, then classifies each shared
   interval by *reference counting within S only*, and splits the
   remainder honestly: an interval whose in-S referencers ≥ 2 and whose
   `SHARED` bit is fully explained *within S* cannot be distinguished
   from one also referenced outside — so **buckets 2 and 3/4 collapse
   to "shared: freed only if nothing outside this selection still
   references them"** unless the floor pass ran. When the eager pass
   ran (this option's default), it recorded per-file *shared bytes*
   totals scan-wide, which lets the oracle bound bucket 3: if S's
   shared intervals are also seen in the scan-wide map… — this is the
   one place B must be candid: **exact bucket 3/4 separation requires
   correlating against files outside S**, and B's oracle does it by
   FIEMAPing the *hardlink-style suspects only* when cheap, otherwise
   reporting the merged honest form "at least X (exclusive); the
   remaining Y is shared — with your selection, with other scanned
   files, or with snapshots this scan cannot see". Interval arithmetic
   at 4 KiB block granularity handles partial overlaps (reflink +
   partial COW produce misaligned extent boundaries; `btrfs fi du`'s
   own docs say set-shared "isn't as simple as adding up shared
   extents", §3).
3. Hardlink subset check: for each `(dev, ino)` group intersecting S,
   compare S∩group against the registry's full group and `nlink`
   (§4) — all-inside-and-fully-seen → participates; else named in the
   "won't be freed" line with its reason ("2 links outside the
   selection" / "1 link outside the scan").
4. Large selections: the per-file rate means a marked 1 M-file subtree
   costs ~10 s. Auto-run up to ~50k files (≈ 0.5 s); above, the card
   shows "press x to compute exact freeable (~N s)" and the confirm
   modal starts the computation with a progress line — **advisory,
   never blocking confirmation** (D6 precedent).

## 6. UI surface

- **In-bar bright segment** (tui reservation 2, delivered): each table
  row's 12-cell proportion bar renders `floor/disk` as the bright
  fraction against the dim total. Presence is a **session-level rule**
  (tier-F/H session ⇒ segments exist once floors land; never
  per-directory flicker — attack-b amendment 6). Unknown floor ⇒ plain
  bar exactly as today. No layout change: the segment recolors cells
  inside the existing `BAR_WIDTH` constraint, no beyond-bar tick.
- **Column + sort**: an optional `excl` column (`≥1.2G` rendering) with
  `SortKey::Exclusive`. This is deliberately re-litigating attack-b
  finding 2 with a *new element*: that finding killed sorting on a
  best-effort guess; the floor is a guaranteed per-entry fact whose
  only error direction is understatement, and its header carries the
  `≥` marker (caveat co-located — amendment 7).
- **Selection card**: "excl ≥ 1.2 GiB of 4.5 GiB · shared 3.3 GiB ·
  mapped at 14:02" for the selected row; the oracle result replaces
  the shared line with the bucket detail when it has run for the
  current selection.
- **Basket strip**: "5 marked · 12.3 GiB · frees ≥ 8.1 GiB (up to
  11.9)" once the oracle lands; "computing…" before.
- **Flat mode `t`**: file rows show their own floor segment (files'
  floors are self-contained — no aggregation question). Breakdown `b`
  and filter composition: floors are additive, so the same folds that
  sum `td` can sum floors per group / per filtered dir — a later
  slice, not slice 1 (§10).
- **Compression caveat** (the thesis line): when a scanned device's
  mount options contain `compress` (`/proc/self/mountinfo`, read
  once), every oracle output and the selection card append one line:
  "compressed filesystem: figures are allocated (uncompressed) bytes —
  physical reclaim may be smaller; the kernel does not expose
  compressed sizes to unprivileged users" (§2: `st_blocks`, FIEMAP,
  and `btrfs fi du` all share this blindness; only root-only
  `TREE_SEARCH_V2` knows). The existing `disk` column has had the same
  blind spot since the scan engine was built — phase 2 words it, it
  does not fix it (out of scope, named in the dossier decisions).

## 7. Deletion integration

- **Confirm modal**: the oracle's range replaces phase-1's optimistic
  freed estimate ("will free 12.3 GiB" — computed from tree
  aggregates, admittedly optimistic for surviving hardlinks, D6) with
  "frees at least X, up to Y; Z won't be freed (reasons)". The phase-1
  open-file advisory line is orthogonal and stays.
- **Floor lockstep** (attack-b amendment 3, honored): `apply_removal`
  of a subtree subtracts the subtree's `dir_floor` up the ancestor
  chain — exact, because floors are additive. One subtlety is handled
  explicitly: a hardlink group whose LCA contribution sits *above* the
  removed subtree, when the removal deletes only part of the group,
  leaves that ancestor's floor overstated. Rule: on removal, every
  registry group intersecting the removed set (the delete flow already
  looks these up for its warning) whose group is no longer wholly
  present has its LCA contribution retracted from `dir_floor` along
  the old LCA chain. Bounded by groups touched by the deletion; an
  integration test asserts `dir_floor` re-derives exactly after any
  removal sequence. Surviving reflink siblings of deleted files become
  *more* exclusive on disk (§1) — their stale floors merely understate
  until the next session: floor-safe, documented.

## 8. Filesystem tiers

Per-device, decided once post-scan by `fstatfs` on each unique
`st_dev` (the scan's `classify_mount` idiom, magic values verified in
research §5):

| tier | devices | floor source | oracle |
|---|---|---|---|
| **F** (extent) | btrfs `0x9123683e`, XFS `0x58465342`; confirmed by the first successful FIEMAP, `EOPNOTSUPP` ⇒ downgrade to H | FIEMAP exclusive bytes + hardlink LCA | full (buckets 1–5) |
| **H** (hardlink-only) | ext family `0xef53` and every other real filesystem | floor = `disk` for `nlink == 1`; LCA rule for groups (registry data only — **no ioctls, near-zero cost**) | hardlink subset check only; extents reported as "no sharing on this filesystem" |
| **Z** (nothing) | ZFS `0x2fc12fc1` (unverified live, §5) | none — `node_floor` sentinel | none; one wording line |

Tier H is honest on ext4 because ext4 has no reflink (§5): an
`nlink == 1` file's blocks are freed on unlink, full stop
(deleted-but-open is phase 1's separate advisory). Tier Z exists
because OpenZFS 2.2 block cloning is pool-level, "knows nothing about
datasets", and exposes no per-file API (§5) — a tier-H floor on ZFS
could silently overstate for cloned files, so the settled "show
nothing rather than invent" stance holds; the one line reads "ZFS does
not expose per-file sharing — no freeable figure rather than a guess".
Root-only precision (`TREE_SEARCH_V2` compressed truth): **not** part
of B; the dossier carries it as a session decision with a
reject recommendation (the privilege wall is total, §3).

## 9. Dump / diff / CLI

Nothing in dumps (extent sharing is *more* volatile than phase 1's
`/proc` state — any process's write anywhere flips it, §7 — and D7's
capability-rule argument applies verbatim). Nothing in diff. CLI: only
`--no-fiemap`/`NO_FIEMAP` (§4), documented in `--help` + README in the
same change. `--no-ui` prints no phase-2 figures in slice 1 (the
per-entry channel is row-scoped; a `--no-ui` surface would need its
own selection semantics — deferred, named).

## 10. Phasing

1. **Slice 1 — the oracle at the delete path** (this is Option A in
   miniature): confirm-modal range + basket figure. Immediate user
   value: fixes the optimistic hardlink estimate. No FloorMap, no
   segments.
2. **Slice 2 — the eager floor**: pass, FloorMap, in-bar segments,
   selection card line, `--no-fiemap`. Hardlink LCA contributions may
   land as 2b if the retraction rule (§7) wants its own review.
3. **Slice 3 — composition**: `excl` sort key + column, flat/breakdown
   /filter floor sums, oracle bucket-3 refinement.

## 11. Where to press (self-identified)

1. **The RSS bill**: +48 MB at 10 M entries for ambient data many
   sessions never look at. The sparse alternative (exception map only
   for shared/multi-link files) is cheaper on clean trees and *worse*
   on exactly the snapshot-heavy trees phase 2 exists for. Attack the
   dense-vec choice.
2. **The eager pass at 10 M files is ~2 minutes of background ioctl
   churn** (open+FIEMAP+close per file — also page-cache and battery
   noise) on every scan of a big tree, tier-F devices, by default.
   Is default-on right, or does it need a size threshold / prompt?
   The dossier carries this as a decision.
3. **Floor staleness has a lying direction after all**: an *external*
   process reflinking/deduping a scanned file after the pass makes the
   stored floor overstate (§7 volatility). Defense: the oracle re-reads
   at action time, so no *actionable* figure is ever stale — but the
   ambient segment can be wrong until then. Press on whether
   "navigation-grade vs action-grade" is a distinction users actually
   absorb.
4. **The oracle's bucket 3/4 separation** (§5 step 2) is the weakest
   mechanism: without a scan-wide address map it degrades to a merged
   "shared with something" remainder. Verify the degraded wording is
   actually honest enough to ship, or whether slice 3 must build the
   map for marked-adjacent files.
5. **`SortKey::Exclusive` re-litigates attack-b finding 2.** The new
   element is real (floor ≠ best-effort guess), but the sorted-column
   authority problem (a `≥ 0` row for a 90 GiB snapshot pair sorts to
   the *bottom*, hiding the biggest reclaim opportunity!) is an
   under-sell version of the same UX trap. Consider whether sort
   belongs in slice 3 at all.
6. **LCA retraction on partial group deletion** (§7) is the fiddliest
   invariant; attack it with adversarial deletion sequences (delete a
   link, then its dir, then re-check every ancestor floor).
