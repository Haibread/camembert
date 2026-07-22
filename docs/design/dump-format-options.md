# Dump format — options dossier for the co-design session

**Status: options for discussion, nothing decided.** This synthesizes one
research pass ([research](dump-format-research.md)), three independent
design proposals pushed to their limit
([A: JSONL/zstd](dump-format-option-a-jsonl.md),
[B: custom binary "CAMB1"](dump-format-option-b-camb1.md),
[C: SQLite](dump-format-option-c-sqlite.md)), and one adversarial review
per proposal ([attack A](dump-format-attack-a.md),
[attack B](dump-format-attack-b.md), [attack C](dump-format-attack-c.md)).

## TL;DR

| | A — JSONL + zstd seekable | B — custom binary | C — SQLite |
|---|---|---|---|
| Adversarial verdict | **Viable with fixes** | **Not viable as pitched** | **Viable with fixes** |
| Dump size (10M entries, on disk) | ~130–180 MB compressed | ~160–230 MB | ~1.1–1.25 GB (+ WAL + temp) |
| Diff 10M×10M, normal tree | streaming merge-join, 10–30 MB RSS, ~3–8 s | claimed 0.5 s, **actually ~minutes–70 min** (refuted) | 30–60 s exact, <100 MB RSS |
| Diff on hardlink-heavy tree | degrades to O(entries) RAM (~600 MB on backup farms) | — | OK at diff time, but **nondeterministic aggregates** (phantom diffs) unless fixed |
| Crash (kill -9) | best-in-class: valid unordered prefix | good frame-level story (the one strong part) | rows survive (WAL), **top-level aggregates don't** |
| Full-disk behavior (the tool's own use case!) | finalize needs 2× compressed dump (~260 MB) | fine | **worst**: 1.1 GB file + ~450 MB index-build temp + WAL growth |
| Lazy browse > RAM | works, mediocre | good (its real strength) | best |
| Incremental cache refresh | full rewrite | full rewrite | **in-place UPDATE** (unique) |
| Schema evolution | best (add a key) | flag space exhausted at v1 → TLV labyrinth | good (ADD COLUMN) |
| Interop/debuggability | `zstdcat \| jq` **empirically verified** | opaque, needs shipped tooling | `sqlite3` (not on minimal containers) |
| Engineering cost | ~2–3 person-weeks | **~13–19 person-weeks** (5–8×) | moderate |

## What the adversarial pass established

**Cross-cutting findings that reshape the problem** (these matter more than
any single option):

1. **The full-disk machine is a first-class design constraint, and all
   three proposals undersold it.** A disk analyzer runs precisely when the
   disk is full. Every design has a finalize/temp cost that can fail there
   (A: 2× compressed dump ≈ 260 MB; C: ~450 MB sort temp + WAL sidecar
   growth). Whatever we pick needs an explicit "low-disk mode" answer
   (pipe to another machine, degraded unordered dump, …).
2. **Sorted emission ⇒ streaming merge-join diff is confirmed** (mtree
   precedent + all three reviews agree the mechanism is sound). But sorted
   emission from a multithreaded scan requires a finalize pass *somewhere*
   (writer-side rewrite, receiver-side sort, or a database index). There is
   no free lunch; the choice is *where* to pay it.
3. **Hardlinks are the recurring landmine.** A's diff memory bound
   collapses on hardlink-heavy trees (Nix stores, backup farms:
   O(distinct hardlinked inodes) RAM, unevictable). C's first-seen
   attribution makes aggregates traversal-order-dependent → phantom diffs.
   Any design must fix a **deterministic hardlink attribution policy**
   (e.g. canonical owner = lowest (dev,ino) path) *in the format spec*,
   not in reader code.
4. **JSON numbers >2^53 corrupt in JS/jq** (empirically verified with a
   real 63-bit inode). Any text format must carry inodes (and arguably
   sizes) as strings, or the HTML exporter and jq-arithmetic silently lie.
5. **B's headline collapsed on placement vs access order**: completion-
   order DEB placement + DFS-order diff access = ~4 orders of magnitude
   more I/O than claimed (~6.4 TB decompressed for a 10M diff). Salvaging
   it requires the finalize reorder it was designed to avoid, at 5–8× the
   engineering cost of A — for no remaining advantage over A or C.

## Where each option genuinely wins

- **A** wins: crash tolerance, interop (verified), schema evolution,
  engineering cost, wire size, and the diff-in-bounded-memory requirement
  on normal trees. Loses: fast reopen (3–8 s parse), lazy browse
  elegance, incremental refresh, hardlink-heavy diff RAM.
- **B** wins: nothing that survives review that A or C doesn't also
  provide. Its one strong section (frame-level crash recovery) is
  portable to other designs. Recommend: **drop B**, but steal its ideas
  (CRC-protected zstd frames, PNG-style magic, `feature_mask` honesty).
- **C** wins: lazy random-access browse, ad-hoc SQL analytics
  (owner/pattern aggregation), and **in-place incremental cache refresh**
  — which is exactly the "cache honnête, rafraîchi en tâche de fond"
  feature of wave 4. Loses: size (~8× A on disk), full-disk behavior,
  musl stack trap, finalize landmines.

## Recommendation to challenge in session

**Option A (JSONL + zstd seekable) as the v1 dump/interchange format, with
the four required fixes from its review baked into the spec:**

1. Inodes (and sizes ≥ 2^53) as JSON strings.
2. Sort key defined on **raw name bytes** (encoding only for JSON
   validity), so naive consumers agree with the comparator.
3. Diff memory bound restated honestly: O(changed dirs + distinct
   hardlinked inodes); document the backup-farm case; specify the
   canonical hardlink-attribution policy in the format.
4. Low-disk story specified: `--pipe` / unordered-dump escape hatches,
   documented 2× finalize footprint.

**And keep C in the back pocket** as the *optional local cache/index* for
wave 4 (stale-cache display, background refresh, owner/pattern SQL) — a
derived artifact built *from* dumps, never the interchange format. This
splits the two jobs the options were fighting over: A is the portable
truth (scp, diff, ncdu import, HTML export), C is a local accelerator
(regenerable, deletable).

Rationale: the MVP + wave 2 features (dump v1, diff, ncdu import,
non-interactive mode) are all served best or well by A at a fraction of
the cost; nothing in waves 1–3 needs what only C provides; and B is
strictly dominated.

## Decisions needed in the co-design session

1. **Accept the A + C-later split?** Or challenge: is lazy browse of
   bigger-than-RAM dumps (A's weakest axis) actually needed before wave 4?
2. **Hardlink attribution policy** (format-level, affects diff
   correctness): canonical owner = first in sorted path order? lowest
   inode? Must be deterministic and specified.
3. **Sort key**: raw bytes (recommended post-review) vs encoded form —
   affects every future interoperating tool.
4. **Numbers as strings**: inodes only, or also sizes >2^53?
5. **Low-disk default**: refuse-and-suggest-pipe, or auto-degrade to
   unordered dump?
6. **Name the format** (file extension, magic line) and the minor-version
   floor for the MVP: which fields are v1.0 vs deferred?
