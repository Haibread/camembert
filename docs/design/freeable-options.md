# Freeable column, phase 1 — options dossier for the co-design session

**Status: draft — awaiting adversarial review and the co-design
session.** Synthesizes the research pass
([freeable-research.md](freeable-research.md)) and three design
proposals pushed to their limit
([A: sweep ledger](freeable-option-a-ledger.md),
[B: annotated tree](freeable-option-b-annotated.md),
[C: ghost rows](freeable-option-c-ghosts.md)).
Addresses HANDOFF "Suggested next steps" §1 and the original vision in
[handoff-original.md](handoff-original.md) §"Colonne « libérable »"
(deleted-but-open with guilty PID now; btrfs `FIEMAP_EXTENT_SHARED` and
hardlink siblings in phase 2; ZFS: show nothing rather than invent).

## Problem statement

The classic lie phase 1 corrects: `df` says the disk is full, `du` (and
every du-style tool, including camembert today) says it isn't, because
processes hold deleted files open. The research confirmed the mechanics
(`/proc/[pid]/fd`, `st_nlink == 0` as ground truth, `(dev, ino)` dedup,
`st_blocks × 512` sizing, ~37 ms sweep, no mainstream analyzer does
this — research §1–§6, §8). What remains is design: **where do bytes
that have no path in the tree live in a tool whose entire UI is a
tree**, under the product thesis *honest answers, never invent*.

## Common ground (all three options, settled by the research)

Not axes — any option ships these identically:

- Filter: fd-held regular files, `st_nlink == 0` (never the
  `" (deleted)"` string), `O_TMPFILE` included; dedup `(dev, ino)`;
  size `st_blocks × 512`; scope decided by `st_dev` ∈ scanned devices,
  never by path text (research §1, §2, §3, §5).
- memfd/shm/anon excluded from every disk figure (RAM, not disk —
  92% of deleted-marked fds on the reference desktop, research §2).
- mmap-only holders (no fd) scoped out: `map_files` needs
  `CAP_SYS_ADMIN`, and the `maps` range length is a guess (research
  §2, open question 2). Named in docs as a coverage gap.
- Loop devices, unlinked directories: scoped out (open question 6).
- Paths handled as raw bytes end to end; ENOENT mid-sweep skipped
  (TOCTOU, research §6); `/proc` absent → feature degrades to
  "unavailable", scan unaffected (research §4).
- Sweep at scan completion on a background thread + refresh on demand;
  never during the scan; the same walk unfiltered
  (`open_file_index`) re-runs when the delete-confirm modal opens and
  fills in the open-file warning asynchronously (research §7).
- Permissions honesty: a single coverage line where the user is
  already looking at detail ("365 of 505 processes unreadable — run
  as root for the full picture"), no nagging elsewhere (research §4:
  unprivileged desktop users see their own big consumers; server
  sysadmins need root).
- CLI: one flag, `--no-proc-sweep` (env `NO_PROC_SWEEP`), for audited
  environments and masked-/proc containers; documented in `--help` +
  README in the same change.

## The real axis

Where the deleted-but-open bytes surface, and what data structure
carries them:

| | A — sweep ledger | B — annotated tree | C — ghost rows |
|---|---|---|---|
| Attribution shown | filesystem-level only; paths as evidence rows | filesystem-level + best-effort per-directory column | filesystem-level + synthetic rows in last-known dirs |
| Can a wrong number/row reach the tree? | **no** | yes (wrong-but-plausible `+N` on a dir) | yes (fake row in a real dir) |
| Data model | scan-level report struct; arena untouched | report + `DirId → bytes` side aggregates (excluded-map pattern) | report + synthetic arena nodes + ghost side set |
| Snapshot / UI-thread impact | none | `Row`/`DirTotals` gain a field; post-scan builder lookup | rows flow through existing snapshots; every consumer must filter ghosts |
| UI surface | disk-gauge suffix + `f` evidence panel (grouped display-only) | freeable column (`+N`, outside the bar) + sort key + selection card | dimmed rows in the table + selection card |
| Dump impact | none | none structural; proposes optional `e`-line keys `fb`/`fbn` (severable) | none, but the writer must filter ghosts forever |
| Deletion-warning reuse | same module, second filter | same | same |
| Phase-2 fit | reserves per-entry channel for when per-entry truth exists | builds the per-entry/per-dir channel now | dead end — phase 2 attaches to real entries, ghosts don't help |
| Consumers needing changes | UI only | UI + view.rs | dump writer, diff, delete, flat view, every future traversal |
| Est. relative cost | 1× | ~2× | ~2.5× + a permanent tax on every future feature |
| Main risk | discoverability (one gauge line + one key) | invented directory numbers erode trust | tree stops being the filesystem |

