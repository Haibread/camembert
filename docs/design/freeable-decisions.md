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

## Condensed reasoning trail

Condensed from the eight source documents of the freeable phase 1
dossier (research, options, three option pitches, three adversarial
attacks) after the co-design session settled D1–D8 above. Originals are
recoverable from git history (see closing line). This trail preserves
reasoning and every attack finding number; it does not restate the
decisions, which remain the binding artifact above.

### Research

Digest gathered 2026-07-23 (`freeable-research.md`) via man pages,
kernel docs/patches, `lsof`/`lsfd` source, and live experiments on a
CachyOS desktop (505 processes). No recommendations, facts only.

- **§1 Enumeration**: `/proc/[pid]/fd/N` is a magic symlink; plain
  `fstatat`/`stat(2)` (no `AT_SYMLINK_NOFOLLOW`) dereferences to the
  live inode even post-unlink, giving true `st_size`/`st_nlink`
  (confirmed live; GNU `stat(1)` needs `-L` to do the same — a
  coreutils convention, not a procfs quirk). The kernel's `" (deleted)"`
  suffix reflects *this dentry's* unlink, not the inode's aggregate
  link count — a hardlinked file still shows the suffix on one link
  while `st_nlink > 0` (reproduced live), and a file legitimately named
  `foo (deleted)` produces an identical-looking string with
  `st_nlink == 1` (kernel docs call this ambiguity out explicitly).
  **Conclusion: never string-match the suffix; `st_nlink == 0` is the
  only ground truth.** Renames are tracked live in the readlink target,
  not frozen at open time. Bind-mount/namespace `"(unreachable)"`
  prefixing is real and kernel-documented (not reproduced live).
  overlayfs is fine; fuse-overlayfs (rootless Podman) makes
  deleted-but-open files fully inaccessible through the fd — a coverage
  gap, not a false positive.
- **§2 What holds space**: live sweep breakdown — memfd (603) and
  shm/anon (418) are RAM-backed, never disk, and dominate the raw
  `(deleted)`-marked fd count; `O_TMPFILE` files (3) are genuine
  disk-backed deleted-from-birth files; regular `st_nlink==0` files (82
  fds → 66 unique `(dev,ino)`, ~117.6 MiB deduped) are the actual
  target; 3 entries carry the suffix with `nlink > 0`, confirming the
  ambiguity is live, not theoretical. mmap-only deleted files (no fd)
  are visible via `/proc/pid/maps` but authoritative sizing via
  `map_files` needs `CAP_SYS_ADMIN`/`CAP_CHECKPOINT_RESTORE` — `maps`'
  mapped-range length is a usable but page-rounded lower-bound proxy
  when that's unavailable. Loop devices and unlinked directories: not
  tested, low value, out of scope. `lsof +L1` already filters on
  `nlink < 1` correctly but doesn't separate memfd/shm from disk files
  or walk `map_files`.
- **§3 Attribution**: after unlink there is no tree path, so the
  readlink string is the only textual link back — and it can be
  non-UTF-8 (raw kernel bytes), ambiguous, or post-rename-truthful but
  tree-unfamiliar. `st_dev` match against the scan's device set is the
  robust, path-independent scope decision (disk space is reclaimed
  per-filesystem, not per-directory). Practical implication: bytes are
  trustworthy at the **filesystem-total** level, only *heuristically*
  attributable to a **directory**.
