# Adversarial review — Option B "CAMB1"

> Produced by an adversarial agent instructed to demolish the proposal.
> Verdict: **NOT VIABLE AS PITCHED**.

## FINDING 1 — The diff is not 0.5 s; it's tens of minutes [FATAL, high confidence]

Setup (the proposal's own numbers): 10M entries, ~1M dirs, ~255 MB
uncompressed DATA, 4 MiB frames → ~60 frames/file, ~15,600 DEBs/frame.

The contradiction: DEBs are placed in **completion order** (G4), the diff
walks in **name-sorted DFS order** (§6) — uncorrelated. Fetching one DEB
decompresses a whole 4 MiB frame of ~15,600 unrelated DEBs.

The count: fetches ≈ 2M (1M matched dir pairs × 2 sides); frame cache =
96 MB / 4 MiB = 24 frames vs 120; miss rate ≈ 0.8 → ~1.6M frame
decompressions × 4 MiB ≈ **6.4 TB decompressed** → **~70 minutes** at
1.5 GB/s, not 0.5 s. "Reads both files once" would be ~120 decompressions;
the real figure is ≥1M — **off by ~4 orders of magnitude**.

Steelman killed: thread-local post-order flushing gives subtree locality
*per thread*, but work-stealing — the format's explicit selling point —
splits big subtrees across threads and interleaves their DEBs. Even a
charitable 5 % miss rate → ~400 GB → 4–5 min, still 500× the claim. zstd
DATA frames aren't seekable, so `offset_in_frame` requires decompressing
the frame prefix anyway.

The "<150 MB RSS" figure is true *because* the cap is enforced — and
enforcing the cap is what causes the thrash. RSS and 0.5 s cannot both
hold; presenting them jointly is the cherry-pick.

Only fix: finalize-time physical reorder of all DATA into name-sorted-DFS
order — a full read+rewrite contradicting G4 and the zero-coordination
pitch. dir_id (≈BFS discovery) order does not help: BFS is not
subtree-contiguous.

## FINDING 2 — "Stream a deleted subtree via the index": same blowup [MAJOR, high]

A deleted 100k-dir subtree (an `rm -rf`'d dataset — the common case) =
100k random DEB fetches → tens of thousands of 4 MiB decompressions for
one subtree.

## FINDING 3 — The 96 MB writer bound is false for big directories [MAJOR, high]

G2 requires DEB contiguity; §3 allows DEBs up to 4 GiB. A thread building a
giant DEB cannot flush until complete: worst-case per-thread memory is
N × (largest serialized DEB) — the proposal's own weakness #4 admits a
~250 MB DEB. The claimed bound needs the asterisk *"constant only if no
directory exceeds ~150k children"*. As stated it's wrong. (The full child
list held before serialization relocates the same unbounded cost into the
scan arena.)

## FINDING 4 — Bytes/entry optimistic [MINOR, high]

Field-by-field correction: name 12–14 B (not ~11 — front-coding only
shares with the previous sibling, resets every 16, shares nothing across
dirs; `node_modules`-style names share almost nothing), asize 3–4, mtime
2–4 → **~27–31 B/entry**, not 25–26. zstd recovers most of it on disk;
the in-flight buffers are what's mis-sized.

## FINDING 5 — Size loses to the real competitor; browse ceiling unit-slip [MINOR, med-high]

- Compressed realistically 160–180 MB; weakness #7 already concedes ~2×
  larger than gzipped ncdu JSON (~100 MB) — the "2.6× smaller" headline is
  vs *uncompressed* JSON, a strawman. For the scp use case CAMB1 ships
  bigger dumps than JSONL+zstd.
- 50M entries → ~200 MB uncompressed mmap summary table: hostile to scp.
- FINAL-INDEX realistically 6–8 B/dir (offsets are a random permutation —
  delta-varint doesn't compress them), not 5.
- "200 MB budget fails around ~300M directories" is a unit slip: at 5 B/dir
  that's 40M dirs; real ceiling nearer ~250M *entries*. The numbers weren't
  re-derived.

## FINDING 6 — kflags is exhausted at v1 [MAJOR, high]

kind(3) + has_ext + hardlink + error + has_ext2 + rsvd = 8 bits with ONE
reserved. HANDOFF already queues btrfs extents, atime, quotas, per-owner
stats, compression ratio — 5+ fields. Within one release cycle everything
routes through nested TLV: the parser labyrinth, each field hand-written
encode/decode + fuzz cases, 2–3 B/field tag overhead eroding the size
advantage. JSONL wins this axis outright.

## FINDING 7 — Crash recovery genuinely strong [survives, with notes]

"CmbF" false positives: ~0.06 expected over 60 frames, but resync is gated
on CRC (2^-32) → negligible — provided the implementation never accepts a
resync without full CRC. Adversarial corruption can silently skip a valid
frame ("one torn frame" bound becomes "or more under adversarial
corruption"). Recovery-walk timing plausible. Still needs `repair` before
mmap-grade browse.

## FINDING 8 — Engineering estimate ~2× optimistic [MAJOR, med]

Realistic solo budget: writer+reader core 3–4 wk, recovery+repair 2,
streaming-cursor diff with eviction/resume 3–4 (genuinely hard),
fuzzing+spec+golden corpus 2–3, `dump cat`+verify+stat 1–2, data-loss-class
edge cases 2–4 → **~13–19 person-weeks ≈ 5–8× JSONL**, front-loading the
highest-risk component (the diff machinery Finding 1 shows doesn't
deliver). Two-tier open (clean vs repair) is a standing support-ticket
generator.

## FINDING 9 — Strictly dominated on nearly every claimed axis [FATAL as conclusion, high]

| Requirement | Dominated by | Why |
|---|---|---|
| Diff under real placement | SQLite | (parent_id,name) index; OS page cache LRU at 4 KB granularity, no 4 MiB-frame thrash |
| Schema evolution | JSONL | additive keys, no flag budget |
| Tooling/opacity | JSONL | jq/grep everywhere |
| Crash simplicity | JSONL / SQLite-WAL | no torn-frame logic, no repair |
| Impl cost | JSONL | 5–8× cheaper |
| Compressed size (basic) | JSONL+zstd | ~2× smaller |

Only uncontested win: mmap O(1) child-total reads during browse — which
SQLite also delivers via an indexed aggregate column. Browse-at-50M
survives (~130–150 MB plausible) but never needed a bespoke format.

## Verdict: NOT VIABLE AS-IS

Viable only with major surgery (mandatory finalize reorder) that guts the
zero-coordination streaming identity. If the honest diff answer is
"minutes, cache-bound", the format has no advantage over SQLite or
JSONL+zstd on any axis, at 5–8× the cost: a 4-month detour to a worse
version of both.
