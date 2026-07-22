# camembert — project handoff

State of the project as of 2026-07-22, written for the next agent (or
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
- The dump format spec is [docs/format/dump-v1.md](docs/format/dump-v1.md);
  writer AND reader implement it. Major-version changes are near-taboo
  (they invalidate every stored dump).
- The options dossiers + adversarial attack reports next to the decision
  docs are the reasoning trail — read them before proposing to revisit.
- Workflow: co-design structural decisions with the user; implement
  autonomously once settled; direct commits on `main`, small and atomic;
  agents work in worktrees, the orchestrator reviews and merges.
- Never put the user's real name or personal email anywhere; the repo
  identity is `Haibread <haibread@users.noreply.github.com>` (set
  repo-locally).

## What is implemented (all merged on main, ~145 tests green)

- **Scan engine** (`camembert-core/src/scan/`): work-stealing,
  fd-relative `openat`/`getdents64`/`statx` (fstatat fallback), mount
  boundaries by `st_dev`, **kernfs excluded by filesystem magic even
  with `--cross-filesystems`**, single-owner arena integration with
  bounded out-of-order holding, per-directory batched aggregation,
  completion cascade, first-seen hardlink registry + post-scan canonical
  re-attribution.
- **Tree** (`tree.rs`): 32-byte nodes, run-list children (D2), subtree
  aggregates, tombstoned removal with negative-delta propagation,
  excluded-reason side map.
- **TUI** (`camembert/src/ui/`): browse-during-scan (arc-swap view
  snapshots, latest-wins nav cell, 33 ms cadence), dashboard cockpit
  (metric cards, statvfs disk gauge, table + donut wheel with identity
  colors, selection card), capability ladders (truecolor→mono,
  sextants→ASCII, NO_COLOR/--color), guarded mark-then-confirm deletion,
  log output never touches the terminal (--log-file).
- **Dump v1** (`dump.rs` + `dump/read.rs`): ordered writer (`-o`,
  `.part`+rename, seekable zstd, `zstdcat|jq`-compatible — verified) and
  streaming reader (torn-frame tolerant, number-or-string u64s).
- **Diff** (`diff.rs`, `camembert diff`): streaming merge-join, bounded
  memory, Added/Removed/Grown/Shrunk/Touched/TypeChanged, `--json`,
  `--threshold` (exit 1 = growth exceeded; 2 = error).
- **ncdu import** (`ncdu.rs`, `camembert import`): hand-rolled streaming
  JSON lexer (handles non-UTF-8 pre-2.5 exports), rebuilds the arena,
  canonical hardlinks, emits ordered dumps. Import→self-diff = zero.
- **Infra**: pre-commit (fmt, clippy -D warnings, actionlint, hygiene),
  GitHub workflows `quality` + `release` (SHA-pinned), Dependabot,
  dual MIT/Apache-2.0, repository metadata. **The GitHub repo
  (github.com/Haibread/camembert) does not exist yet — creating and
  pushing it is the user's own action.**

## Known limitations (documented in code where they live)

- No io_uring statx yet; no HDD-adaptive threading; worker fd usage can
  approach RLIMIT_NOFILE on pathologically wide trees; a worker panic
  hangs the scan (owner panics are handled).
- Deletion: intermediate-symlink TOCTOU window (needs a descriptor-
  relative unlink walk); freed-space estimate for surviving hardlinks is
  optimistic (warned in dialog).
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

## Suggested next steps, in value order

1. **TUI slices 3–6** of [tui-design.md](docs/design/tui-design.md):
   mouse + clickable breadcrumb/cards, deletion basket + `v` review,
   toasts, `?` cheatsheet, animations + `--no-motion`, responsive
   mini-donut, themes + `camembert.toml` + OSC 11 light detection. The
   design doc is the spec; slices 1–2 (cockpit + wheel) are done.
2. **Freeable column, phase 1**: deleted-but-open files via
   `/proc/*/fd` + `(deleted)` symlinks with guilty PID; reuse it for
   the deletion open-file warning. Phase 2: btrfs
   `FIEMAP_EXTENT_SHARED`. (ZFS: show nothing rather than invent.)
3. **Flat view + pattern aggregation** (`node_modules` = 14 GiB
   cumulative): same aggregation machinery over the frozen arena;
   rayon-friendly (see option C's frozen-structure idea in the
   scan-tree dossier).
4. **Filter query language + Ctrl-K palette** (they ship together —
   the palette is the language's UI, reserved in the design).
5. **io_uring batched statx** with runtime detection + fallback (spec'd
   in the original handoff §3; the completion invariant it needs is
   already documented in owner.rs).
6. **Release engineering**: musl static builds (x86_64 + aarch64) in the
   release workflow, `--version` embedding, first tag.
7. Wave 4 per the archived handoff: ssh remote scan, HTML export, watch
   mode (single-mutator design sketched in scan-tree docs), dated cache.

## How to work on this repo

```bash
cargo test --workspace                                  # ~145 tests
cargo clippy --workspace --all-targets -- -D warnings   # zero tolerance
pre-commit run --all-files
```

Read the relevant decision doc before touching a subsystem. Update
README + `--help` with any CLI change. Never bump versions on your own.
The user prefers co-designing structural decisions and being offered
concrete options with a recommendation — bring dossiers, not open
questions.
