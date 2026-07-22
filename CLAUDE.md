# CLAUDE.md

Disk-usage analyzer (ncdu successor) in Rust. All project context, design
hypotheses, and roadmap live in [HANDOFF.md](HANDOFF.md) — read it before any
design or implementation work. It is a set of challengeable hypotheses, not a
frozen spec.

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
