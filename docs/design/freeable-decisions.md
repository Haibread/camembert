# Freeable phase 1 — decisions (co-design session, 2026-07-23)

Outcome of the co-design session over the
[options dossier](freeable-options.md) and the three
[attack](freeable-attack-a.md) [reports](freeable-attack-b.md)
[(c)](freeable-attack-c.md). Settled; reopening one requires a new
element. Covers HANDOFF next-step "Freeable column, phase 1"
(deleted-but-open files); btrfs shared extents and hardlink siblings
remain phase 2.

## D1 — Shape: sweep ledger (Option A), amended

Phase 1 is **Option A**: the post-scan `/proc` sweep produces a
scan-level side artifact (the *ledger*) — never tree nodes, never
per-directory aggregates, never dump records. Every number rendered in
the tree remains scanned-filesystem truth; the ledger renders as
kernel-reported evidence (paths + guilty PIDs), not tree-grade numbers.
Options B (annotated tree) and C (ghost rows) are rejected — C outright
(two fatal findings: unreachable core API as pitched, and markable
ghosts whose name-reuse collision can unlink an unrelated live file),
B because its per-directory channel is the wrong substrate for phase
2's non-additive sources and its `+N` column promotes best-effort
attribution to ranking authority.

The **full amendment list of freeable-attack-a.md is binding**; the
load-bearing ones are D2 and D6.

## D2 — Scope: root-filesystem only, honest about the rest

The headline figure counts only deleted-open files whose `st_dev`
equals the **scan root's** filesystem — the same filesystem the
`statvfs` disk gauge describes — so freeable is always a coherent
subset of that gauge's `used`. Under `--cross-filesystems`, files held
on other crossed devices appear in the panel (labeled with their
filesystem) but are **excluded from the gauge suffix** — never a
"30 GiB freeable" against a 20 GiB disk. Known documented gap: btrfs
multi-subvolume layouts share one pool across several `st_dev`s; the
root-subvolume scoping under-counts there, and the panel says so
rather than silently reassuring.

## D3 — Ground truth: nlink==0, (dev,ino) dedup, st_blocks

A file is deleted-open iff `fstatat` through `/proc/[pid]/fd/N` yields
`st_nlink == 0` — the `(deleted)` readlink suffix is per-dentry
ambiguity, display-only. Entries are deduplicated by `(st_dev,
st_ino)`; sizes are `st_blocks * 512` (allocated, sparse-correct).
memfd/tmpfs/devtmpfs-backed entries are **not disk space**: they are
excluded from every disk figure and shown in the panel as one separate
"RAM-backed (memfd/shm), not disk" line, so `lsof +L1` users
understand the difference rather than suspecting a miss. Known
limitation, stated in the panel when relevant: mmap-only holders
(no fd) are invisible without `CAP_SYS_ADMIN` (`map_files`).

## D4 — Sweep lifecycle: scan end + pre-deletion, never during scan

One sweep runs when the scan completes (off the UI thread; the UI
consumes the result via the existing snapshot/notification machinery).
The same sweep machinery, unfiltered (all open files, not just
nlink==0), refreshes **before the delete-confirm modal opens** to
power the open-file warning by `(dev, ino)` match against marked
entries. No sweeping during the scan (the tree is still moving;
process state would be stale by scan end anyway). No periodic
background sweeps.

## D5 — UI: gauge suffix + `f` panel + thresholded toast

- **Headline**: a suffix on the existing disk-gauge line ("… · 1.2 GiB
  freeable"), clickable → opens the panel. No fifth metric card.
- **`f` panel**: floating modal (same family as the `v` review list)
  listing deleted-open files — evidence path (with `(deleted)`
  annotation), holder PID + `/proc/[pid]/comm` name, allocated size —
  grouped display-only under the deepest still-existing ancestor
  directory, largest first; a coverage line ("N of M processes
  readable — run as root for the full view" when applicable); the
  RAM-backed line (D3); the cross-filesystem section (D2). Modal
  precedence joins the slice-4 ladder: confirm > review > freeable
  panel > cheatsheet.
- **Toast**: at scan end, if the root-filesystem freeable total is
  ≥ 100 MiB **and** ≥ 1 % of the filesystem's capacity, one toast:
  "1.2 GiB freeable by closing files — f". Both bounds so small disks
  aren't nagged about crumbs and big arrays aren't nagged about
  rounding noise.

## D6 — Deletion warning: advisory, coverage-honest

The confirm modal shows "N marked entries are open in M processes
(...)" as an **advisory** — it never blocks confirmation. When the
sweep's process coverage was partial, the warning carries the same
caveat as the panel ("open-file check saw K of M processes") so an
absent warning is never false reassurance on a multi-user machine
(attack A serious finding). Non-interactive paths (`--output -`,
`--no-ui`) print nothing for freeable on stdout — stdout may be a
dump stream; the hint lives in the TUI panel only.

## D7 — Surface: `--no-proc-sweep` (env `NO_PROC_SWEEP`), no dump keys

One new flag, `--no-proc-sweep` (env `NO_PROC_SWEEP`, presence
semantics like `NO_MOTION`), disables both the scan-end sweep and the
pre-deletion refresh — for paranoid environments and containers with
masked `/proc`. `/proc` absent or unreadable degrades silently to
"no data" (debug-level trace only). **No freeable keys in dumps**:
open-file state is process state, instantly stale, and dump-v1
capability rules would demand header surface for a value diff ignores.
A dump-loaded session simply has no ledger.

## D8 — Module boundary

The sweep lives in a new `camembert-core` module (`freeable.rs`) with
zero changes to `tree.rs` / view snapshots / dump / diff — attack A
verified this isolation holds in the current code. The UI consumes a
plain `FreeableLedger` value. Phase 2's per-entry sources (FIEMAP,
hardlink siblings) will design their own per-entry channel and the
reserved in-bar bright segment; the ledger and that channel compose
(gauge sums both; the panel stays the deleted-file drill-down).