- **§4 Permissions**: reading `/proc/[pid]/fd/*` is gated by ptrace
  access mode `PTRACE_MODE_READ_FSCREDS` (same-UID or
  `CAP_SYS_PTRACE`), independent of `hidepid`. Quantified: of 505
  processes, only 140 (28%, this user's own) were readable; 365 denied.
  An unprivileged desktop user still sees their own big consumers; an
  unprivileged sysadmin on a multi-user server sees almost nothing that
  matters (the standard "run lsof as root" advice, §8). `/proc` absent
  (chroot) should degrade the feature to "unavailable," not fail the
  scan.
- **§5 Dedup/sizing**: the same deleted inode can be held open by
  several PIDs at once (measured: one leveldb file held by 5 renderer
  PIDs) — dedup key is **`(st_dev, st_ino)`**, not per-fd. Size is
  **`st_blocks × 512`**, confirmed sparse-correct (a 1 GiB sparse file
  with 4 KiB written reports `st_size=1G` but `st_blocks*512=4096`,
  the true freeable amount); `st_size` would wildly overstate. A sweep
  is independent point-in-time snapshots per fd, no cross-process
  locking, same staleness any du-style tool already lives with. 512-byte
  block units are POSIX-fixed, no per-filesystem lookup needed.
- **§6 Cost**: O(processes × fds) readdir+readlink+fstatat, no
  recursion. Measured: 505 processes / 6559 fds / 37 ms single-threaded
  Python, 66 unique deleted files / ≈117.6 MiB. Cross-checked: `lsof
  +L1` took 226 ms for its narrower output; `lsfd` took 747 ms for a
  much bigger job (all 72,251 open-file-table entries). A dedicated
  sweep is trivially fast and needs no threading to stay off the UI
  path. TOCTOU (process dies mid-sweep) confirmed benign: plain ENOENT,
  skip.
- **§7 Reuse for delete-warning**: the same walk, unfiltered (no
  `st_nlink` filter), yields every open file's `(dev,ino)` — exactly
  the input the deletion open-file warning needs, one pass, no extra
  syscalls or permissions.
- **§8 Prior art**: `lsof +L1` is the closest tool (filters `nlink<1`
  correctly, per source); `lsfd` has an explicit `DELETED` boolean
  column but is a process inspector, not a du-style aggregator, and no
  dedup/total. `lsof-org/lsof` issue #65 shows this is a recognized,
  unsolved pain point. No mainstream disk-usage tool (ncdu, gdu,
  dua-cli, pdu, baobab, WizTree, filelight, WinDirStat) surfaces
  deleted-but-open files today. Standard sysadmin workflow: `lsof +L1`
  as root → identify PID/path → restart the holder or truncate through
  the still-open fd (`> /proc/PID/fd/N`) without disrupting the
  process — the workflow camembert's guilty-PID display should make
  discoverable.
- **Open design questions raised** (not decisions): (1) surface
  memfd/shm as a separate "RAM, not disk" line or omit — settled D3
  (separate line); (2) skip mmap-only or attempt a degraded estimate —
  settled D3 (skip, documented gap); (3) is a wrong-but-plausible
  directory number worse than none — settled D1 (yes; Option A, no
  per-directory number); (4) privilege-escalation guidance needed at
  all — settled D5 (coverage line only, no nagging); (5) same sweep or
  separate for the delete-warning — settled D4 (same machinery,
  refreshed at pre-deletion time); (6) attempt loop-device/unlinked-dir
  cases — settled (scoped out, common ground across all options).

### Options

Three design pushes (`freeable-option-a-ledger.md`,
`-b-annotated.md`, `-c-ghosts.md`) synthesized in
`freeable-options.md`, sharing a common core (fd-held `st_nlink==0`
regular files incl. `O_TMPFILE`, `(dev,ino)` dedup, `st_blocks×512`
sizing, `st_dev`-set scoping never path-text scoping, memfd/shm
excluded from disk figures, mmap-only/loop/unlinked-dir out of scope,
sweep at scan-end + unfiltered pre-deletion refresh, one
`--no-proc-sweep`/`NO_PROC_SWEEP` flag). The axis: where the bytes
surface and what carries them.

- **Option A — sweep ledger.** Core idea: freeable is a
  **filesystem-level** fact, not a directory fact; model it as a
  standalone scan-level report struct (`FreeableSweep`) with zero
  arena/view/dump coupling, surfaced as a disk-gauge suffix plus an
  `f` evidence panel (kernel-reported paths + guilty PIDs, grouped
  display-only by deepest live ancestor — never a byte claim). Pros:
  the only option where every tree number stays scanned-filesystem
  truth; verified isolation (new `freeable.rs` alone); cheapest;
  composes cleanly with phase 2's future per-entry channel without
  prejudging it. Cons: no in-table hint — discoverability depends on
  noticing the gauge suffix or a toast. **Won**: honesty (research
  showed only the filesystem total is always correct, §3), isolation,
  and not building phase 2's channel prematurely (phase-2 sources are
  non-additive, confirmed by attack-b finding 1 against this very
  option's rejected rival). Adopted as D1, amended per attack-a.
- **Option B — annotated tree.** Core idea: attach every attributable
  deleted inode to its deepest still-existing ancestor directory via a
  component-wise raw-byte path walk (dropping the ambiguous final
  component, requiring `dev` agreement), surfaced as a `+N` column
  with its own sort key, plus a gauge-line residual split for
  unattributed bytes. Pros: answers "which directory, which PID"
  directly in the table; its `dev`-scope decision genuinely caps any
  path-attack's blast radius at "wrong row," never "wrong total";
  pre-builds a per-directory aggregation channel. Cons: phase-2's
  actual sources (btrfs shared extents, hardlink siblings) are
  **non-additive** — summing per-directory partial sums up the
  ancestor chain cannot represent them, so the channel would have to
  be torn out, not reused (attack-b finding 1); the dedicated sort key
  promotes a best-effort guess to ranking authority with no in-band
  caveat (finding 2); directory-name recycling (e.g. a recreated
  Postgres OID directory) lets a correct-total-but-wrong-row number
  reach the table with every scope guard green (finding 4). **Lost**:
  its phase-2 justification was backwards and its headline `+N`
  column/sort key is exactly the wrong-but-plausible number the
  product thesis forbids. Rejected in D1.
- **Option C — ghost rows.** Core idea: make each attributable
  deleted-but-open file a synthetic, dimmed row inserted into the
  frozen arena at its last-known location — maximal discoverability,
  reusing the table itself as the only UI concept, with the row
  contributing zero to every aggregate. Pros: one glance, no new
  panel/column/gauge suffix to learn. Cons: the "almost nothing new"
  pitch is false — inserting a row post-scan needs new **public**
  core mutation API that doesn't exist today (only `pub(crate)`
  primitives), so it's a second, larger mutation path than the
  designed single-owner arena contract (attack-c FATAL-1); worse, a
  ghost is markable and deletable *today* with no refusal guard, and a
  last-known-name collision with a newly-created real file at the
  same path lets the user's delete confirmation **unlink the real
  file** — silent data loss, not just a wrong number (FATAL-2); zero
  aggregates break the `%` column, identity-color ranking, and the
  wheel unless each is special-cased (SERIOUS-3); deleting a ghost's
  containing directory tombstones it and silently drops freeable bytes
  that were never actually freed (SERIOUS-4); every current and future
  arena consumer (dump writer foremost) needs a permanent skip-ghost
  clause (SERIOUS-5). **Lost decisively**: two FATAL findings — an
  unreachable core API as pitched, and a data-loss unlink path via
  name-reuse. Rejected outright in D1.

