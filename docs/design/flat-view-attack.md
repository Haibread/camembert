# Adversarial review — flat view + pattern aggregation dossier

> Verdict: **SURVIVABLE WITH AMENDMENTS** — the skeleton is right and its
> two load-bearing engine claims (topological `DirId` dedup, hardlink
> re-attribution already landed before the fold) are *literally true in the
> code*. What does not survive as written is the presentation layer: the
> "category camembert" headline does not compose with the dossier's own
> overlap decision or the wheel's real math; the cache-invalidation model
> leaves a stale, dishonest frame when you delete *while already in* flat
> mode; and the config story collides with `parse()`'s all-or-nothing
> fallback. Plus a cluster of oversold "cheap"/"honest" numbers.

## The through-line flaw

The dossier is two documents wearing one cover. The **engine half** (axes 3
and 4 — the mask pass, the fused fold, the aggregate cross-check) is careful,
grounded, and mostly correct against `main`. The **surface half** (axis 1 —
the donut, mode switching, cache lifecycle) is asserted to "compose
orthogonally, as today" and largely *not* checked against the widgets it
rides on. Every serious finding below is a surface/engine seam the dossier
declared clean without looking: the donut's `MIN_SLICE_FRACTION` merge, the
`serve_local`/epoch lifecycle under a delete-in-mode, and `config::parse`'s
wholesale fallback. The grounding section earned trust on the engine and
then spent it, unearned, on the UI.

## Findings (severity-ranked)

### SERIOUS — the cache-invalidation trigger misses "delete while in the mode" → a stale, dishonest frame [1]

Axis 4 keys the cached `FlatReport` on a new `mutation_epoch` "bumped only by
successful deletion batches" and says to "compute lazily **on first `t`/`b`**
after scan end or after a deletion." That trigger model is mode-*entry*. But
marks work from flat mode (axis 1, and confirmed: `MarkedEntry` is
view-independent, the basket/review/confirm ladder is mode-agnostic —
ui.rs:646-708), so the natural gesture is: press `t`, mark three huge files,
`D`, `y` — **without ever leaving flat mode**. Deletion runs
`ScanOutcome::apply_removal` (scan.rs:604), which tombstones and subtracts up
the chain; the modal closes; the next frame re-renders flat mode from the
*cached* report, which still lists the just-deleted file with its old size.
On a tool whose entire thesis is "never show a number you can't stand
behind," the flat view now shows a file that no longer exists, as space still
occupied — the exact dishonesty the feature exists to avoid, produced by its
own delete path.

The fix is not the counter (the counter is fine) — it is the *check site*.
The report must be recomputed on an **epoch mismatch detected at render
time** while in a non-tree mode, not "on first `t`/`b`". The dossier's own
`Option<(mutation_epoch, FlatReport)>` shape supports this; its prose
("lazily on first `t`/`b`") describes the wrong trigger. State it as: every
frame in flat/breakdown mode, if `cache.epoch != mutation_epoch`, recompute
before drawing.

Amendment (mandatory): specify render-time epoch reconciliation, and add a
test that marks-and-deletes *inside* flat mode and asserts the deleted row is
gone on the very next snapshot.

### SERIOUS — the donut does not compose with either the mode or the overlap decision [2]

Axis 1 calls the breakdown donut "a camembert of categories… the single most
on-brand rendering this feature has" and the flat donut "top-N file slices
(+ an 'everything else' slice)." Neither survives contact with `wheel.rs`.

