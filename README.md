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

> **Status**: early bootstrap. The scan engine is not implemented yet. See
> [HANDOFF.md](HANDOFF.md) for the full design hypotheses and roadmap.

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

```bash
cargo run -- --help
```

Every CLI option is also settable through an environment variable (e.g.
`LOG_FILTER` for `--log-filter`); see `--help` for the mapping.

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
