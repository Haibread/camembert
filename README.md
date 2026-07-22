# camembert

A disk usage analyzer for Linux (ncdu successor) that aims to answer the real
questions:

- **What grew?** — diff between two scans, sorted by growth.
- **What can I actually free?** — freeable space is not the same as size
  (deleted-but-open files, hardlinks, btrfs shared extents).
- **What is big *and* cold?** — size × age scoring for deletion candidates.

And to be honest about the numbers other tools get wrong: hardlinks, sparse
files, filesystem compression, inodes, quotas, unreadable directories.

Named after the pie chart — *camembert* in French.

> **Status**: early development. The parallel scan engine (work-stealing
> `openat`/`getdents64`/`statx` traversal, streaming aggregation, hardlink
> dedup, mount-boundary detection), the interactive browse-during-scan
> TUI, and the ordered dump-format v1 writer (`--output`) are implemented.
> See [HANDOFF.md](HANDOFF.md) for the full design hypotheses and roadmap,
> `docs/design/` for the settled decisions, and
> [`docs/format/dump-v1.md`](docs/format/dump-v1.md) for the dump format.

## Layout

- [`camembert-core/`](camembert-core/) — pure library: scanning, aggregation,
  size semantics. No UI dependencies.
- [`camembert/`](camembert/) — the TUI/CLI frontend binary.

## Requirements

- Rust (stable, edition 2024 — 1.85+)
- [pre-commit](https://pre-commit.com/) for the git hooks (development only)

## Build

```bash
cargo build --workspace
```

## Run

When stdout is a terminal, `camembert` opens the **interactive mode** by
default (see below). With `--no-ui` (env `NO_UI`), or automatically when
stdout is a pipe or a file, it runs in **summary mode**: scan to
completion, then print totals plus the largest directories by real
(on-disk) size.

```bash
# Browse the current directory interactively (default on a terminal)
cargo run --release

# Summary mode: scan /var, print the top 10 directories
cargo run --release -- /var --no-ui --top 10

# Pin the worker-thread count (0 = auto: 2x CPU cores, capped at 8)
cargo run --release -- /home --threads 4

# Follow mount points into other filesystems (off by default:
# mount points are recorded but not descended into)
cargo run --release -- / --cross-filesystems

# Verbose engine diagnostics on stderr (product output stays on stdout)
cargo run --release -- /var --no-ui --log-filter camembert_core=debug

# Scan and write a dump (works in both modes; see "Dump" below)
cargo run --release -- /var -o var.cmbt
```

## Interactive mode

The core promise of the interactive mode: the tree is **navigable while
the scan runs**. Directories appear as they are discovered, totals fill in
and re-sort live (snapshots at ~30 fps; directories with more than ~20k
children update at 4 Hz and show an "updating…" note), and a spinner marks
directories still being scanned. Quitting mid-scan cancels the scan.

| Key | Action |
| --- | --- |
| `↓` / `j`, `↑` / `k` | move the cursor |
| `Enter` / `l` / `→` | open the directory under the cursor |
| `Backspace` / `h` / `←` | go back up to the parent |
| `g` / `G` | jump to the top / bottom |
| `d` | sort by real (disk) size — the default, descending |
| `a` | sort by apparent size |
| `n` | sort by name (raw bytes, ascending) |
| `m` | sort by modification time |
| `c` | sort by item count |
| *(active sort key again)* | reverse the sort direction |
| `p` | show/hide the apparent-size column |
| `q` / `Esc` / `Ctrl-C` | quit (cancels the scan if still running) |

**Provisional totals (hardlinks)**: while the scan is running, a
hardlinked inode is attributed to the directory where it was *first seen*.
If any hardlinks were encountered, the footer shows *"provisional totals
(hardlinks) — corrected at scan end"* until the scan completes — the
final numbers count each inode exactly once. The note only appears when
hardlinks actually exist in the tree.

Diagnostics (`tracing`) never touch the interactive screen: they are
discarded by default, or written to a file with `--log-file scan.log`
(env: `LOG_FILE`) when you need them while debugging.

## Summary mode

Example output:

```text
Scanned /usr/share/licenses in 0.04s
  total: 18.7 MiB real, 16.0 MiB apparent
  entries: 1713 (591 dirs)  errors: 0  excluded (other fs): 0

Top 5 directories by real size:
    18.7 MiB  /usr/share/licenses
     5.8 MiB  /usr/share/licenses/slack-desktop
```

Notes on the numbers (honesty is the point of this tool):

- **real** is `st_blocks * 512` (what the tree occupies on disk),
  **apparent** is `st_size`; sparse files, compression and tail slack make
  them legitimately disagree.
- Hardlinked inodes are counted **once**, attributed to their canonical
  link (the smallest path in raw-byte order) — deterministic across
  scans of an identical tree.
- Unreadable directories never abort the scan: they are counted in
  `errors` and the affected totals stay honest about what was not read.
- Symlinks are never followed; they count with their own (link) size.

While a scan runs, a progress line (entries, dirs, errors, bytes so far)
is logged to stderr every second.

Every CLI option is also settable through an environment variable
(`SCAN_PATH`, `LOG_FILTER`, `LOG_FILE`, `THREADS`, `CROSS_FILESYSTEMS`,
`TOP`, `NO_UI`, `OUTPUT`); see `cargo run -- --help` for the full reference,
including the interactive-mode key map.

## Dump

`--output FILE` / `-o FILE` (env: `OUTPUT`) writes a **camembert-dump v1**
(`.cmbt`) once the scan completes — the interchange format scans are
diffed and re-browsed from (spec:
[`docs/format/dump-v1.md`](docs/format/dump-v1.md)). It works in both
modes: summary mode writes it after the summary; interactive mode writes
it as soon as the scan finishes (quitting mid-scan cancels the scan and
skips the dump). The file is written to `FILE.part` first and atomically
renamed, so a crash never leaves a truncated dump under the final name.

The container is JSON Lines inside a seekable zstd stream, so **stock
tools read it directly** — no camembert needed:

```bash
# Scan /var and dump it
camembert /var --no-ui -o var.cmbt

# Inspect with standard tools: one JSON object per line
zstdcat var.cmbt | jq .

# e.g. the 5 biggest directories by on-disk subtree size
zstdcat var.cmbt | jq -r 'select(.t == "d") | [.td, .path] | @tsv' \
  | sort -rn | head -5

# '-' streams the dump to stdout instead (summary mode only)
camembert /var --no-ui -o - | zstdcat | jq -c 'select(.t == "e")'
```

Format notes:

- One header line (`t:"h"`) declares the version and capabilities; a
  final `t:"e"` line marks clean completion. Directory lines (`t:"d"`)
  carry subtree totals (`ta`/`td`/`tn`/`te`); their child entries follow
  with per-entry sizes, mtime, and kind.
- Filenames are raw bytes: non-UTF-8 bytes appear percent-encoded
  (`%XX`), `%` as `%25`. Sibling order is always raw-byte order.
- `ino`/`dev` are JSON **strings**, and any integer ≥ 2^53 is emitted as
  a string too, so `jq`/JavaScript arithmetic never silently corrupts
  them.
- Hardlinked inodes keep full metadata on every link (`i`/`l` fields) but
  are counted once in totals, at the canonical link.

## Test

```bash
cargo test --workspace
```

## Development

Install the git hooks once:

```bash
pre-commit install
```

Lint and format checks match CI expectations:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