**Flat mode → a gray disc.** `build_slices` merges every slice below
`MIN_SLICE_FRACTION = 0.02` (wheel.rs:18, :282) into the gray "rest." On any
tree big enough to want a top-files view, no *single file* is ≥ 2% of the
whole scan — so every one of the up-to-1000 file slices merges, and the
flat-mode donut renders as one near-uniform gray circle that reads as
"everything is uncategorized." The dossier's "top-N file slices" never
appear. (If instead the intent is to normalize against the top-N subset
rather than the scan total, the dossier must say so — it says "proportion bar
(of scan total)" for rows, which is the reading that produces the gray disc.)

**Breakdown mode → overlap breaks the math.** Axis 3 (correctly) chooses
*overlapping* groups: a `*.log` inside `node_modules` counts in both. Feed
those group totals to the donut against the scan total and the fractions
**sum above 1**. `build_slices` computes the trailing rest as
`rest += (1.0 - accounted).max(0.0)` (wheel.rs:292): once `accounted > 1`,
the "everything else" wedge is clamped to **zero**, and `rasterize` then
silently renormalizes by the fraction sum (wheel.rs:65, boundaries = acc /
total), shrinking every category wedge by an arbitrary factor. The result is
a donut with no uncategorized slice and every proportion distorted — a
wrong-looking picture of a right number. Even *without* overshoot, the gray
rest is now "uncategorized + every sub-2% category + unranked," i.e. exactly
the "Other" bucket the dossier mocks qdirstat for (prior-art §, "much lands
in Other").

The identity-color plumbing itself is *fine* for ≤ 8 categories: `assign_identity`
hands out ranks 0..N and `Theme::identity` indexes `rank % IDENTITY_LEN` with
`IDENTITY_LEN = 9` (theme.rs:55, :269) — eight category slices get eight
distinct colors, no wrap collision. So the machinery *tolerates* category
slices in the "won't panic / won't mis-color" sense the task asked about. It
does not *render them honestly*.

Amendment: pick and document the donut's denominator per mode; for breakdown,
either cap wedges so the sum ≤ 1 and reserve a true "uncategorized" wedge
computed as `total − bytes-in-any-group` (which, because of overlap, is *not*
`total − Σgroups`), or drop the donut in these modes (decision-2 "hide like
zen"). Do not ship "sum a set of deliberately-overlapping totals into a pie."

### SERIOUS — `[patterns]` collides with `config::parse`'s all-or-nothing fallback [3]

`parse()` returns `FileConfig::default()` on **any** `toml::from_str` error
(config.rs:89-95) — every key reset, one `warn!`, scan continues. Today that
is benign (three scalar keys). The dossier bolts a `[patterns]` table onto
the same struct and promises "invalid → warn, non-fatal, keep going"
(axis 3, and the module's own honesty stance). Those two do not compose:

- If `[patterns]` deserializes as `BTreeMap<String, String>` (label → glob)
  and the user writes the very thing the dossier floats in decision 5 —
  `presets = false` under `[patterns]` — that bool in a string-map is a serde
  type error, so **the whole file** falls back to default and the user
  silently loses their `theme`, `color`, and `no_motion` too, over a patterns
  typo. One malformed pattern entry nukes unrelated config.
- The unknown-key detector (`#[serde(flatten)] unknown`, config.rs:44-46,
  :96-103) fires on any top-level key not named in `RawConfig`. Adding
  `[patterns]` therefore *requires* a `patterns` field on `RawConfig`, or
  every launch warns "unrecognized key(s): patterns." The dossier presents
  the section as drop-in and never says this — it is the one config-schema
  change it hand-waves.

Amendment: parse patterns in a way that isolates their failures from the
scalar keys (deserialize the scalars first, then validate patterns per-entry,
skipping and warning on bad ones), and state that `RawConfig` grows a
`patterns` field so the unknown-key path stays quiet.

### ANNOYING — the interned-name glob memo is oversold precisely on the trees it's pitched against [4]

Grounding leans on "match a glob against every *unique name once* and memoize
in a bitset… ≤ 1 bit per unique name (≪ 1 MB)." Two problems, both worst on
the trees `tree.rs`'s own module docs flag as pathological (Maildir,
unique-name-heavy — tree.rs:16-23):

- **Zero dedup work exactly where names don't repeat.** A git object store
  (`.git/objects/ab/<38 hex>`) or a Maildir has essentially *all-unique*
  basenames, so "evaluate each unique name once" ≈ "evaluate each node once" —
  the memo saves nothing over just testing inline. Its payoff is real only on
  repetitive trees (`package.json` × millions), the opposite workload.
- **"≪ 1 MB" drops the pattern factor.** It is one bitset *per pattern*. At
  the dossier's own 64-pattern cap over a 10 M-unique-name tree that is
  64 × 10 M bits = **80 MB** transient, not ≪ 1 MB. Even a modest 16 patterns
  × 2 M names ≈ 4 MB. The figure is only true for few patterns *and* few
  unique names.

It's transient and never fatal, but the "cheap by construction" framing and
the "≪ 1 MB" number are both wrong on the documented worst case. State the
real bound (`patterns × unique_names` bits) and note the memo helps only
repetitive-name trees.

### ANNOYING — the top-1000 cap is neither announced nor deterministic [5]

Axis 2 fixes N at a compile-time 1000 via a min-heap. Two honesty/stability
gaps the dossier doesn't address (the task's "1000 honesty" and "ties at the
cutoff, stability across recomputes"):

- **No truncation indicator.** Nothing in the dossier shows "top 1000 of
  9,412,338 files." A silent cap on a tool that elsewhere counts unreadable
  bytes to stay honest is off-brand; the view must say the list is truncated.
- **No tiebreaker → nondeterministic membership and order.** With thousands
  of equal-`disk` files at the heap boundary (sparse files, 4 KiB stubs,
  zero-length logs), *which* 1000 appear and in what order depends on heap
  displacement order, and shifts after every deletion recompute. `Row`/dir
  ordering elsewhere breaks ties deterministically (e.g.
  `top_dirs_by_disk` ties by `d.index()`, scan.rs:624; the dump sorts
  siblings by raw bytes). The flat heap needs the same — tie by node id or
  path — or the list reshuffles under the user between recomputes.

### COSMETIC — the simple `*`/`?` matcher silently mis-reads real glob syntax [6]

Axis 3 starts without `globset`, with a "~40-line two-pointer `*`/`?`"
matcher, and frames the only risk as "if the session wants character
classes." The sharper edge is the *opposite*: with that matcher there are no
*invalid* globs — every string is valid as a literal — so `*.{log,tmp}`,
`core.[0-9]`, `[abc]` are matched **literally**, fire on nothing, and warn
nothing. A user writes a plausible pattern, it silently never matches, and
the config's "non-fatal, discoverable" promise doesn't cover "syntactically
accepted, semantically inert." Worth one README line: only `*` and `?` are
special; braces and classes are literal in phase 1.

### COSMETIC — `-o -` (dump-to-stdout) summary gate unstated (attack-A already taught this) [7]

Axis 5 adds a "Top N files" line to the `--no-ui` summary and, like the
freeable dossier before it (freeable-attack-a [5]), never mentions
`--output -`. Here the blast radius is small *by luck of layout*: the entire
summary block already lives inside the `!dump_to_stdout` else (main.rs:628-666,
right after the existing "Top N directories" loop), so a top-files section
added in the obvious place is gated for free and cannot corrupt the zstd
stream. But the dossier repeats the omission it should have inherited as a
lesson — say "inside the `!dump_to_stdout` branch, beside the top-dirs list."

### COSMETIC — `--top` now means two different things at two different limits [8]

Reusing `--top` (main.rs:76, env `TOP`, default 20 — it *does* exist; the
dossier did **not** invent CLI surface) for a top-*files* summary section
means one flag governs both the existing top-*dirs* list and the new
top-files list, while the interactive view uses a *fixed* 1000 cap unaffected
by `--top`. `--top 20` prints 20 dirs and 20 files; the TUI shows 1000. One
knob, two meanings, and a third limit it doesn't reach. Defensible, but name
it in the docs so `--top 5000` (which the 1000-cap heap can't serve
interactively anyway) isn't a surprise.

### COSMETIC — "excluded mounts contribute nothing" is not literally true [9]

Axis 2: "Excluded mounts / kernfs: not scanned, contribute nothing, cannot
appear. (They have no `DirMeta`; the fold never descends them.)" An excluded
mount's **own inode size** *is* folded into its parent's aggregate at scan /
import time (e.g. ncdu.rs:287-289 adds `asize`/`dsize` to the parent's sums;
the scanner does likewise), so it is counted once as the mount-point
directory. Two edge consequences: a mount that itself matches a *dir* pattern
(a `node_modules` that is a bind-mount, say) has no `DirMeta` to sum, so it is
silently dropped from that group; and its own bytes sit in the ancestor
totals regardless. Negligible in bytes, but "contribute nothing / cannot
appear" overstates it — for the files-only flat list it's correct (they're
dirs, skipped), for dir-pattern groups it isn't.

### COSMETIC — breakdown's inapplicable sort keys are under-enumerated [10]

Axis 1 says flat mode flashes `c`/`e` as "not applicable" and breakdown maps
`d/a` = total, `c` = count, `n` = label. That leaves `m` (mtime) and `e`
(errors) — real keys today (keymap.rs:93-108) — undefined in breakdown, where
a *group* has neither an mtime nor an error count. They need the same
"not applicable" flash; the dossier lists only the flat-mode pair.

## What survived the attack (genuinely)

- **Topological `DirId` order — the load-bearing dedup claim — holds on every
  path.** `add_dir` takes the parent's `DirId` as an argument (tree.rs:619),
  a hard data dependency that forces the parent to be interned into the dir
  arena *before* any child, so parent index < child index always. Verified
  for: the live scan (a child section cannot integrate before the parent dir
  that owns the `DirId` it references); **ncdu import** (DFS descent —
  `open_dir` calls `add_dir(node, Some(parent.dir), …)` at ncdu.rs:386, and
  the fixture's own assertion is DFS-preorder, ncdu.rs:914-924); and **after
  deletions** (`apply_removal` only tombstones and subtracts up-chain,
  tree.rs:491-550 — the arena never reorders or compacts, and because a
  removed dir tombstones its *entire* subtree, no *live* dir ever has a
  tombstoned ancestor, so the single forward mask pass never reads a missing
  or stale parent mask). I could not construct a breaking case. The
  `mask[d] = mask[parent(d)] | own_match` forward pass is sound.

- **Hardlink re-attribution has already landed before any fold could run —
  the task's async worry is unfounded.** `finalize_hardlinks()`
  (scan.rs:541) runs **synchronously** inside `finish_scan` (ui.rs:352),
  which is called at ui.rs:468 *before* `phase = Phase::Done` at ui.rs:493.
  Only the freeable `/proc` sweep is spawned off-thread (ui.rs:490); the
  arena's hardlink attribution is done on the UI thread before the frozen
  arena serves its first post-scan view. The fold, triggered on the first
  `t`/`b` in `Phase::Done`, always sees canonical attribution — a
  `HARDLINK_EXTRA`-skipping sum equals the root aggregate by construction, as
  claimed. (The Grounding's "post-scan re-attribution has already run by the
  time the arena freezes" is correct; only the task's parenthetical "async"
  hypothesis was wrong.)

- **The outermost-match arithmetic is consistent through deletion** — *given
  the epoch recompute of [1]*. `DirMeta` subtree aggregates are subtracted
  up-chain on removal (`apply_negative_delta`, tree.rs:556) and `children()`
  filters tombstones at the single filter point (tree.rs:397-403), so a
  recomputed per-node fold and `Σ outermost DirMeta aggregates` stay equal.
  The cross-check invariant test the dossier proposes is well-founded.

- **Marks are genuinely view-independent.** `MarkedEntry` captures node +
  path at mark time; the basket, review modal and confirm flow key on nothing
  view-specific, so marking in flat mode and deleting works with no new
  mechanism — as claimed.

- **The key/modal/Esc claims are exactly right.** `t` and `b` are unused
  (keymap.rs has no such bindings); the modal ladder is confirm > review >
  freeable > cheatsheet (ui.rs:646-708); normal-mode `Esc` quits today
  (ui.rs:714). The one behavior change (Esc returns to tree in a non-tree
  mode) is honestly flagged as decision 1. Zen composes: `z` already hides
  "cards, gauge or wheel" (keymap.rs:129-132), so `t`/`b` + zen is a bare
  table of flat/breakdown rows — clean, no conflict.

- **No interactive dump-viewer exists** (dispatch is scan / diff / import;
  the latter two are non-interactive), so the fold only ever runs over a
  live-scanned frozen arena — the same clean bill freeable-attack-a [8] gave
  the sweep trigger. If a `.cmbt`-in-TUI mode ever lands, the fold is a pure
  `fn(&Tree)` and inherits nothing dangerous; note it and move on.

- **Isolation holds.** A new `camembert-core/src/flat.rs` computing a pure
  `fn(&Tree, &[Pattern], top_n) -> FlatReport` needs nothing from the dump
  writer, diff, or snapshot path; the only UI-side additions are a `ViewMode`
  enum, a `mutation_epoch` counter, and two render paths. The blast radius is
  as small as advertised.

## Verdict: SURVIVABLE WITH AMENDMENTS

Not killable — the engine is the honest one, the topological and hardlink
claims are true in the code (not merely plausible), and the phase-boundary
(post-scan only, marks-style flash during scan) is the right identity call.
But it does **not** survive as written: the donut headline is a rendering the
widget can't deliver under the dossier's own overlap choice ([2]), the cache
lifecycle prints a deleted file as present when you delete in-mode ([1]), and
the config story silently eats unrelated settings on a patterns typo ([3]).

Required amendments (severity order):

1. **Recompute the report on a render-time epoch mismatch**, not "on first
   `t`/`b`"; test delete-while-in-flat-mode. [1]
2. **Fix or drop the mode donut**: define the per-mode denominator; for
   breakdown, reserve a true uncategorized wedge (`total − bytes-in-any-group`,
   not `total − Σgroups`) and cap wedges so the sum ≤ 1, or hide the wheel in
   these modes. Do not pie-chart deliberately-overlapping totals. [2]
3. **Isolate `[patterns]` failures from the scalar keys** and state that
   `RawConfig` gains a `patterns` field (else every launch warns). [3]
4. Re-price the glob memo: `patterns × unique_names` bits, and say it helps
   only repetitive-name trees (it's dead weight on git/Maildir). [4]
5. Add a truncation indicator and a deterministic cutoff tiebreaker to the
   top-1000 list. [5]
6. Doc the matcher's literal treatment of `{}`/`[]` ([6]); place the summary
   line inside `!dump_to_stdout` and say so ([7]); name the `--top`
   dual-meaning ([8]); soften "excluded contribute nothing" ([9]); flash
   `m`/`e` in breakdown too ([10]).

With 1–3 done, the surface catches up to the engine and the feature is the
honest, on-thesis differentiator it claims to be. Without them, its flagship
donut lies about proportions and its flagship view lists files it just
deleted — on the one tool built to never do that.