### Attack findings

#### A — `freeable-attack-a.md` (verdict: survivable with amendments; full amendment list binding per D1)

1. **accepted, resolved by D2.** Under `--cross-filesystems`, the
   gauge's single-device `statvfs` and the sweep's multi-device
   `st_dev` scope diverge — a cross-mount deleted WAL file could make
   the gauge print "30 GiB freeable" against a 20 GiB, 90%-used disk.
   Resolution: the gauge suffix is scoped to the root filesystem's
   `st_dev` only; other crossed devices' freeable bytes move to the
   panel, never summed onto the root gauge.
2. **accepted, resolved by D2.** Default btrfs subvolume layouts
   (`@`, `@home`, …) each carry their own `st_dev` but share one
   physical free-space pool, so a deleted file on a sibling subvolume
   is silently dropped by the device filter while the gauge's
   whole-pool `statvfs` still counts it — an undercount that reads as
   false reassurance. Resolution: D2 states this as a known documented
   gap in the panel/README rather than hiding it.
3. **accepted, resolved by D6.** The delete-confirm warning reuses the
   same ptrace-gated `open_file_index` walk as the panel but, as
   pitched, carried none of the panel's coverage caveat — an
   unprivileged user marking a file held by another user's process
   (e.g. `postgres`) would see EACCES-driven silence read as "nothing
   is open," the exact multi-user case the warning exists for.
   Resolution: D6 gives the confirm-modal warning the same "N of M
   processes unreadable" caveat as the panel.
4. **accepted, resolved by D6.** The non-blocking advisory warning can
   lose its own race on large process counts (37 ms scales to ~0.7 s+
   at 10k processes) — a fast confirm keystroke can land before the
   warning arrives, exactly on the big server where a wrong deletion
   is most expensive. Resolution: D6 states the warning is advisory
   and may not have landed yet, rather than presenting non-blocking as
   a pure UX kindness.
