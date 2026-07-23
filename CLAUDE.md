# CLAUDE.md

Disk-usage analyzer (ncdu successor) in Rust. Read [HANDOFF.md](HANDOFF.md)
(current project state and next steps) before any design or implementation
work. The original ideation document is archived at
[docs/design/handoff-original.md](docs/design/handoff-original.md); settled
decisions live in `docs/design/*-decisions.md` and are binding.

## Documentation

Everything added to the CLI/app (commands, flags, env vars, behaviors,
output formats) must be documented in the same change:

- in the command's `--help` (clap doc comments — self-sufficient, including
  env var names and syntax of non-trivial values);
- and in the user-facing docs (README for now; the format may change later —
  man page, mdBook, … — but the information must exist from day one so it
  can be migrated rather than reconstructed).

No undocumented feature lands, even experimental ones — mark them as
experimental instead.

## Benchmarks (regression guard + external comparison)

Any change touching the scan hot path (`camembert-core/src/scan/`,
`tree.rs`, owner-side accumulators) must be benchmarked **before and
after** with:

```bash
scripts/bench-compare.sh              # warm cache, 200k-file synthetic tree
scripts/bench-compare.sh --cold       # page cache dropped (sudo), the real contest
```

The script builds `--release`, generates a deterministic synthetic tree
(cached under `target/`), and compares camembert (`--no-ui`) against
every known disk-usage tool it finds: `du` (always), plus `diskus`,
`dust`, `dua`, `pdu` from `target/bench-tools/bin` — populate once with

```bash
cargo install --locked --root target/bench-tools hyperfine du-dust dua-cli parallel-disk-usage diskus
```

(`ncdu`/`gdu` are C/Go: install system-wide if wanted; the script picks
up whatever exists.) Results are printed and exported to
`target/bench-results/<timestamp>.{md,json}` (+ `latest.*`), kept out
of git — compare `latest.md` against the previous run's file. A
camembert slowdown against its own previous local run, or falling
behind `gdu`/`dust`-class scanners on the same tree, is a regression:
fix it or explain it in the change that introduces it.

## Agents and model selection

Delegate work to subagents (Agent tool) whenever it helps, and pick the model
based on the task at hand — not one-size-fits-all:

- **haiku** — cheap and fast. Mechanical or low-risk work: codebase searches,
  crate/docs lookups, repetitive edits, formatting, boilerplate.
- **sonnet** — the default workhorse. Standard implementation: TUI views,
  CLI plumbing, tests, focused refactors, straightforward features.
- **opus / fable** — expensive, use deliberately. The hard parts of this
  project: scan-engine concurrency (lock-free aggregate updates during scan),
  unsafe/syscall-level code (`openat`/`getdents64`, io_uring + fallback),
  dump-format design, size-semantics correctness (hardlinks, btrfs extents,
  sparse files), adversarial review of any of the above.

Guidelines:

- Match the model to the *hardest part* of the delegated task, not its size.
- Fan out independent subtasks in parallel; keep each agent's scope narrow.
- When unsure, omit the model override and let the agent inherit the session
  model.
