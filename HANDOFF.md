# camembert — project handoff

State of the project as of 2026-07-24, written for the next agent (or
human) picking it up. The original ideation document is archived at
[docs/design/handoff-original.md](docs/design/handoff-original.md); this
file describes what actually exists.

## What camembert is

A disk usage analyzer (ncdu successor) in Rust whose thesis is
**differentiation through honest answers to real questions**: what grew
(diff), what is actually freeable, what is big *and* cold — with numbers
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

## What is implemented (all merged on main, ~559 tests green)

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
  → min(2×cores, 8); measured 95 → 76 ms on the bench tree) and
  **io_uring-batched statx** (per-worker rings, runtime probe, sync
  fallback, auto-engaged at ≤ 2 workers where it wins 12-21 %;
  `--statx-engine`/STATX_ENGINE experimental override).
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
  GitHub Release (notes categorized via `.github/release.yaml` labels);
  `--version` embeds the build commit (build.rs, `-dirty` aware).
  **v0.1.0 is released and end-to-end verified** (sha256 + static-pie +
  version output checked from the published artifact).
- **Infra**: pre-commit (fmt, clippy -D warnings, actionlint, hygiene),
  GitHub workflows `quality` + `release` (SHA-pinned), Dependabot,
  dual MIT/Apache-2.0, repository metadata. The GitHub repo is live at
  [github.com/Haibread/camembert](https://github.com/Haibread/camembert)
  (public, `quality` CI green on main).

## Known limitations (documented in code where they live)

- io_uring statx ships behind an auto heuristic: engages at ≤ 2
  resolved workers (the HDD tier), where it measures 12-21 % faster;
  at high warm thread counts it loses to io-wq context-switch storms,
  so auto stays sync there. Threshold is warm-cache-derived — retune
  after cold-cache/real-HDD runs (`--statx-engine` forces either
  engine). Worker fd usage can approach RLIMIT_NOFILE on
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
8. Age/"big and cold" score view (size × age) — **dossier delivered,
   awaiting a decision**:
   [age-score-prototype.md](docs/design/age-score-prototype.md) measured
   seven formulas on five real trees and recommends building *no* score
   (every continuous formula collapses onto either the size or the age
   axis; the threshold quadrant wins and already ships as
   `--filter '>10M older:1y'`; mtime is widely fabricated — 22.6k files
   share cargo's fixed 2006 timestamp, some directories carry negative
   mtimes), while [age-view-mockups.md](docs/design/age-view-mockups.md)
   designs four surfaces should the score be built anyway. The actionable
   item either way: README's "size × age, visible at a glance" describes
   a feature that does not exist.
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
cargo test --workspace                                  # ~559 tests
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
