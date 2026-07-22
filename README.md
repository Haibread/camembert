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
> dedup, mount-boundary detection) is implemented; the TUI and the dump
> format are not yet. See [HANDOFF.md](HANDOFF.md) for the full design
> hypotheses and roadmap, and `docs/design/` for the settled decisions.

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

Scan a directory and print totals plus the largest directories by real
(on-disk) size:

```bash
# Scan the current directory, top 20 by default
cargo run --release

# Scan /var, show the top 10 directories
cargo run --release -- /var --top 10

# Pin the worker-thread count (0 = auto: 2x CPU cores, capped at 8)
cargo run --release -- /home --threads 4

# Follow mount points into other filesystems (off by default:
# mount points are recorded but not descended into)
cargo run --release -- / --cross-filesystems

# Verbose engine diagnostics on stderr (product output stays on stdout)
cargo run --release -- /var --log-filter camembert_core=debug
```

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
- Hardlinked inodes are counted **once** (first seen); when any were seen
  the summary says so, since per-path attribution is provisional.
- Unreadable directories never abort the scan: they are counted in
  `errors` and the affected totals stay honest about what was not read.
- Symlinks are never followed; they count with their own (link) size.

While a scan runs, a progress line (entries, dirs, errors, bytes so far)
is logged to stderr every second.

Every CLI option is also settable through an environment variable
(`SCAN_PATH`, `LOG_FILTER`, `THREADS`, `CROSS_FILESYSTEMS`, `TOP`); see
`cargo run -- --help` for the full reference.

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
