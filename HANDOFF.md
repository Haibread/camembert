# camembert — project handoff

State of the project as of 2026-07-24, written for the next agent (or
human) picking it up. The original ideation document is archived at
[docs/design/handoff-original.md](docs/design/handoff-original.md); this
file describes what actually exists.

## What camembert is

A disk usage analyzer (ncdu successor) in Rust whose thesis is
**differentiation through honest answers to real questions**: what grew
(diff), what is actually freeable, what is big *and* stale (the filter
query language, not a score — see next-steps item 8) — with numbers
that are correct where other tools lie (hardlinks, sparse files,
unreadable dirs, kernfs). See [README.md](README.md) for the product
pitch.

## Ground rules (binding)

- [CLAUDE.md](CLAUDE.md): delegate to agents with a model adapted to the
  task; every CLI addition documented in `--help` AND the README, in the
  same change.
- Decision documents in `docs/design/*-decisions.md` are **settled**;
  reopening one needs a new element, not re-litigation:
  - [dump-format-decisions.md](docs/design/dump-format-decisions.md)
    (D1–D6: JSONL+zstd-seekable interchange; SQLite deferred to a wave-4
    cache; canonical hardlink owner = smallest raw-byte path; ino/dev as
    JSON strings, u64 ≥ 2^53 as strings; degrade-don't-fail on low disk;
    `.cmbt`).
  - [scan-tree-decisions.md](docs/design/scan-tree-decisions.md) (D1–D5:
    single-owner-thread arena, run-list children, hardlink correction off
    the critical path, ~450 MB @ 10 M target, 33 ms UI cadence).
  - [tui-design.md](docs/design/tui-design.md) (dashboard-cockpit look,
    capability ladders, identity colors, design reservations for the
    diff skin / freeable segment / sunburst / kitty-graphics opt-in, and
    the remaining implementation slices).
  - [freeable-decisions.md](docs/design/freeable-decisions.md) (D1–D8:
    sweep-ledger shape, root-fs scoping, nlink==0 ground truth, scan-end
    + pre-deletion lifecycle, gauge/panel/toast UI, advisory warning,
    `--no-proc-sweep`, no dump keys, module isolation).
  - [freeable2-decisions.md](docs/design/freeable2-decisions.md) (D1–D6:
    oracle-first Option B, allocated-logical units + "exclusive" wording,
    floor lifecycle + kernel ≥ 6.1 gate + `--no-fiemap`, mark-time oracle
    with async confirm modal, filesystem tiers, fiemap.rs isolation).
- The dump format spec is [docs/format/dump-v1.md](docs/format/dump-v1.md);
  writer AND reader implement it. Major-version changes are near-taboo
  (they invalidate every stored dump).
- The reasoning trail (research digest, options pushed to their limit,
  adversarial attack findings) lives condensed inside each decisions doc's
  own "Condensed reasoning trail" section — read it before proposing to
  revisit; full originals are recoverable from git history.
- Workflow: co-design structural decisions with the user; implement
  autonomously once settled; direct commits on `main`, small and atomic;
  agents work in worktrees, the orchestrator reviews and merges.
- Never put the user's real name or personal email anywhere; the repo
  identity is `Haibread <haibread@users.noreply.github.com>` (set
  repo-locally).

## What is implemented (all merged on main, ~582 tests green)

- **Scan engine** (`camembert-core/src/scan/`): work-stealing,
  fd-relative `openat`/`getdents64`/`statx` (fstatat fallback), mount
  boundaries by `st_dev` — **crossed by default since 2026-07-24**
  (user decision: `--one-filesystem`/`ONE_FILESYSTEM` is the opt-out,
  `--cross-filesystems` removed; the disk gauge captions multi-fs scans
  as "spans N filesystems" via `ScanOutcome::device_count()`; known
  accepted caveat: btrfs snapshot subvolumes are descended and can
  multiply-count — documented in README/--help), **kernfs excluded by
  filesystem magic regardless**, single-owner arena integration with
  bounded out-of-order holding, per-directory batched aggregation,
  completion cascade, first-seen hardlink registry + post-scan canonical
  re-attribution. **Media-adaptive auto threading** (sysfs rotational +
  mountinfo fallback for btrfs: SSD → min(cores, 16), HDD → 2, unknown
  → min(2×cores, 8); measured 95 → 76 ms on the bench tree) — since
  2026-07-25 a `rotational=1` is only believed when the device's active
  I/O scheduler is not `none`, because cloud block volumes (Scaleway SBS,
  virtio-SCSI) claim to spin while being network flash; the contradiction
  resolves to `unknown`. **io_uring-batched statx** (per-worker rings,
  runtime probe, sync fallback) is still available via
  `--statx-engine io_uring`, but `auto` no longer engages it: it wins
  12-21 % at ≤ 2 workers on the dev box and loses 1.2-1.7× at every
  worker count on cloud block storage.
- **Tree** (`tree.rs`): 32-byte nodes, run-list children (D2), subtree
  aggregates, tombstoned removal with negative-delta propagation,
  excluded-reason side map, **error-reason side map** (`errno` of every
  unreadable dir / failed stat, preserved end to end — issue #8; taxonomy +
  severity in `errno.rs`, dropped on removal to stay consistent with `te`).
- **TUI** (`camembert/src/ui/`): browse-during-scan (arc-swap view
  snapshots, latest-wins nav cell, 33 ms cadence), dashboard cockpit
  (metric cards, statvfs disk gauge, table + donut wheel with identity
  colors, selection card), capability ladders (truecolor→mono,
  sextants→ASCII, NO_COLOR/--color), guarded mark-then-confirm deletion,
  log output never touches the terminal (--log-file). **All six design
  slices of [tui-design.md](docs/design/tui-design.md) are implemented**:
  mouse everywhere via per-frame `FrameGeometry` hit-testing (clickable
  rows/slices/breadcrumb/errors-card, hover card), deletion basket strip
  + `v` review modal + toasts (`toast.rs`) + `?` cheatsheet generated
  from the `keymap.rs` dispatch table, 150 ms eased animations
  (`anim.rs`, `--no-motion`/NO_MOTION) with idle-quiescent polling,
  responsive mini-donut collapse below 100 columns + `z` zen mode,
  themes tokyo-night/light/high-contrast (`--theme`/THEME), XDG
  `camembert.toml` (CLI > env > file > default), OSC 11 background
  detection in a bounded raw-mode termios window (rustix, no thread).