## Where each option genuinely wins

- **A** wins honesty (the only option in which every number the tree
  shows remains a scanned-filesystem fact), isolation (no arena, view,
  dump, diff, or delete changes), and cost. Loses: no in-table hint —
  the user must notice the gauge or press `f`.
- **B** wins the workflow question ("which directory, which PID" read
  directly off the table), and is the only option that pre-builds the
  aggregation channel phase 2 needs. Loses: it is precisely the
  wrong-but-plausible number the thesis forbids (research open
  question 3 asked this verbatim), and it doubles the UI surface
  before an adversarial pass has vetted the attribution rules.
- **C** wins discoverability — one glance, zero new concepts. Loses:
  every structural invariant. The dump/diff/delete/flat-view filter
  obligations (option C §6) are a permanent tax, the arena leaks on
  refresh, and phase 2 gets nothing back. The strongest case reads
  well and costs the most.

## Recommendation to challenge in session

**Option A — the sweep ledger — with its panel grouping evidence rows
by deepest still-existing ancestor (display-only, raw paths visible).**

Reasons, in thesis order:

1. **Honesty is the differentiator, and only A is airtight.** The
   filesystem-level total is the one number the research showed can
   always be computed correctly (`st_dev` scoping, research §3). B's
   per-directory figures and C's placed rows are best-effort claims
   rendered in the same visual language as scanned truth; the thesis
   says a wrong-but-plausible number is worse than no number. A still
   answers "where/who" — as kernel-reported evidence text with guilty
   PIDs in the panel, which is what the `lsof +L1` workflow actually
   needs (research §8) — it just refuses to *aggregate* the evidence
   into tree-grade numbers.
2. **The gauge is the semantically correct home.** Freeable-by-close
   bytes are reclaimed per filesystem, not per directory (research
   §3), and the disk gauge is the existing filesystem-truth line —
   whose "scan covers N% of occupied" residual these bytes partially
   explain. The feature lands where the question it answers is asked.
3. **Phase 2 is served better by *not* building its channel early.**
   Phase 2's per-entry sources (FIEMAP shared extents, hardlink
   siblings) have their own access patterns (per-file ioctls, lazy
   evaluation, selection-dependent hardlink math) that should shape
   the per-entry freeable channel and the reserved in-bar bright
   segment. Phase-1 deleted-open bytes are structurally different —
   *not in the tree, not a fraction of any bar* — and wiring them
   into a per-entry channel now would prejudge phase 2's design with
   the one source that doesn't fit it. Ledger and future per-entry
   channel compose (gauge line sums both; panel stays the deleted-file
   drill-down); nothing gets unbuilt.
4. **Cost and blast radius.** A touches the UI and one new core
   module. B touches view plumbing; C touches everything and taxes
   every future traversal. For a feature whose adversarial review is
   still pending, the smallest honest shape is the right opening bid.

Mitigation for A's real weakness (discoverability): a one-time toast at
scan completion when the sweep finds ≥ a threshold ("1.2 GiB freeable —
press f"), plus the clickable gauge suffix. Whether the toast is
welcome or a nag is a session call (decision 3).

## Decisions needed in the co-design session

1. **Accept the ledger, or fight for the directory column?** The core
   trade: is a best-effort `+N` per directory (B) worth the first
   invented number in the tree, or does the panel's display-only
   grouping deliver enough "where"? (Research open question 3,
   verbatim.)
2. **Gauge suffix vs a fifth metric card** for the headline number.
   The cards row is currently exactly four (`total real · entries ·
   errors · hardlinks`); a fifth card is louder but crowds narrow
   terminals and duplicates the gauge's filesystem framing.
3. **Scan-end toast**: yes (discoverability) or no (nag)? If yes,
   threshold?
4. **memfd/shm**: one separate "RAM-backed, not disk" line in the
   panel (recommended — explains what `lsof` users will see missing)
   or fully omitted (research open question 1)?
5. **`--no-proc-sweep`**: accept the single flag, or ship zero new
   surface and rely on `/proc`-absent degradation?
6. **B's `e`-line keys** (`fb`/`fbn` freeable summary in dumps):
   strike entirely (recommended — process state in a filesystem
   snapshot) or keep as informational-only minor addition?
7. **Confirm-modal open-file warning**: advisory fill-in (recommended)
   or block confirmation until the sweep lands?
8. **Root guidance**: is the panel coverage line enough, or should
   `--no-ui` output also print the "run as root" hint on servers
   (where unprivileged sweeps see almost nothing that matters,
   research §4)?
