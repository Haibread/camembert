# Option A — sweep ledger (filesystem-level truth, zero tree coupling)

> Proposal by a design agent instructed to push the "freeable is a
> property of the filesystem, not of any directory" angle as hard as it
> honestly can. Not a decision. Facts referenced from
> [freeable-research.md](freeable-research.md) (§n).

## 1. Pitch

Deleted-but-open files have, by definition, no path in the tree camembert
built — the honest place for their bytes is the **filesystem line**, not
a directory column. Option A models the phase-1 sweep as a standalone
scan artifact: one core module, one report struct, no arena changes, no
snapshot changes, no dump changes. The "where did this live" question is
answered with **evidence, not aggregates**: the panel shows each deleted
file's kernel-reported last path and the guilty PIDs verbatim, so the
user gets the serverfault workflow (`lsof +L1` → PID → restart/truncate,
research §8) pre-chewed — without camembert ever writing a per-directory
number it cannot guarantee.

The UI home is the **disk gauge**: it is the one line that already talks
about the filesystem (`statvfs` capacity, "this scan covers N% of
occupied"), and deleted-but-open bytes are precisely (part of) the
df-vs-du residual that line exposes. The freeable figure *explains* the
gauge's gap; putting it anywhere else separates the number from the
question it answers.

## 2. What phase 1 claims (and what it does not)

Counted — the honest core (research §1, §2, §5):

- fd-held (`/proc/[pid]/fd/*`) regular files with **`st_nlink == 0`**
  (ground truth; the `" (deleted)"` readlink suffix is never trusted —
  it fires on surviving hardlinks and on legitimately-named files);
- `O_TMPFILE`-style anonymous disk files (same filter catches them);
- deduped by **`(st_dev, st_ino)`** (research §5: the same deleted
  leveldb file held by 5 PIDs must count once);
- sized by **`st_blocks × 512`** (sparse-safe; `st_size` would claim
  1 GiB freeable for a 4 KiB-allocated sparse file);
- restricted to **scanned devices**: `st_dev` must be in the set of
  devices the scan actually covered (root device, plus crossed devices
  under `--cross-filesystems`). The device filter, not the path string,
  decides scope (research §3).

Excluded, stated in docs and `--help`:

- `memfd:`/`/dev/shm`/anon-inode entries — RAM, not disk (research §2:
  1021 of 1103 deleted-marked fds on the reference desktop). The panel
  shows their deduped total as **one separate line labeled "RAM-backed,
  not disk"** so the user who saw them in `lsof` output isn't left
  wondering; they never enter the freeable disk total.
- mmap-only holders with no fd: invisible without `CAP_SYS_ADMIN` on
  `map_files` (research §2). Phase 1 does not attempt the `maps`
  length-proxy fallback — a page-rounded partial-range guess is an
  invented number. Named as a coverage gap in the docs; revisit if
  field reports show it matters.
- loop-device backing files and unlinked directories: scoped out
  (research open question 6 — unverified, low value).

## 3. Data model

New module `camembert-core/src/freeable.rs`. Nothing touches
`tree.rs`, `view.rs`, or the dump writer.

```rust
/// One /proc sweep, frozen at `at`. Ephemeral: never dumped.
pub struct FreeableSweep {
    /// Deduped by (dev, ino), sorted by disk bytes descending.
    pub files: Vec<DeletedOpenFile>,
    /// Σ disk over `files` — the number on the gauge.
    pub disk_total: u64,
    /// memfd/shm/anon deduped total. Separate; never in disk_total.
    pub ram_backed: u64,
    pub coverage: Coverage,
    pub at: std::time::SystemTime,
}

pub struct DeletedOpenFile {
    pub dev: u64,
    pub ino: u64,
    pub disk: u64,           // st_blocks * 512
    pub apparent: u64,       // st_size, shown secondary
    /// Last-known path per readlink: raw bytes (procfs paths are not
    /// guaranteed UTF-8, research §3), " (deleted)" suffix stripped
    /// for display. Display-only — never used for scope decisions.
    pub path: Vec<u8>,
    pub holders: Vec<Holder>,   // pid + comm (/proc/pid/comm)
}

pub struct Coverage {
    pub procs_scanned: u32,
    pub procs_denied: u32,   // EACCES on fd/ (ptrace gate, research §4)
    pub fds_seen: u64,
    /// /proc missing or not procfs: the feature is unavailable, the
    /// scan is unaffected.
    pub unavailable: Option<UnavailableReason>,
}
```

The sweep function takes the scanned-device set (a small addition to
`ScanOutcome`; `DirMeta.dev` already carries per-directory devices) and
returns `FreeableSweep`. ENOENT mid-walk = process/fd gone, skip
(research §6: benign TOCTOU). All `/proc` reads are `fstatat(fd_dirfd,
name, 0)` — follow the magic symlink — plus one `readlink` for the path
text.

A second entry point, `open_file_index(...)`, runs the **same walk
unfiltered** and returns a `HashMap<(dev, ino), Vec<Holder>>` — this is
the deletion open-file warning's input (research §7: same data, no
`st_nlink` filter). One module, two filters, zero duplicated code.

## 4. Sweep timing

- **Once, at scan completion.** The UI spawns a short-lived thread when
  the phase flips to `Done` and receives the report over the existing
  event-loop channel — no UI stall (measured 37 ms single-threaded on
  505 procs, research §6, but a 10k-process server scales linearly;
  a thread costs nothing and keeps the worst case off the render loop).
  `--no-ui` mode runs it inline after totals and prints one summary
  line.
