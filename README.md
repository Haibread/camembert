<div align="center">

# 🧀 camembert

**A disk usage analyzer that answers the real questions.**

*What grew? What can I actually free? What is big **and** cold?*

[![CI](https://github.com/Haibread/camembert/actions/workflows/quality.yaml/badge.svg)](https://github.com/Haibread/camembert/actions/workflows/quality.yaml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

*(camembert is French for pie chart — yes, really)*

</div>

---

Every disk analyzer tells you what is big. **camembert** is built for the
questions you actually have during an incident:

- **What grew since yesterday?** — `camembert diff` two scans, sorted by
  growth, in streaming constant memory.
- **What can I actually free?** — freeable ≠ size: hardlinks are counted
  once and attributed deterministically; deleted-but-open files and btrfs
  shared extents are on the roadmap.
- **What is big *and* cold?** — size × age, visible at a glance.

And it is **honest about the numbers** other tools get wrong: hardlinks,
sparse files, unreadable directories (counted *and* located, never
silently missing), kernel pseudo-filesystems (`/proc` claims 128 TiB —
camembert never counts it), mount boundaries.

## The interface

A **dashboard cockpit** you can navigate *while the scan runs* — totals
fill in and re-sort live, and the donut wheel's slices grow in real time:

<div align="center">
  <img src="docs/images/tui.png" alt="camembert's interactive TUI: a dashboard with metric cards, a disk gauge, a sortable directory table with proportion bars, and a live donut wheel of the current directory's children" width="820">
</div>

The wheel is a real pie chart drawn in your terminal with sub-cell
pixels — sextants (2×3 per cell) on modern terminals, half-blocks
everywhere else. Each of the top children gets an **identity color**:
the same color paints its table row, its proportion bar, and its slice,
so your eye links them instantly. The palette is Tokyo-Night-family
truecolor with a full fallback ladder (256 → 16 → mono/ASCII) and
[`NO_COLOR`](https://no-color.org) support.

Everything you see is also clickable: table rows, wheel slices, the
breadcrumb, the errors card (see [Mouse](#mouse-interactive-mode) below)
— the keyboard map stays complete either way.

## Install

From source (Rust stable, edition 2024):

```bash
git clone https://github.com/Haibread/camembert
cd camembert
cargo install --path camembert
```

Prebuilt static binaries (x86_64 + aarch64 musl) will ship with the
first release.

## Quick start

```bash
# Browse a directory interactively (default on a terminal)
camembert /var

# Summary mode: totals + top directories, no UI
camembert /var --no-ui --top 10

# Scan and write a dump — the interchange format everything builds on
camembert /var -o today.cmbt

# THE feature: what changed between two scans?
camembert diff yesterday.cmbt today.cmbt

# Monitoring probe: exit 1 if growth exceeds the threshold
camembert diff yesterday.cmbt today.cmbt --threshold 500M --json

# Already have ncdu exports? Bring them along — no rescan needed
camembert import old-ncdu-export.json -o old.cmbt
camembert diff old.cmbt today.cmbt
```

Every option is also an environment variable (`THREADS`,
`CROSS_FILESYSTEMS`, `TOP`, `NO_UI`, `OUTPUT`, `THRESHOLD`, `COLOR`,
`LOG_FILTER`, `LOG_FILE`, …) — see `camembert --help` and
`camembert <subcommand> --help` for the full reference, including the
interactive key map and the diff JSON schema.

## Keys (interactive mode)

| | |
| --- | --- |
| `↓`/`j` `↑`/`k` | move · `⏎`/`l` open · `⌫`/`h` up · `g`/`G` ends |
| `d` `a` `n` `m` `c` `e` | sort: disk (default) · apparent · name · mtime · items · **errors** (again = reverse) |
| `p` | toggle the apparent-size column |
| `Space` `u` `D` | mark for deletion · clear marks · delete (confirm with `y`) |
| `q`/`Esc` | quit (cancels a running scan) |

**Deletion is guarded**: mark-then-confirm, mount points refused, every
entry re-checked (existence, file type, device) immediately before
removal — anything that changed since the scan is skipped, never
deleted. Symlinks are removed, never followed.

## Mouse (interactive mode)

Mouse support is additive — every key above keeps working, nothing
requires the mouse:

| | |
| --- | --- |
| Click a row | select it |
| Click it again, or double-click any row | open it (like `⏎`) |
| Wheel over the table | scroll the cursor |
| Click a donut slice | open that child directly |
| Click a breadcrumb segment (header) | jump to that ancestor (like `⌫` repeated) |
| Click the `errors` metric card | sort by subtree error count (like `e`) |
| Move the mouse over a row | update the selection card below the table, without moving the keyboard cursor |

Moving the keyboard cursor reclaims the selection card from the mouse.

## The dump format

`.cmbt` dumps are **JSON Lines in a seekable zstd container**
([spec](docs/format/dump-v1.md)) — versioned, crash-safe (written to
`.part`, renamed atomically), and readable with stock tools, no
camembert required:

```bash
zstdcat today.cmbt | jq -r 'select(.t == "d") | [.td, .path] | @tsv' \
  | sort -rn | head -5
```

Sibling order is raw-byte sorted, which is what makes `diff` a
streaming merge-join: two 10M-entry dumps diff in megabytes of RAM,
not gigabytes.

## Honest numbers

- **real** (`st_blocks × 512`, the default) vs **apparent** (`st_size`)
  are both always carried — sparse files and compression make them
  legitimately disagree.
- Hardlinked inodes count **once**, attributed to their canonical
  (smallest-path) link — deterministic across scans, so diffs never
  show phantom growth.
- Unreadable directories never abort a scan and never vanish: the
  summary lists exactly where reads failed; in the TUI, sort with `e`.
- Kernel pseudo-filesystems (`/proc`, `/sys`, cgroups…) are never
  descended into, even with `--cross-filesystems`.
- The disk gauge tells you how much of the *occupied* filesystem your
  scan actually covers — a total without context is half a lie.

## Roadmap

Scan engine, live TUI, dump v1, diff, ncdu import, and guarded deletion
are implemented. Next: the freeable column (deleted-but-open files,
btrfs shared extents), flat view and pattern aggregation, the filter
query language with a command palette, per-owner views, io_uring statx,
remote scan over ssh, and an HTML report export. The full design trail
lives in [`docs/design/`](docs/design/).

## Development

```bash
cargo test --workspace          # the suite (~130 tests)
pre-commit install              # fmt + clippy -D warnings + hygiene hooks
```

The workspace splits a pure core library
([`camembert-core/`](camembert-core/)) from the TUI/CLI frontend
([`camembert/`](camembert/)); design decisions are recorded in
[`docs/design/`](docs/design/) and are binding. See
[HANDOFF.md](HANDOFF.md) for the current project state.

## License

Dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