- **Dump v1** (`dump.rs` + `dump/read.rs`): ordered writer (`-o`,
  `.part`+rename, seekable zstd, `zstdcat|jq`-compatible — verified) and
  streaming reader (torn-frame tolerant, number-or-string u64s). **Minor 1**
  adds the optional `er` error-reason field (portable errno name; issue #8),
  round-trips into the tree side-table. TUI: selection-card errno note +
  severity-ordered per-errno breakdown under the errors card.
- **Diff** (`diff.rs`, `camembert diff`): streaming merge-join, bounded
  memory, Added/Removed/Grown/Shrunk/Touched/TypeChanged, `--json`,
  `--threshold` (exit 1 = growth exceeded; 2 = error).
- **ncdu import** (`ncdu.rs`, `camembert import`): hand-rolled streaming
  JSON lexer (handles non-UTF-8 pre-2.5 exports), rebuilds the arena,
  canonical hardlinks, emits ordered dumps. Import→self-diff = zero.
- **Freeable phase 1** (`camembert-core/src/freeable.rs`,
  `camembert/src/ui/freeable_panel.rs`): post-scan `/proc` sweep ledger
  per [freeable-decisions.md](docs/design/freeable-decisions.md) D1–D8
  — deleted-but-open files (`st_nlink == 0` ground truth, `(dev,ino)`
  dedup, `st_blocks` sizing, memfd/shm classified out by path prefix),
  root-filesystem-scoped gauge suffix, `f` evidence panel (guilty
  PIDs/comm, display-only ancestor grouping, coverage + RAM-backed +
  cross-device honesty lines), thresholded scan-end toast (≥ 100 MiB
  and ≥ 1 % capacity), advisory open-file warning in the delete confirm
  (marked files by `(dev,ino)` + files *inside* marked dirs by path
  containment, coverage-honest), `--no-proc-sweep`/`NO_PROC_SWEEP`.
  Nothing in tree/dump/diff (D8 isolation).
- **Freeable phase 2 slice 1** (`camembert-core/src/fiemap.rs`,
  `camembert/src/ui/oracle.rs`): the mark-time selection oracle per
  [freeable2-decisions.md](docs/design/freeable2-decisions.md) D2/D4/D5
  — FIEMAP wrapper (pagination loop, never `FLAG_SYNC`, delalloc →
  unknown), per-device physical-interval sweep bucketing bytes into
  exclusive / shared-within-selection (ceiling) / shared-outside /
  unknown, D4 hardlink rule fed by the now-public hardlink registry
  (`ScanOutcome::hardlink_groups`), filesystem tiers by statfs magic
  (btrfs/XFS extent, ext-family hardlink-exact, ZFS no figures), kernel
  ≥ 6.1 gate + compress-mount caveat (mountinfo super-options now
  parsed). UI: jobs spawn per mark off-thread (50k-file cap, per-inode
  map cache, serial-guarded against unmark/remark races, invalidated on
  deletion epochs), confirm modal gains an async oracle slot that
  updates in place (spinner while pending, `y` always live), quantified
  D2 wording replaces the phase-1 qualitative hardlink note when ready.
  `--no-fiemap`/`NO_FIEMAP` disables it outright. Real reflink
  integration tests (FICLONE fixtures under `CARGO_TARGET_TMPDIR`,
  guard-skipped off btrfs/XFS).
- **Freeable phase 2 slice 2** (`camembert-core/src/fiemap/floor.rs`,
  `camembert/src/ui/floor_rt.rs`): the ambient exclusive floor per
  freeable2 D1/D3 — whole-tree off-thread pass sequenced after the
  phase-1 sweep (or at scan end under `--no-proc-sweep`), gated on
  kernel ≥ 6.1 + `--no-fiemap`; SHARED-unset extents on single-link
  files + wholly-seen fully-live hardlink groups landed at their LCA
  (additive by construction, understates only); whole-value snapshot
  (never mutated), post-deletion `reaggregate_floor` (no FIEMAP, per
  attack-b finding 9), deletion interlock (cancel-before-write-lock,
  unconditional respawn, generation-guarded results). UI: the reserved
  in-bar bright segment (emphasized same-hue identity color; bold on
  ANSI-16/mono, nothing wrong ever), selection-card `excl ≥ X · mapped
  Xm ago` / `fully shared` lines with one caveat (compress wins over
  unmapped-count), dim `mapping extents… N files` footer progress.
  Also on main: the disk-gauge coverage fix on compressed mounts
  (`Coverage::Exceeds` wording instead of a fabricated 100%), and Esc
  now ascends from tree view instead of quitting (user request,
  recorded in query-decisions D6).
- **Freeable confidence verdict** (`camembert-core/src/confidence.rs`):
  the answer to the product review's "the UI is drowning in uncertainty
  rather than communicating it". One `Verdict` — three graded rungs
  (`measured`/`partial`/`fragmentary`) plus an ungraded `no figure` —
  derived only from signals the sweep and the oracle already compute,
  with the rule, its two half-boundaries and every deliberate
  non-signal documented on the type. Rendered as a headline **above**
  the existing caveat lines (which are unchanged) at the top of the `f`
  panel and the top of the delete-confirm modal; a pending oracle
  grades as an absence and flips in place when the report lands. Level
  is carried in plain text (mono-readable), color only reinforces via
  theme slots. No new CLI surface.
- **Flat view + pattern breakdown** (`camembert-core/src/flat.rs`,
  `camembert/src/ui/flatview.rs`): per
  [flat-view-decisions.md](docs/design/flat-view-decisions.md) D1–D6 —
  `t` (top files, cap `flat_cap` default 1000) / `b` (category
  breakdown) as in-place modes, contextual Esc, disjoint
  first-match/outermost-wins groups (presets + `[patterns]` in
  camembert.toml, per-key-resilient config parse), dual engine: live
  provisional accumulation on the scan owner (~66 ns/node, memoized
  interned-name globs, denormalized basenames) + exact frozen-arena
  fold post-scan, recomputed per deletion epoch at render; donut shows
  mode data; `--no-ui` summary prints top files (`--top`).
- **Bench harness** (`scripts/bench-compare.sh`, CLAUDE.md
  "Benchmarks"): hyperfine comparison vs du/dust/dua/pdu/diskus
  (+ ncdu/gdu when installed) on a deterministic 200k-file synthetic
  tree, warm or `--cold`; mandatory before/after any scan-hot-path
  change. Its first run caught and fixed a 1 s progress-poller stall
  in `--no-ui` (camembert now ~74 ms on the bench tree).
- **Filter query language + palette** (`camembert-core/src/query.rs`,
  `camembert/src/ui/palette.rs`): per
  [query-decisions.md](docs/design/query-decisions.md) D1-D7 — qualifier
  tokens (bare smartcase substrings, globs, `dir/` ancestors, `>100M`,
  `older:/newer:`, `kind:`, `ext:`, `is:`, `!` negation, quoting;
  `( ) | ;` reserved with feature-naming errors), inert broken terms
  with structured spans, Ctrl-K palette (query-first, `>` commands
  generated from the keymap) + `/` pre-scoped shortcut, text-input mode
  suspending global keys, post-scan-only debounced off-thread fold
  (5-pass, std scoped threads, bit-identical at any thread count;
  1.9 ms @ 1M/8 threads), hardlink membership by any path via a lazy
  reverse map (bytes counted once at the canonical), filtered dir
  totals + residual pill, composition with t/b/donut, dir marks
  refused under filter, history in XDG state + read-only `[queries]`,
  `--filter`/FILTER (strict in --no-ui, exit 2; dumps never filtered).
- **Releases**: tag-triggered workflow builds static musl binaries
  (x86_64 + aarch64, native runners) with sha256 sums attached to the
  GitHub Release; `--version` embeds the build commit (build.rs,
  `-dirty` aware). Release notes come from
  [CHANGELOG.md](CHANGELOG.md) — the workflow extracts the tag's
  section and passes it as `--notes-file`, failing the job if there is
  none, then appends GitHub's generated notes (label categories in
  `.github/release.yaml` only ever catch dependabot PRs, since work
  lands directly on `main`). **v0.2.0 is released** (2026-07-24: reclaim
  oracle, ambient exclusive floor, confidence verdict, errno plumbing,
  deletion TOCTOU fix, and the breaking
  `--cross-filesystems` → `--one-filesystem` swap), and **end-to-end
  verified from the published artifacts**: both sha256 sums check out,
  both binaries are statically linked, the x86_64 one runs a real scan
  and reports `camembert 0.2.0 (3d07b34)` — the tag's commit. One
  discrepancy found while verifying: **the aarch64 binary is static but
  not PIE** (the x86_64 one is `static-pie`), so it runs without ASLR;
  the earlier "static-pie" claim only ever held for x86_64. Not a
  regression — same workflow as v0.1.0, whose verification only covered
  x86_64. **Investigated 2026-07-24, decided not to fix**: rustc's
  `aarch64-unknown-linux-musl` spec does not set
  `static_position_independent_executables` (x86_64's does), and forcing
  PIE through the `musl-gcc` wrapper the release job installs is the
  documented segfault-at-startup path
  ([rust-lang/rust#95926](https://github.com/rust-lang/rust/issues/95926)).
  A working binary without ASLR beats a hardened one that does not run;
  the difference is stated in the README's Install section instead.
  Reopen if upstream enables static-pie for the target, or if the job
  moves off `musl-gcc` to self-contained `rust-lld` linking.
- **Infra**: pre-commit (fmt, clippy -D warnings, actionlint, hygiene),
  GitHub workflows `quality` + `release` (SHA-pinned), Dependabot,
  dual MIT/Apache-2.0, repository metadata. The GitHub repo is live at
  [github.com/Haibread/camembert](https://github.com/Haibread/camembert)
  (public, `quality` CI green on main).

## Known limitations (documented in code where they live)

- **Cross-filesystem validation (2026-07-25, Scaleway DEV1-S, kernel
  6.8, ext4 / XFS / btrfs / btrfs+zstd / f2fs / exfat / tmpfs / ZFS on
  one host).** Totals are byte-exact against an independent `lstat` walk
  on all eight (both `apparent` and `real`); `du -sb` disagrees only
  because it does not count directory inodes in apparent mode. Two
  filesystem properties bite: **ZFS accounts `st_blocks` on
  transaction-group commit**, so freshly written data reads as ~0 real
  bytes for seconds (README "Honest numbers"; three tests now probe for
  it and skip their absolute-size assertions), and **exfat has no
  symlinks**, so running the suite with `TMPDIR` on exfat fails 17 tests
  that build fake sysfs trees out of symlinks — not worth guarding, but
  worth knowing. exfat is also the one filesystem where camembert loses
  to `du` (cold, 100k entries: 78 s vs 58 s; parallel readers fight the
  FAT chain) — accepted, it is a transfer format, not a scan target.

- io_uring statx is opt-in only (`--statx-engine io_uring`): the auto
  heuristic that engaged it at ≤ 2 workers was retired on 2026-07-25
  after a cross-filesystem run on a 2-vCPU Scaleway instance measured it
  1.2-1.7× *slower* than sync at every worker count from 1 to 8, warm and
  cold, on ext4/XFS/btrfs/f2fs — the opposite of the dev box's 12-21 %
  win at the same counts. Nobody has yet measured either engine on a
  **real spinning disk**, which is the one medium the old heuristic
  claimed to serve; that measurement would be the new element needed to
  bring an auto rule back. Worker fd usage can approach RLIMIT_NOFILE on
  pathologically wide trees; a worker panic hangs the scan (owner panics
  are handled). The media-adaptive thread policy resolves anon-bdev
  filesystems (major 0 — btrfs, notably) via a `/proc/self/mountinfo`
  fallback to the covering mount's real backing device, but a
  **multi-device btrfs volume is classified from a single member
  device**: mountinfo reports only one, so a volume mixing an SSD and an
  HDD can be misjudged either way (enumerating
  `/sys/fs/btrfs/<uuid>/devices/` and combining every member
  conservatively, as already done for device-mapper/RAID slaves, is a
  possible refinement). Genuinely undetectable cases (network
  filesystems, unreadable sysfs/mountinfo) still fall back to the
  pre-adaptive `min(2x cores, 8)` default.
- Deletion: the executor walks descriptor-relative (`openat`/`fstatat`/
  `unlinkat` from the scan-root fd, `O_NOFOLLOW` below the root) and
  re-checks each target's `(dev, ino)` against the identity the UI
  recorded at confirm time — the intermediate-symlink TOCTOU and a
  real-directory swap of the top-level target are both closed. Residual
  (documented in `delete.rs`): the root's path above the anchor is
  trusted (as the scan trusts it); a rename strictly within the marked
  subtree mid-walk stays bounded to that subtree (per-entry `unlinkat`,
  no symlink ever followed); the identity anchor is captured at confirm
  time, not scan time. Freed-space estimate for surviving hardlinks is
  still optimistic (warned in dialog).
- Hardlinks: if a concurrent rewrite changes an inode between the
  scan's two `statx` snapshots of it, canonical re-attribution shifts
  the root total by the size delta (the canonical link's size wins for
  the group); per-directory subtree-aggregate consistency is preserved
  and the divergence is logged at debug.
- FIEMAP pagination has a forward-progress guard: a filesystem
  returning non-advancing extent batches leaves the unmapped tail
  uncounted (`exclusive` understates) instead of looping forever on the
  open fd — pathological/hostile filesystems only.
- Dump: ordered-only writer (D5 unordered/degrade tier unimplemented);
  `ext:false` (no uid/gid/mode yet); TUI writes the dump on the UI
  thread at scan end (brief stall).
- Diff memory is bounded by the largest directory block, not strictly
  constant; hardlink-extra entries show full size in the entry list
  (dir totals are correct).
- `camembert ./diff` needed to scan a directory literally named `diff`
  (clap subcommand precedence).
- Scanning-a-kernfs-root is allowed (explicit user intent); only mounts
  *inside* a scan are excluded.
- Freeable phase 2 slice 1: buckets 3/4 are merged — "shared outside"
  cannot distinguish a scanned-but-unselected sharer from an invisible
  snapshot without root (`LOGICAL_INO` is EPERM; attack-b finding 4),
  so the shared-elsewhere figure is exact as "not freed now" but names
  no culprit. A mark whose oracle thread fails to spawn (rare) reads
  Ready with that mark's bytes silently absent. The 50k-files-per-mark
  cap sends the overflow to "not estimated" with a caveat line. Extent
  maps are cached per (dev,ino) within a deletion epoch — external
  filesystem writes between mark and confirm are not watched (D3:
  acknowledged, not tracked).
- Freeable phase 2 slice 2: between a deletion and its respawned pass
  landing, every ambient floor surface is empty (the stale snapshot is
  dropped rather than shown overstating — D3); a post-deletion
  `reaggregate_floor` carries the previous unmapped count and
  compressed flag forward, and never re-FIEMAPs (surviving reflink
  siblings that became more exclusive keep their old, understating
  figure until the next full pass — i.e. the next scan). The footer
  progress count has no denominator (hardlink groups tick it once per
  group). Flat-view rows and filtered totals show no floor yet
  (slice 3 composition).
- Freeable: mmap-only holders invisible without CAP_SYS_ADMIN
  (`map_files`); btrfs multi-subvolume layouts under-count (root-subvol
  `st_dev` scoping, stated in the panel); directory-containment
  open-file warning matches by path text — mount-namespace divergence
  gives false negatives (advisory only); unprivileged sweeps see ~28 %
  of processes on a desktop (coverage line says so).
- Flat view: full paths (and Enter-jump/marking on flat rows) are
  post-scan only — the live provisional view shows basenames
  (denormalized onto `TopFile`; the scan arena is not shared with the
  UI thread); breakdown drill-down is deferred to the query language;
  group-level marking ("mark every node_modules") is a deliberate
  fast-follow with its own guard design.
- TUI: the design's "excluded mounts dim italic" styling is not
  implemented (no excluded-row rendering exists yet — the theme
  mechanism has a slot for it); the header mini-donut is decorative,
  not clickable; bar fills animate from 0 (no per-row from-value
  tracking); relative times in the selection card can go stale while
  the loop idles between events.

## Windows port — in progress, started 2026-07-25

Not on the value-ordered list below; started on the user's explicit ask.
The Win32 details, the API choices and the facts measured on real hardware
live in
[windows-backend-design.md](docs/design/windows-backend-design.md) — read
that before touching any of this. This section is only the state.

### Where it stands

The **whole workspace** compiles on `x86_64-pc-windows-msvc` — 0 errors, 0
warnings, clippy clean at `-D warnings` — and the binary scans, browses and
dumps for real. `cargo test --workspace` there: **392 passing, 0 failing**
(2 ignored, both Unix-only guards).
Linux is untouched throughout: 596 tests green at every commit, and the
warm 200k-file bench moved 70.8 -> 68.3 ms across the seam commit, which
is noise.

Merged, in order: `358e05b` binary's Unix couplings; `b47d898` the
platform seam under `scan/linux/`; `c2a9ccf` `cfg`-gated modules and deps;
`dccada7` `ScanErrno` (the actual blocker — see below); `ae8dd04` the
`OsStr` bridge; `9e837fd` the design doc; `afdc727` the windows-2025 CI
job; `c656ea2` the Windows scan backend; `5dc5928` the reduced TUI;
`e38c3f3` the Windows bench harness; `f433a85` the integration tests
running there; `271390c` the nlink dossier; then the four commits that
close the performance hole below.

### The performance hole — closed 2026-07-27, landing 1 of 2

`scripts/bench-compare.ps1`'s first run said the Windows backend was 14×
behind gdu, and 95 % of the scan was one line: `worker.rs::query_nlink`,
`NtQueryInformationByName(FileStatInformation)` once per non-directory
entry purely to obtain a link count. The dossier
([windows-nlink-dossier.md](docs/design/windows-nlink-dossier.md), read
its orchestrator's review note first — it corrects three things the body
gets wrong) measured why and what it bought. Short version: the call is an
**open** (97.6 % of its 46 µs is the create path through fourteen
minifilters), and it **changed no total on any tree**. What deduplicates
hardlinks is the owner's inode registry; `nlink > 1` was only ever a gate
on entering it, and on Windows that gate admits 92 % of `C:\Windows\
System32` anyway.

**Shipped: the registry keys on the listing's own file id, and a repeat
sighting deduplicates.** Zero per-entry calls. Warm, 200k-file synthetic
tree, Ryzen 9 5950X / NVMe / NTFS, Defender real-time protection **on and
unmodified** throughout:

| tool | before | after |
|---|---|---|
| **camembert** | **2044 ms** | **107.0 ms** |
| gdu | 138 ms | 138 ms |
| robocopy `/L /S` | 582 ms | 573 ms |
| diskus | 3167 ms | 3067 ms |
| dust | 3890 ms | 3666 ms |

19.1× faster than it was, 1.29× faster than gdu. `C:\Windows`: 5845 →
2643 ms. Peak RSS is the price and it is workload-shaped: 16.99 → 21.46 MB
on the 200k tree (23.4 B/entry, exactly where the registry is useless
because the tree has no hardlinks), 84.95 → 87.43 MB on `C:\Windows`.

**Totals are unchanged, verified per entry, not at the root**: dumps from
the shipped binary and the new one, run through `camembert diff`, on the
`mklink /H` fixture, `System32\drivers`, `System32` (8.4 GiB) and the 200k
tree — `diskDelta 0, apparentDelta 0, entryDelta 0` on all four. The 25
`touched` entries on `System32` are Windows' own event logs churning
between runs; the shipped binary diffed against *itself* twenty minutes
apart shows the same 25, all with zero size deltas.

What changed besides the speed, and why each is not a regression:

- **`hardlinked inodes: N` counts something narrower** — inodes reached by
  more than one path *in this scan*, not inodes with `nlink > 1`
  somewhere on the volume. 728 → 0 on `System32\drivers`, whose 728
  linked files all have their siblings in WinSxS, outside the scan. The
  line says which one it is, in those words, because otherwise the drop
  reads as a regression. Linux wording is untouched.
- **Dumps omit `l`** rather than write the registry's admission marker.
  `i` is still written for genuine groups, so `drivers` goes 14 617 →
  10 713 bytes and the 200k tree stays at ~122 KB (121 757 → 121 759,
  which is the elapsed-time field). Spec §8.1 is the platform note.
- **`--links`/`LINKS`** restores all of it — the call, the true count, `l`,
  and the "links you cannot see" reading of `⛓`. Measured at 5693 ms on
  `C:\Windows` against the shipped binary's 5845 and 1.965 s on the 200k
  tree, i.e. it *is* the old behaviour. Documented as experimental with
  the cost on the label, in per-*file* terms (~19× on a file-dense tree,
  ~2× on `C:\Windows`) since directories are never queried.
- **`camembert-core/tests/scan.rs::hardlinks_are_counted_once_not_twice`**
  is the guard that should have existed all along: 64 files + 64 hard
  links, portable, running in the `windows-2025` job.

### Landing 2 — the selection card asks, 2026-07-27

The lazy per-file link query *at the point of consumption* (dossier §4
Option D's surviving variant, §6 step 6). **The selection card is done.
The flat view's `⛓` column is not — deliberately, see below.**

For the row under the cursor, camembert now issues exactly one
`NtQueryInformationByName` and says what it means:

```
╭ ntfs.sys ────────────────────────────────────────────────────────────╮
│  3.4 MiB · 2.1% of parent                                            │
│modified 12 days ago · 1 items                                        │
│2 links · 1 outside this scan — deleting this frees nothing           │
╰──────────────────────────────────────────────────────────────────────╯
```

Two numbers, kept apart on purpose: **how many links exist** (the
filesystem's `NumberOfLinks`, fresh) and **how many this scan reached**
(the hardlink registry, keyed on the scan's own node identity — never a
file id matched across two APIs). Their difference is the answer the
`--links`-free scan gave up. A single-link file says `1 link · nothing
else points at this file` rather than falling silent, and a query that
fails says `links unknown · <reason>` — never nothing, because nothing
reads as "no links".

Where it lives, and why:

- **`camembert-core/src/winlink.rs`** (`cfg(windows)`, public) owns the
  one `unsafe` block. `scan/windows/worker.rs::query_nlink` now calls into
  it, so the scan path and the card path cannot drift. Its `LinkCount` is
  three-way — `Known` / `Unsupported` / `Failed(QueryFailure)` — which is
  the `is_unsupported_status` distinction the scan already made, promoted
  into the type so a consumer cannot collapse it by accident.
- **`camembert/src/ui/nlink_rt.rs`** is the UI runtime, copied in shape
  from `ui/oracle.rs`: off-thread job, `Pending` placeholder, update in
  place, results memoised per node *and* per inode where the registry
  knows one, all invalidated on the deletion epoch. The card never blocks
  a frame — 46 µs is nothing on NTFS, but a UNC scan root is not bounded
  by that measurement.
- **Nothing is asked when the answer is already held** (`--links`, i.e.
  `link_counts_known`), for directories, or for anything that is not a
  plain file in the tree. A WOF/OneDrive-backed file *is* queried: the
  scan skips those to avoid changing what `C:\Windows` deduplicates to
  (gap 1 below), which is a registry question, and there is no registry
  here.
- **No CLI or env surface was added.** Nothing to document in `--help` or
  the README; the card simply says more than it did.

Three rendered cases, from the tests that pin them
(`ui::tests::windows_links`, real scans, real syscalls, real
`TestBackend`): links outside the scan (above, and `Netwfw10.dat` in
`System32\drivers` reads `3 links · 2 outside this scan`), a lone file,
and a refused query (`links unknown · the entry is gone` for a name that
raced away; `links unknown · access denied` for an unreadable directory).

**Why the `⛓` column in flat view was not done, with the measurement.**
It is affordable: 50 queries against one open directory handle cost
**1.6 ms** on this box (31.8 µs each, warm, Defender on), 4.8 % of a 33 ms
frame, and memoised it is paid once per viewport rather than per frame.
Two things make it a decision rather than an extension, and both belong to
the user:

1. **One thread per row does not scale to a viewport.** The runtime spawns
   a job per candidate, which is right for one cursor row and wrong for
   fifty; the column needs a *batched* job grouped by parent directory
   (the 1.6 ms above is one shared handle — a fresh open per query is
   3.0 ms), plus the one-frame-late geometry feedback loop
   `clamp_freeable_cursor` already uses to learn what is visible.
2. **`⛓` means something different on Windows now.** Landing 1
   deliberately redefined it to "reached by more than one path in this
   scan". Filling the column from a live query would make the same glyph
   mean "has links anywhere" in flat view and "reached twice here" in tree
   view. Pick one meaning before writing the code.

Note that thread scaling was measured at 1/2/4/8/12/16/24/32 workers
before this change and 8 came out optimal, with 12+ regressing ~45 %. **Do
not trust that number now** — it measured a workload that was 95 % one
syscall, and the shape has changed completely. Re-measure.

### Landing 3 — directories get their index bytes back, 2026-07-27

Known gap 4 (below), closed. A Windows directory listing reports
`AllocationSize = EndOfFile = 0` for every **subdirectory** entry in it, so
every directory below the scan root contributed nothing to its own size —
while the root, which `windows.rs::open_root` opens by handle and asks with
`FileStandardInfo`, reported the real figure. On a `sub/` of 400 files with
38-character names: **0 B as a child, 195.1 KiB as a root**. `camembert sub`
and `camembert parent-of-sub` disagreed about `sub` by two orders of
magnitude, and counting directory inodes is exactly what the README says
separates camembert from `du -sb`.

The worker already opens every directory in order to enumerate it, so the
correction needs no extra open — only the information query on a handle it
holds. `Batch::dir_own_size` (`cfg(windows)`, beside `dir_error`) carries
it, exactly one batch per job; `Owner::correct_dir_own_size` applies it as a
**delta against what the node already holds**, which makes it idempotent and
lets one walk up the ancestor chain repair the directory, its parent's total
and every ancestor at once. The node is rewritten in the same breath, so the
invariant everything else reads — a directory's aggregate is the sum of its
own entry lines — survives. The live flat accumulator learns the same delta
(`Accumulator::add_dir_bytes`) or it would drift from the frozen-arena fold,
which is the D2 agreement invariant `tests/flat_agreement.rs` pins.

**The `.` entry is not a shortcut** — checked, because it would have been
free: a directory's *own* listing reports `eof = alloc = 0` for `.` and `..`
exactly as its parent's does for it. The by-handle query is the only route.

**Totals moved, and the move is exactly accounted for.** On each tree the
root-total delta equals the summed own-sizes of the non-root directories, to
the byte, with `tn` and `te` unchanged — nothing else shifted, nothing was
double-counted:

| tree | dirs | before (real) | after (real) | Δ = Σ dir own sizes | dirs with a real index |
|---|---|---|---|---|---|
| 200k synthetic | 8 301 | 16 384 | 34 013 184 | 33 996 800 | 4 200 / 8 300 |
| `C:\Windows\System32` | 1 591 | 8 986 968 432 | 8 995 070 320 | 8 101 888 | 329 / 1 590 |
| `C:\Windows` | 170 391 | 35 512 619 064 | 35 813 441 592 | 300 822 528 | 35 057 / 170 390 |

**How truth was established, without asking the API under test.** NTFS
exposes a directory's B-tree as a named stream: opening
`\\?\<dir>:$I30:$INDEX_ALLOCATION` **by name** and calling `GetFileSizeEx` is
a different object reached through a different call, and it agrees byte for
byte. At 0/1/10/50/100/200/400/800 long-named entries the oracle reports 0,
0, 4 096, 24 576, 49 152, 98 304, 196 608, 524 288 and camembert reports each
exactly — a step function in whole 4 KiB INDX blocks, with the two zeros
real (NTFS keeps a small index resident in the MFT record). Sampled across
the three trees above: **4 080 directories checked, 0 disagreements**
(System32 every 3rd, `C:\Windows` every 61st, bench tree every 11th).
`fsutil file layout` needs elevation and a free-space differential is
~MB-noisy on a live system drive; both were tried and are not the oracle
this is.

**Cost: at or below the noise floor**, which is the ~25 ms estimate holding.
The clean measurement is the same binary A/B (the query short-circuited by a
throwaway env var, both arms behind the same `cmd /c` wrapper so neither
pays for the other's shell): 200k tree 139.7 ms off vs **137.5 ms on**;
`C:\Windows` 4.184 s ± 1.073 off vs **3.189 s ± 0.189 on**. Both read as
"on is faster", which is not a claim — it is the measurement saying the
per-directory query is smaller than the run-to-run variance of a live
Windows box. Cross-binary runs bracket it the other way at +3 ms on the
200k tree, so the honest figure is **0–4 ms per 8 300 directories** and
**not distinguishable from zero on 170 391**. Beware: an early cross-binary
reading of +559 ms on `C:\Windows` was pure ambient interference and did not
reproduce (a repeat gave 3.213 s before vs 3.196 s after).

`scripts\bench-compare.ps1`, 200k tree, warm, Defender on and unmodified:
camembert **116.2 ms → 123.0 ms**, still 1.15× faster than gdu. Read that
pair with the noise in mind — gdu moved 152.0 → 141.7 ms between the same
two runs, i.e. the script's run-to-run spread is wider than the difference
it is being asked to show, which is why the controlled A/B above is the
number to trust.

`cargo test --workspace` on Windows: **396 pass, 0 fail** (2 ignored),
clippy clean at `-D warnings`, `cargo fmt --check` clean. The property is
pinned portably by
`camembert-core/tests/scan.rs::directory_size_does_not_depend_on_being_the_scan_root`
— Unix passed it already, Windows did not.

### Landing 4 — names decode back exactly, 2026-07-28

`docs/design/windows-delete-dossier.md` §2.8's measurement, shipped as
`camembert-core/src/wtf8.rs`: `wtf8_to_utf16(&[u8]) -> Option<Vec<u16>>`,
pure, portable, `unsafe`-free, tested on every platform.
`tree::os_name_from_bytes` now decodes a Windows name through it and
`OsString::from_wide`, so an interned name comes back **byte for byte** —
unpaired surrogates included. The old arm ran `String::from_utf8_lossy`,
which turned a lone surrogate into U+FFFD and made `o` (reveal) and `y`
(copy path) name a file that does not exist. The encode direction was
already exact (`scan::windows::worker::wtf8_name`); only the way back was
lossy.

Bytes that are not well-formed WTF-8 are **refused** by the decoder rather
than guessed at — overlong forms, scalars above U+10FFFF, truncated
sequences, and a surrogate *pair* written as two three-byte surrogates
(ill-formed WTF-8, and admitting it would give one name two encodings).
Such bytes can only come from a dump written on another platform, so
`os_name_from_bytes` keeps the lossy fallback for them and its doc says
plainly that a filesystem round-trip must call the decoder itself and
refuse on `None`. That turns "camembert cannot name a Windows file" into
"camembert refuses to name entries that did not come from this platform",
which is checkable.

Pinned by `tree::tests::a_windows_name_with_a_lone_surrogate_decodes_to_
itself` (verified to fail against the old lossy arm: *"[D800] came back as
a different name"*) plus eleven decoder tests, one of which round-trips
2000 pseudo-random UTF-16 strings — surrogate-biased, fixed seed —
through std's own WTF-8 encoder. Linux is untouched: its arm of
`os_name_from_bytes` is unchanged and the third arm
(`not(any(unix, windows))`) preserves the old behaviour for any other
platform.

### Landing 5 — the Recycle Bin meter, 2026-07-28

Slice 1 of the delete dossier's recommendation
([windows-delete-dossier.md](docs/design/windows-delete-dossier.md) §4.3,
§7), and the first of the two zero-destruction surfaces it says must exist
before any executor. `camembert-core/src/recycle.rs` (`cfg(windows)`,
read-only, no write path) asks `SHQueryRecycleBinW` about the volume
holding the scan root — resolved with `GetVolumePathNameW`, the same volume
`GetDiskFreeSpaceExW` measured for the gauge, so the two figures describe
one disk. On this box: **6 264 307 348 bytes across 66 items**, matching
the dossier's probe exactly.

That gap is the Windows twin of the `/proc` sweep's: `C:\$Recycle.Bin` is
hidden, per-SID and ACL'd, so no directory tree shows it, while the
free-space figure counts every byte as used.

- **Wording is the design.** The gauge grows `· 5.8 GiB in the Recycle Bin`
  and one thresholded toast says `Recycle Bin: 5.8 GiB in 66 items — not
  free until you empty it`. The word *freeable* is banned, and a test
  enforces the ban: on Linux it means "a `close(2)` away", and these bytes
  come back only when the user empties the bin, which camembert never does
  and never offers.
- **Threshold reuses freeable D5 verbatim** — ≥ 100 MiB *and* ≥ 1 % of
  capacity — restated in `camembert/src/ui/recycle_rt.rs` rather than
  imported, because `freeable_panel` is `cfg(unix)` and never compiles
  here. Suffix unthresholded, toast thresholded, exactly as on Linux.
- **Off the UI thread**, because it is not free: measured **16.5–23.3 ms**
  on a 66-item bin (`recycle::tests::bench_query_cost`, `#[ignore]`d), i.e.
  half a frame already, and a bin with tens of thousands of items is not
  bounded by that. One job thread, a one-shot channel, non-blocking
  `try_recv` in the event loop at step 2.57 — the freeable sweep's shape.
- **No CLI or env surface**, no key, no panel, no palette command. There is
  one number and one sentence; `?`/keymap/palette are untouched.
- **`\\?\` is stripped before the call.** The Windows backend carries the
  extended prefix everywhere and shell entry points refuse it (dossier
  §2.5e). Only a drive-letter root is rewritten; a UNC or volume-GUID path
  keeps its prefix and the call refuses, which is the honest outcome since
  those have no bin.
- Pinned by a `TestBackend` render test asserting the suffix appears, that
  it never says "freeable", and that an empty or unmeasured bin adds
  **nothing** (verified to fail with the suffix suppressed). Plus the
  wording/threshold unit tests and two live-call tests.

Linux is untouched: the gauge's freeable arm is byte-identical and the new
push sits behind `#[cfg(windows)]` after it.

### Decisions taken, not to be relitigated without a new element

- **`windows-sys` is a T1 dependency** (2026-07-26). A `std`-only walker
  had two holes sharing one key: `std` exposes neither allocation size nor
  link count on Windows on stable. So `sem` stays `"blocks"` and hardlink
  dedup stays on, which is what the thesis requires.
- **The taxonomy grows non-POSIX rows** (2026-07-26). `ERROR_SHARING_
  VIOLATION` is the error a Windows scan hits most after access-denied and
  `EBUSY` does not describe it. Wire name `WIN_SHARING_VIOLATION`, no `E`
  prefix, numbered from 2^24 so the decimal fallback can never collide
  with an errno, and unconditional so a Windows-written dump decodes on
  Linux.
- **The Windows scan does not ask for link counts** (2026-07-27, user
  decision on the dossier's recommendation). Hardlink dedup keys on the
  directory listing's file id and a repeat sighting, not on `nlink > 1`;
  `--links` opts back in. The load-bearing fact is that totals are
  byte-identical — reopening this needs a tree where they are not, not a
  preference. What is *not* settled and stays open: whether the summary
  counter's two meanings should ever be reconciled (they answer different
  questions and both are true), and whether the `⛓` *column* should ever
  be filled from the consumption-point query (landing 2 answered the
  selection card only — see above for why the column is a separate call).
- **The Windows TUI is reduced, not absent** (2026-07-26). Table, wheel,
  gauge, navigation, sorting, filtering, flat view, diff, dump and themes
  survive; deletion, the freeable panel, the confidence verdict and the
  FIEMAP floor line do not. A feature that cannot work is absent from the
  keymap and the help, never present-and-failing.
- **OPEN, needs a decision: the TUI's `hardlinks` metric card and the
  flat-view `⛓` column still read the pre-2026-07-27 meaning** (found by
  adversarial review, 2026-07-27). `--no-ui`'s summary line was made
  honest — it says "reached by more than one path in this scan" and names
  `--links` — but the card shows a bare number, and `ViewSnapshot` carries
  no `link_counts_known`, so it *cannot* qualify itself even if someone
  wanted it to. On `C:\Windows\System32\drivers` the card reads `0` by
  default and `728` under `--links` with nothing distinguishing them.
  Fixing it is two things and only the first is mechanical: thread the
  flag into `ViewSnapshot` (touches ~10 construction sites), then decide
  what the card should *say* — which is a wording choice on a user-facing
  surface, deliberately left to the user rather than picked by an agent.
  The same question governs the `⛓` column in flat view, where filling it
  from a live per-file query (landing 3 of the link-count work) would give
  one glyph two meanings across tree and flat view.
- **Capability detection keys on the terminal, and an absent `TERM` means
  the opposite thing on Windows** (2026-07-27). `TERM`/`COLORTERM` are a
  Unix convention that no Windows console sets — not `cmd`, not
  PowerShell, not Windows Terminal — so reading the silence as "advertises
  nothing" put every Windows user on the bottom rung of *both* ladders:
  `caps=Caps { color: Mono, glyphs: Ascii }`, i.e. no colour and no wheel
  at all, on hardware that renders 24-bit fine. The floor there is
  truecolor (the console has taken 24-bit SGR since Windows 10 1511 with
  VT processing, which crossterm enables); an explicitly set `TERM` still
  wins, keeping MSYS/Git Bash correct. Glyphs key on `WT_SESSION`:
  Windows Terminal gets sextants (its Cascadia font covers
  U+1FB00..U+1FB1F — *rendered on the box to check, not inferred*), any
  other console stops at half-blocks because a legacy `conhost` may be on
  a raster font and empty boxes are worse than a coarse wheel. The
  platform is carried in `TermEnv` as data rather than read from `cfg!`
  inside the detection, so both matrices are unit-testable from either OS.
  **Beware when probing this by hand: the agent harness sets `NO_COLOR` in
  its own shells**, which silently pins any measurement to `Mono` — clear
  it explicitly or you will measure your instrument.

### Why `errno.rs` was the blocker, in case it comes up again

Eleven of the taxonomy's 25 constants are `#[cfg(not(windows))]` in
rustix, and the fourteen that survive carry *Winsock* values — `EACCES` is
10013 there. The dump spec's decimal fallback (§6.4) would have silently
mis-classified a Linux-written dump read on Windows: the field that tells
a user whether their disk is dying or their permissions are wrong. Fixed
by owning the numbering. Note the pre-existing `known_names_round_trip`
test did NOT pin the wire — it walked TABLE against TABLE and would have
passed through a wholesale renaming. `wire_names_and_numbers_are_pinned`
is the one that actually pins it; update it deliberately.

### Known gaps, in rough value order

1. **Reparse points never enter the hardlink registry.** The guard that
   used to skip the *stat call* for anything the listing flagged now skips
   *registry admission* for it, so the consequence is unchanged and the
   fix is now cheaper: WinSxS is both heavily hardlinked *and* heavily
   WOF-compressed, so on a Compact-OS system those hardlinks do not dedup
   and `C:\Windows` over-reports. Under the old shape relaxing the guard
   meant paying 46 µs for every WOF file; under the new one it costs a
   hash insert. Deliberately **not** done in the same change as the
   registry rework — it moves totals, which that change is specifically
   claiming it does not, and it deserves its own before/after. Obvious
   next fix, and now a cheap one.
2. ~~The integration tests do not compile on Windows.~~ **Closed
   2026-07-27.** `cargo test --workspace` runs there: **374 tests pass, 0
   fail**, and the `windows-2025` CI job is now `cargo test` rather than
   `cargo check`. What was intrinsically Unix is gated at test-fn or
   module granularity, never whole-file where a portable test could
   survive: `delete`/`fiemap` (the modules they test are `cfg(unix)`), the
   io_uring parity pair, the `RLIMIT_NOFILE` exhaustion test, and
   `scan_a_known_tree` (it cross-checks against `nlink`/`dev`/`ino`, i.e.
   gap 5's missing oracle). The rest ports through a new
   `camembert-core/tests/support/mod.rs` picking a per-platform mechanism
   for the three fixtures that need one — non-UTF-8 name (raw invalid byte
   vs unpaired UTF-16 surrogate), symlink (skips cleanly without Developer
   Mode), unreadable directory (`chmod 000` vs an `icacls` deny ACE), each
   returning whether it *actually* worked so a test skips instead of
   silently passing. Three new portable tests recover the ground
   `scan_a_known_tree` used to cover alone. `tui_smoke.rs` stays Unix-only
   — ConPTY harness limitation, not a product one.
3. **A symlink's reported size differs from Unix** (noticed 2026-07-27,
   unverified): Unix reports the target string's byte length, while the
   Windows backend takes a reparse point's own `EndOfFile`/`AllocationSize`,
   which is likely 0. Probably *correct* rather than a bug — NTFS keeps the
   link text in the reparse buffer, not in file data — but nobody has
   confirmed the actual value, because creating a symlink needs Developer
   Mode or elevation and the dev box has neither. Confirm, then either
   document it as a platform difference or fix it; do not guess.
4. ~~**Subdirectories contribute 0 to directory-index bytes.**~~ **Closed
   2026-07-27** — see "Landing 3" above. Every directory the scan opens is
   now asked for its own size; the ones it never opens (junction, mount
   point, unknown reparse tag, failed open) keep the listing's 0, which is
   the honest answer with no handle to ask.
5. **Junctions are refused, not resolved**, so `--one-filesystem` is a
   no-op and junction-heavy trees under-count. Descending them needs cycle
   detection camembert does not have.
6. **No cross-check partner.** The APIs that would serve as an oracle
   (`MetadataExt::{file_index, number_of_links}`) are the nightly-only
   ones; `fsutil file queryfileid` shelled out from a test is the only one
   available. The *bench* half of this gap is closed —
   `scripts/bench-compare.ps1` (2026-07-27) makes CLAUDE.md's before/after
   mandate enforceable on Windows, and immediately found the nlink hole
   above.
7. Alternate data streams are invisible; deduplicated volumes report the
   stub. Both out of T1 scope, both worth a README line.

### Where the work actually happens

**The user's own machine is Windows 11 with a working MSVC toolchain**
(`cargo 1.97`, `x86_64-pc-windows-msvc`) — established 2026-07-27. That
supersedes this section's previous advice to treat CI as the only
authority and rent a VM for anything else. Build, test, run and benchmark
locally; `scripts/bench-compare.ps1` and the competitor binaries under
`target/bench-tools/bin` are set up there.

The `windows-2025` CI job is still the thing that lasts and the check that
protects the port from bit-rot, so keep it honest. It runs `cargo test
--workspace --locked` as of 2026-07-27 (gap #2 below), which is what makes
a hardlink-accounting change on this platform reviewable at all.

Kept for the record, should a second machine ever be needed: Scaleway
Windows instances cost ~0.38 EUR/h on the only SKUs supporting the image
(twenty times the DEV1-S validation lab), their images ship OpenSSH
pre-installed, and a `with-ssh` creation tag provisions the project's IAM
keys at first boot, so one is drivable headlessly with no RDP and no
password. The password-encryption key must be RSA. Delete the server WITH
its SBS volume and its IP — both survive server deletion and keep billing.

## Suggested next steps, in value order

1. **Freeable phase 2 slice 3** — composition: floor figures on
   flat-view rows, filtered-total floor sums, breakdown groups;
   `SortKey::Exclusive` + `excl` column only if it survives the
   attack-report reservations (freeable2 D2 says no sort key in
   phase 2 — reopening needs a new element, and attack-b findings 1-3
   are the standing argument against sort authority).
2. Wave 4 per the archived handoff: ssh remote scan, HTML export, watch
   mode (single-mutator design sketched in scan-tree docs), dated cache.
3. Per-directory inode counters + an `f_files`-near-limit alert (statvfs)
   — the archived design's "failure mode nobody surfaces".
4. Apparent/real slack surfacing across small-file masses (`st_blocks`
   is already carried per entry — effectively free to expose).
5. Quotas (`quotactl`, XFS project quotas) — needs its own dossier; on a
   shared machine the disk isn't always the real limit.
6. Composable stdout output of the marked selection, fzf-style
   (`rm $(camembert --print ...)`).
7. Cleanup recipes: display-only suggestions for known paths (e.g.
   `journalctl --vacuum-time=`, `pip cache purge`) — never executed,
   only shown.
8. Age/"big and stale" — **decided 2026-07-24: no score view.**
   [age-score-prototype.md](docs/design/age-score-prototype.md) measured
   seven formulas on five real trees: every continuous formula collapses
   onto either the size or the age axis, the threshold quadrant wins and
   already ships as `--filter '>10M older:1y'`, and mtime is widely
   fabricated (22.6k files share cargo's fixed 2006 timestamp; some
   directories carry negative mtimes). The README/`--help` claim was
   corrected to describe the filter — "big and *stale*", mtime not atime.
   [age-view-mockups.md](docs/design/age-view-mockups.md) keeps four
   surface designs should new evidence reopen it (a fileserver or NAS
   with genuinely cold data is the untested case; the prototype's §10
   lists what a rescan there would have to show). Optional follow-ups,
   not scheduled: a named filter preset expanding to a visible editable
   query, and a display guard badging absurd mtimes (1881, cargo-2006)
   rather than ranking them.
9. Traversal-dedup — **dossier delivered, awaiting a decision**:
   [traversal-dedup-dossier.md](docs/design/traversal-dedup-dossier.md)
   (recommends Option D: `statx` `MNT_ID_UNIQUE`/`SUBVOL` +
   `STATX_ATTR_MOUNT_ROOT` classification, mountinfo plan consulted only
   at boundaries, aliases skipped *visibly*, subvolumes labelled rather
   than deduped). Covers the bind-mount and snapshot-subvolume
   double-counting accepted in the crossing-by-default addendum (see
   [scan-tree-decisions.md](docs/design/scan-tree-decisions.md)) and adds
   three newly found alias cases (overlayfs lower/upper, ZFS
   `.zfs/snapshot` automounts, bind-mounted regular files).
10. `AT_NO_AUTOMOUNT` on the scan's `statx`/`statat` calls: camembert
    currently passes only `AT_SYMLINK_NOFOLLOW`, so on ZFS with
    `snapdir=visible` it *triggers* the automounts it then descends —
    the tool mutates the system it measures. One flag; the original
    design already called for excluding unmounted autofs. Hot path, so
    it needs the usual before/after benchmark.

## How to work on this repo

```bash
cargo test --workspace                                  # ~582 tests
cargo clippy --workspace --all-targets -- -D warnings   # zero tolerance
pre-commit run --all-files
```

Read the relevant decision doc before touching a subsystem. Update
README + `--help` with any CLI change. Never bump versions on your own.
The user prefers co-designing structural decisions and being offered
concrete options with a recommendation — bring dossiers, not open
questions. Every new dossier must answer, before options are drafted:
**what does this work displace, and does the thesis agree with that
trade?**