5. **accepted, resolved by D7.** The `--no-ui` summary line was never
   reconciled with `--output -` (dump to stdout): every other stdout
   line is gated behind `!dump_to_stdout` precisely to avoid injecting
   text into a zstd dump stream, but the freeable line's lifecycle
   section never mentioned that mode. Resolution: D7 states no
   freeable output on stdout in dump/non-interactive paths; the hint
   lives in the TUI panel only.
6. **accepted, implementation note.** The doc's claim that the
   scanned-device set is already available (`DirMeta.dev` "already
   carries" it) is wrong — the owner tracks no aggregated device set
   today; one must be materialized (cheap: one insert per directory).
   Resolution: accepted as a correction to the cost estimate, not a
   design change; folded into D8's module boundary.
7. **accepted, resolved by D5.** Discoverability is worse than the
   pitch admitted: zen mode hides the gauge entirely (zero surface),
   and appending the freeable suffix to an already-full gauge line
   drives the bar width to zero on an 80-column terminal. Resolution:
   D5 ships the thresholded scan-end toast as the primary
   discoverability mechanism, not just the gauge suffix.
8. **accepted, resolved by D8 (implementation note).** No interactive
   dump-viewer exists today so the sweep's `Phase::Done` trigger is
   currently safe, but a future ".cmbt-in-TUI" mode reopening a dump
   made on the same machine/filesystem would match live `/proc` state
   against a stale historical tree. Resolution: noted as a latent trap
   to guard (tie the sweep to a live-scan marker, not `Phase::Done`)
   before such a viewer is built; no phase-1 behavior change needed
   since no such mode exists yet.
9. **accepted, documentation note.** "The gauge sums both layers" in
   phase 2 glosses over the fact that a deleted-but-open file can also
   have btrfs CoW-shared extents already counted in its `st_blocks`,
   risking double-counting with a future phase-2 shared-extent figure.
   Resolution: D8 flags this as a "modulo shared extents" caveat for
   phase 2's design, not a phase-1 change.

#### B — `freeable-attack-b.md` (verdict: survivable with amendments as a standalone option, but its case for existing as more than a column collapsed; Option B rejected wholesale in D1)

1. **[SERIOUS] rejected: decisive in D1's rejection of Option B.**
   B's central pitch — build the additive per-directory channel now so
   phase 2 "adds sources to an existing pipe" — is backwards: btrfs
   shared extents and hardlink siblings are non-additive (freeable of
   a union of subtrees isn't the sum of their parts; two snapshots
   sharing 90 GiB would each wrongly show `+90 GiB`, the parent
   `+180 GiB`), so an additive per-dir scalar model would have to be
   torn out, not extended, when phase 2 lands.
2. **[SERIOUS] rejected: decisive in D1's rejection of Option B.** The
   dedicated `SortKey::Freeable` gives a best-effort, admittedly
   sometimes-wrong number the same ranking authority as trustworthy
   sorts, with no in-band signal it's epistemically weaker — worst
   case, a wrong row (see finding 4) floats to the top with full
   visual authority, precisely the "tool that lies" failure the
   product exists to prevent.
3. **[SERIOUS] rejected: option not adopted, so the underlying
   `per_dir`/`ta`-`td` consistency mechanism was never built.** Between
   an in-app delete and the next `r`-refresh, the `freeable` column
   would silently diverge from the post-delete `disk`/`td` columns —
   two columns showing two different tree states side by side on an
   otherwise frozen table.
4. **[SERIOUS] rejected: contributed to, but not solely decisive for,
   B's rejection in D1 — the option itself nominated this as the
   finding to press hardest.** Directory-name recycling (a Postgres
   `DROP`+`CREATE DATABASE` reusing an OID directory name while a
   deleted relation file from the old database is still open) passes
   every scope guard (prefix match, dev match, live, not tombstoned)
   and hangs a large "freeable" figure on the wrong, unrelated
   directory — a live counter-example to "correct where other tools
   lie."
5. **[ANNOYING] rejected: option not adopted, so the proposed dump
   `fb`/`fbn` keys were never added.** The keys would violate dump-v1's
   header-capability rule (no way to distinguish "swept, found zero"
   from "didn't sweep"), and being diff-ignored defeats their own
   monitoring justification (no trend surfaced without manual
   `zstdcat | jq`).
6. **[ANNOYING] rejected: option not adopted; the underlying
   inaccurate claim ("scan-time publish path untouched") was never
   tested against real code changes.** Adding a `freeable` field to
   `Row`/`DirTotals` does touch the shared `build_snapshot` call site
   used by both the scan-time owner and post-scan navigation,
   contradicting the doc's isolation claim.
7. **[ANNOYING] rejected: option not adopted, so the conditional
   column-presence design was never built.** A column that appears and
   disappears per directory based on a nonzero-value rule shifts table
   geometry (name column, bar alignment) as the user navigates — the
   first data-dependent column presence in a UI that otherwise keeps
   layout stable across frames.
8. **[COSMETIC] rejected: option not adopted.** The selection card's
   "PID X holds N GiB" phrasing asserts a present fact from what is
   actually a point-in-time sweep result that can already be stale by
   the time it's read; shared with every option, but B's column makes
   an under-counted (server, low-coverage) figure look more
   authoritative than a gauge suffix would.

#### C — `freeable-attack-c.md` (verdict: does not survive as pitched, becomes a substantially larger and riskier feature if fixed; Option C rejected outright in D1)

1. **FATAL-1 rejected: decisive in D1's rejection of Option C.** The
   pitch's "almost nothing new" insertion mechanism ("append a run to
   a completed directory") relies on arena mutators (`push_node`,
   `add_dir`, `push_run`) that are all `pub(crate)` — unreachable from
   the UI crate where the pitch says ghosts get inserted. Ghosts would
   require new **public** core mutation API that doesn't exist,
   contradicting the pitch's own low-cost framing.
