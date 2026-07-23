# Option C — split surfaces: `/` filters, Ctrl-K commands (the k9s school)

> Proposal pushed as hard as it honestly can be: filtering and
> commanding are **different verbs** and deserve different keys — a
> lightweight always-visible filter prompt on `/`, and a pure command
> palette on Ctrl-K. Not a decision. Facts referenced from
> [query-research.md](query-research.md) (§n).

## 1. Pitch

The strongest palette precedents that *stayed learnable* did not
overload one input: k9s gives `:` to "go somewhere / do something"
and `/` to "narrow what I'm looking at", and is praised for exactly
that clarity; Notion splits `/` (insert) from Cmd-K (navigate); yazi
likewise (research §4.6, §4.7). Option C follows the split. **`/`**
drops a one-line filter prompt into the cockpit (between table and
footer — not an overlay, the tree stays fully visible) speaking the
same qualifier-token language as option A, re-aggregating live as you
type. **Ctrl-K** is a *pure* command palette: fuzzy over every
key-map action, saved queries as ready-made commands, recents on
empty input — no query grammar inside it, ever. Each surface does one
thing, has one error model, and can ship, be tested, and be
documented separately. The user who never learns the palette still
gets the filter; the user who never filters still gets the palette.

## 2. The `/` filter prompt

- Opens on `/` (free key, research §2.5); a single input line slides
  in above the footer, cockpit intact — the point of a live filter is
  watching the tree respond, so **nothing covers the tree** (the
  overlay options cover part of it by construction).
- Language: option A §2's qualifier tokens, unchanged (same parser,
  same reserved set, same never-error model, same parse echo — shown
  in the prompt's right half). This option is *not* a different
  language; it is a different *home*.
- Live re-aggregation with the same debounced rayon fold as option A
  §4 (the engine is shared across all three options).
- Enter **commits**: the prompt closes, the filter pill appears in
  the header (option A §3.2). Esc while in the prompt **restores the
  state before `/`** — live preview is free to explore because
  backing out is always one key. With a committed filter, Esc in
  tree view clears it (same single Esc-ladder addition as A).
- During the scan: `/` flashes the marks idiom ("filter available
  when the scan completes") — the prompt never opens on a moving
  arena.

## 3. The Ctrl-K command palette

A centered overlay (same ladder rung as A), **commands only**:

- Fuzzy (subsequence) list generated from the `keymap.rs` dispatch
  table (cheatsheet's source — palette and keys cannot drift), plus
  palette-only entries: "clear filter", "filter…" (which just closes
  the palette and opens `/` — the two surfaces compose by
  *delegation*, never by embedding), and every `[queries]` saved
  query as `filter: <label>` (Enter applies it directly).
- Empty input: recent commands + saved queries (research §4.7).
- Because no query is ever typed here, the palette needs no parser,
  no echo line, no debounce — it is the smallest palette any option
  can ship, and it works fully during the scan (commands are
  phase-aware exactly as their keys already are).

History: `/` gets the persisted query history (Up/Down recall,
`$XDG_STATE_HOME/camembert/history`); Ctrl-K gets in-memory command
recents. Two lists that never mix — a filter string can never be
"run" as a command by accident.

## 4. Semantics, engine, CLI

Identical to option A: candidate model and dir-marking refusal
(A §2.2, §3.3), fold + `FilterGeneration` + epoch invalidation
(A §4), post-scan-only phase 1, `--filter` CLI slice and
dump-never-filtered rule (A §5), docs same-change rule. The dossier's
side-by-side therefore compares C against A on **surface shape
alone** — that is deliberate: C exists to make the "one input vs two"
decision explicit instead of implicit.

## 5. What the split buys, concretely

1. **No sigil grammar.** A's `>`-prefix mode switch inside one input
   is broot's documented discoverability pain (research §4.6). Here
   the keybinding *is* the mode; `?` cheatsheet lists both.
2. **Two small error models** instead of one compound one: the
   prompt is never-error (Everything school); the palette has no
   errors at all. A's single input must explain, in one status line,
   both "unknown qualifier" and "no matching command".
3. **The tree stays visible while filtering** — an overlay palette
   covering the donut while the donut re-aggregates wastes the
   feature's best moment.
4. **Independent shipping**: the prompt (with the engine) is the
   flagship slice; the palette can land a release later without
   blocking the query language — or vice versa. A must ship both at
   once because they are one widget.
5. **Precedent fit**: every TUI the target audience already runs
   (vim/less `/`, k9s, fzf-bound shells) reads `/` as "filter this
   view". Ctrl-K palettes are an editor/web convention — honoring
   both idioms separately meets both muscle memories.

## 6. Honest weaknesses

1. **The Ctrl-K reservation said** "command palette — ships with the
   filter/query language as its UI" ([tui-design.md](tui-design.md)).
   C re-reads that as "ships alongside": the palette hosts *entry
   points* (saved queries, "filter…") but the language lives in `/`.
   If the session holds the original reading literally, C is out of
   spec by definition — this needs an explicit call.
2. **Two surfaces to learn and document**: two keys, two prompts,
   two history stores, two footer hint sets. The split that makes
   each half simpler makes the whole bigger.
3. **A one-line prompt has no room** for a completion list or rich
   hints — qualifier discovery falls back to the cheatsheet/README
   (Everything has the same limitation and survives it, but
   Everything's users had its help page; ours get one echo line).
4. **Saved-query recall is palette-side while query *editing* is
   prompt-side**: applying a saved query then tweaking it means
   Ctrl-K → apply → `/` → Up. Workable, but the seam shows exactly
   where the two surfaces meet.
5. Shared with A: debounced synchronous fold hitch at 10 M; reserved
   -token debt; excluded-mount blind spots; dir-inode accounting
   rule.