- **Not during the scan.** Not for cost (37 ms is nothing) but for
  semantics: a freeable figure aging alongside a half-built tree
  answers no question the user can act on yet, and the completion
  moment is when the gauge's covers-N% line becomes meaningful.
- **Refresh on demand**: `r` inside the panel re-runs it (the user just
  restarted a daemon and wants to see the bytes come back).
- **Fresh at deletion time**: opening the delete-confirm modal
  triggers `open_file_index` on a thread; the warning line ("2 marked
  entries are open in PID 4312 (postgres)") fills in when it lands.
  Confirmation is **not blocked** on it — the warning is advisory, and
  blocking the modal on a slow /proc walk would teach users to hate it.

## 5. UI

- **Disk gauge suffix** (post-scan): `· 1.2 GiB freeable (deleted,
  still open)` — clickable, and `f` from anywhere. Hidden when the
  sweep found nothing or /proc is unavailable (a permanent "0 B
  freeable" is noise).
- **`f` panel** — a modal in the existing precedence chain (confirm >
  review > freeable > cheatsheet), listing evidence rows: disk size,
  path (lossy-decoded for display, escape-hatched like tree names),
  holder PIDs + comm. Rows grouped by their deepest *still-existing*
  ancestor directory — a **display-only textual grouping** of the
  path evidence, explicitly not a per-directory byte claim: the group
  header shows the ancestor path, not a number-bearing tree row, and
  the raw per-file paths stay visible under it.
- **Coverage footer**, only inside the panel (the user opted into
  detail — no nagging on the main screen): `365 of 505 processes
  unreadable — run as root for the full picture` (research §4: an
  unprivileged desktop user sees ~28% of processes but their own big
  consumers; a server sysadmin sees almost nothing that matters).
  When `/proc` is absent: `f` shows a toast "freeable: /proc not
  available here"; the gauge shows nothing.
- **Metric cards, table, wheel, bars: untouched.** The tui-design
  "freeable segment" reservation (each bar's actually-freeable
  fraction) stays reserved for phase 2, where per-entry data (btrfs
  shared extents, hardlink siblings) actually exists. Phase-1 bytes
  are *not part of any bar's total* — the files are not in the tree —
  so painting them into bars would misrepresent them structurally,
  not just numerically.

## 6. Dump and diff

**Nothing.** A dump is a filesystem snapshot; open-fd state is process
state, stale the moment it is written, meaningless on the machine that
reads the dump, and poison for diff (a "freeable" delta between two
dumps would compare two process populations, not two filesystems). No
new record type, no new header capability, no `e`-line keys. The format
spec's minor-version budget is spent on filesystem facts only.

## 7. CLI surface

One flag: `--no-proc-sweep` (env `NO_PROC_SWEEP`, `--no-motion`
precedent for naming and env handling). Reading other processes'
`/proc/[pid]/fd` is legitimate but observable (ptrace-mode checks can
be logged by hardened kernels/LSMs); paranoid or audited environments
get a clean off switch, and containers with a masked `/proc` get
silence instead of a wall of EACCES traces in the log file. Documented
in `--help` and README in the same change, including the fd-held-only
scope and the root/coverage story. No other surface: no sweep-tuning
knobs, no output-format options, nothing to deprecate later.

## 8. Phase-2 growth

Phase 2 (btrfs `FIEMAP_EXTENT_SHARED`, hardlink siblings out of
selection; ZFS: show nothing) is **per-entry data about files that are
in the tree** — the structural opposite of phase 1's not-in-the-tree
bytes. Option A keeps the two honest layers separate:

- per-entry freeable lands as a side aggregate next to the arena (the
  `excluded`-reason side-map pattern in `tree.rs`) plus a `Row` field
  in snapshots, feeding the reserved bright bar segment — built when
  there is per-entry truth to carry, shaped by phase 2's actual access
  pattern (FIEMAP is per-file and expensive; it will want lazy/partial
  evaluation that a phase-1 eager map would prejudge);
- the ledger stays what it is: the filesystem-level "deleted but still
  open" line plus its evidence panel. In phase 2 the gauge line can
  sum both layers; the panel remains the deleted-file drill-down.

Nothing built in phase 1 has to be unbuilt.

## 9. Honest weaknesses

1. **No per-directory number while browsing.** A user staring at
   `/var/log` gets no hint in the table that 1.8 GiB of deleted logs
   hover nearby; they must notice the gauge or press `f`. The panel
   grouping mitigates but does not equal an in-tree column.
2. **Discoverability hangs on one gauge suffix.** If the user never
   reads the gauge line, phase 1's headline feature is invisible. A
   one-time toast at scan end ("1.2 GiB freeable — press f") is the
   obvious patch and the obvious nag risk; left as a session decision.
3. **The panel is a flat list.** Hundreds of deleted files (busy
   container hosts) make it a scroll fest; grouping helps, but there
   is no sort/filter language in it (that arrives with the wave-3
   query language, which the panel should adopt).
4. **Freeable appears only at scan end.** On a 30-minute HDD scan the
   user waits that long for a number that was computable in 37 ms.
   Accepted for phase 1 (see §4); a "run sweep early, refresh at
   completion" variant is trivial if wanted.
5. **Evidence rows can still mislead** even labeled: a path renamed
   before deletion shows its final name (research §1 — truthful but
   unfamiliar), and non-UTF-8 or PATH_MAX-truncated paths display
   escaped. The panel shows what the kernel said; the docs must say
   exactly that.