2. **FATAL-2 rejected: decisive in D1's rejection of Option C** (D1
   cites this explicitly: "markable ghosts whose name-reuse collision
   can unlink an unrelated live file"). Nothing in the current mark
   path refuses marking a ghost row; combined with last-known-path
   name reuse (a real, unrelated file later created at the same path
   the ghost occupies), confirming a delete on the ghost's row deletes
   the real, live file — silent data loss, not merely a wrong number.
3. **SERIOUS-3 rejected: option not adopted, so the required
   special-casing was never built.** "Ghosts contribute zero to every
   aggregate" is achievable in principle but breaks the `%` column
   (percentages over 100%), identity-color ranking (a large ghost
   takes a color rank and displaces a real sibling), and the wheel
   (ghosts would eat a slice) unless each of four independent render
   paths is separately made ghost-aware.
4. **SERIOUS-4 rejected: option not adopted.** Deleting a ghost's
   containing directory would tombstone the ghost along with it via
   the normal subtree-removal walk, silently dropping its contribution
   to the freeable total even though the underlying process-held file
   was never actually freed — freeable would need to be tracked
   independent of arena tombstone state, undercutting "it's just a
   row in the tree."
5. **SERIOUS-5 rejected: option not adopted.** Every current and
   future arena consumer — the dump writer foremost, since a ghost
   entry in a `.cmbt` file corrupts the self-diff-is-zero and
   re-import invariants — needs a permanent skip-ghost clause that
   cannot be centralized the way tombstone-filtering is, because the
   freeable UI itself needs the unfiltered view.
6. **ANNOYING-6 rejected: option not adopted.** Each `r`-refresh
   tombstones and re-inserts ghosts with fresh interned names, so a
   churny host repeatedly pressing refresh drives the interner toward
   its 2^26-name cap and leaves dead ghost runs accumulating in each
   ancestor's run list — a user-repeatable, previously nonexistent
   unbounded-growth path.
7. **ANNOYING-7 rejected: option not adopted.** The pitch sells
   discoverability as "free" because there's no gauge suffix to
   notice, but its own fallback for unattributable residue still
   requires the gauge-line freeable figure — so C needs the gauge work
   the rejected annotation option needed, plus rows on top of it.
8. **COSMETIC-8 rejected: option not adopted.** Baking a
   `" (deleted — comm, PID)"` suffix into the row name would corrupt
   sort-by-name and the path-reconstruction used elsewhere; and
   because sort defaults to size-descending, a large ghost could put a
   dimmed "deleted" row under the default cursor on directory entry.

Originals recoverable from git history.
