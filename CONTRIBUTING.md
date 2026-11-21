# Contributing

Questions and proposals go in a [GitHub issue](https://github.com/Haibread/camembert/issues).
For a vulnerability, follow [SECURITY.md](SECURITY.md) instead. The short
version: build, run the tests, install the pre-commit hooks, open a pull
request.

## Development setup

You need:

- Rust stable 1.88 or later, the floor declared as `rust-version` in
  [`Cargo.toml`](Cargo.toml), on Linux or Windows. Install it with
  [rustup](https://rustup.rs) on either OS, then add the components the hooks
  call:

  ```bash
  rustup component add rustfmt clippy
  ```

- [pre-commit](https://pre-commit.com), which also pulls the hook
  environments (yamllint, actionlint) by itself:

  ```bash
  pipx install pre-commit
  ```

  On macOS, `brew install pre-commit` works too.

From a fresh clone:

```bash
cargo build --workspace
```

```bash
pre-commit install
```

`pre-commit install` registers both the `pre-commit` and `commit-msg` hooks.
The setup works when `cargo test --workspace` passes. See
[README.md](README.md#install) for running the binary and
[README.md](README.md#development) for the man page, completion and package
builds.

## Running the tests

The whole suite, which must be green before a pull request:

```bash
cargo test --workspace
```

One crate, one test file or one test:

```bash
cargo test -p camembert-core
```

```bash
cargo test -p camembert-core --test scan
```

```bash
cargo test -p camembert-core --test scan -- <test_name>
```

Unit tests sit next to the code. Integration tests live in
[`camembert-core/tests/`](camembert-core/tests/) (scan engine, dump format,
rescan, deletion, fiemap) and [`camembert/tests/`](camembert/tests/) (CLI,
man pages, packaging, and `tui_smoke.rs`, which drives the TUI through a pty
and runs on Unix only). None of them needs a service, credentials or a
container runtime; they build their own directory trees in temporary
directories.

Tests marked `#[ignore]` are timing-sensitive or are subprocess helpers of
another test, and are skipped by default. Run them explicitly:

```bash
cargo test --workspace -- --ignored
```

A new behaviour comes with tests, and a fix comes with a regression test.

Changes to the scan hot path (`camembert-core/src/scan/`, `tree.rs`,
owner-side accumulators) are also benchmarked before and after with
[`scripts/bench-compare.sh`](scripts/bench-compare.sh) (or
[`scripts/bench-compare.ps1`](scripts/bench-compare.ps1) on Windows); see
[CLAUDE.md](CLAUDE.md#benchmarks-regression-guard--external-comparison) for
the setup.

## Pre-commit hooks

The hooks in [`.pre-commit-config.yaml`](.pre-commit-config.yaml) must pass
before you push. Run them on the whole repository:

```bash
pre-commit run --all-files
```

Or re-run a single hook while iterating:

```bash
pre-commit run cargo-clippy --all-files
```

| Hook | Checks | Fix |
| --- | --- | --- |
| `cargo-fmt` | formatting | `cargo fmt --all` |
| `cargo-clippy` | lints, warnings are errors | fix the lint it reports |
| `yamllint` | YAML style, strict | fix the line it reports |
| `actionlint` | workflow syntax | fix the workflow it reports |
| `no-co-authors` | no `Co-authored-by` trailer or "generated with" line in a commit message | reword the message |

`--no-verify` and `SKIP=` are not the fix. A red hook is a real finding: fix
the code or, when the rule is wrong for this repository, change the
configuration in the same pull request and say why. The same hooks run in CI,
so skipping them locally only moves the failure somewhere slower.

## Continuous integration

| Workflow | Purpose | Reproduce locally |
| --- | --- | --- |
| [`quality.yaml`](.github/workflows/quality.yaml) | pre-commit on all files and the test suite on Linux, the test suite on Windows, a build on the declared minimum Rust version | `pre-commit run --all-files` and `cargo test --workspace --locked` |
| [`release.yaml`](.github/workflows/release.yaml) | builds and publishes the release artifacts from a `v*` tag | none; it publishes |

Every job in `quality.yaml` must be green to merge. The Windows job has no
local equivalent on Linux or macOS; it runs the same `cargo test` command, so
a failure there points at a `cfg(windows)` arm. The minimum-version job is
reproduced with:

```bash
cargo +1.88 check --workspace --locked --all-targets
```

Neither workflow uses a secret beyond the repository's own `GITHUB_TOKEN`.
Actions are pinned to commit SHAs; Dependabot proposes the updates
([`dependabot.yaml`](.github/dependabot.yaml)).

## Submitting a change

- Work on a branch and open a pull request against `main`.
- Commit subjects are imperative and lowercase, like `git log --oneline`.
- User-facing changes are documented in the same pull request: in the clap
  `--help` text and in the [README](README.md), plus an entry under
  `Unreleased` in [CHANGELOG.md](CHANGELOG.md).
- Settled design decisions in [`docs/design/`](docs/design/) are binding;
  reopening one needs new evidence.
- Label the pull request `feature`, `fix`, `chore` or `dependencies`;
  [`.github/release.yaml`](.github/release.yaml) turns those into release
  notes categories.

## Releasing

Maintainers only. Move the `Unreleased` changelog entries under a new
version heading in [CHANGELOG.md](CHANGELOG.md), set `version` in the
`[workspace.package]` table of [`Cargo.toml`](Cargo.toml), commit, then tag
and push:

```bash
git tag vX.Y.Z
```

```bash
git push origin vX.Y.Z
```

[`release.yaml`](.github/workflows/release.yaml) builds the binaries,
packages and checksums, and publishes the GitHub Release with that version's
changelog section as its notes. It fails when the changelog has no section
for the tag.
