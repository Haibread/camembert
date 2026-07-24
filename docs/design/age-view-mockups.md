# Big-and-cold surface — UX mockups

**Status: exploration, not a decision.** Companion to a parallel dossier
prototyping the size×age *scoring formula* on real data. This document
only covers the *surface*: where "what is big and cold?" lives in the TUI
and what it looks like. The score itself is treated as a given black box
— a single comparable number per file, direction unspecified (higher =
"more worth your attention"), computed by someone else's work. Nothing
here should be read as a proposal for how that number is computed.

Mockups approximate a 120×40 terminal. Column alignment is illustrative,
not pixel-exact — the same caveat [tui-design.md](tui-design.md) gives
its own README hero image ("not a live terminal capture").

## What this displaces, and does the thesis agree

Per the dossier workflow: before drafting options, what does this work
compete with?

- **Screen real estate and key-space**, against the backlog in
  [HANDOFF.md](../../HANDOFF.md) (freeable phase 2, quotas, cleanup
  recipes, …) — an opportunity-cost question for whoever sequences work,
  not something this document resolves.
- **The query language**, which already answers a *restricted* version of
  this question today: `>1G older:1y` filters to a big-and-cold set, and
  a saved query (`[queries]`) can name it. Any dedicated surface has to
  earn its keep against "just type the query" — argued per-option below,
  and revisited in the recommendation.
- Nothing here competes with the thesis. The README's third differentiator
  bullet is *exactly* "what is big and cold?" — building a surface for it
  fulfills a promise already made, it doesn't chase a tangent.

## Shared precedent this design leans on

- **D3, [flat-view-decisions.md](flat-view-decisions.md)**: `t`/`b` are
  in-place table modes — cards, gauge, basket, footer stay; only the table
  and the donut change; contextual Esc leaves the mode; the donut mirrors
  whichever mode is active, top-N with a merged "others" slice for flat
  data. `t`'s flat list is **regular files only, canonical hardlink owner
  only** — directories are structurally excluded.
- **[tui-design.md](tui-design.md)**: identity color (bar color == slice
  color == name color) is an already-spent visual channel; dim-italic is
  already spent on excluded mounts; coral is already spent on errors. Any
  new visual cue for "cold" has to find an unclaimed channel or reuse the
  score's own bar, not repurpose one of these.
- **[query-decisions.md](query-decisions.md) D2**: the filter is
  deliberately **post-scan only** — a typed predicate changes too fast to
  accumulate incrementally, and the README already carries the mtime
  disclosure prose (`older:` bullet): *"this tool never reads atime; a
  `relatime`-mounted filesystem's own atime is unreliable anyway."* That
  sentence exists in docs today but nowhere in the live UI — worth fixing
  regardless of which option below ships (see "Disclosure" below).
- **A fact that matters more than it looks**: `Row.mtime` / `Node.mtime`
  is *"the entry's own"* (`camembert-core/src/view.rs:89`,
  `tree.rs:214`) — for a **directory** row this is the directory inode's
  own mtime (bumped by *any* direct child add/remove/rename), not a
  recursive "oldest/newest file inside" aggregate. Sorting directories by
  raw mtime already exists (`m`) and already has this caveat; any new
  surface that ranks *directories* by age inherits it. Surfaces limited to
  *files* (like `t`) never hit it, because a file's own mtime is a
  meaningful, un-confounded signal.

## How age reads at a glance (cross-cutting answer)

Applies to every option below, so stated once:

- **Primary: a relative string**, using the *same units the query
  language already teaches* (`h`/`d`/`w`/`mo`/`y`) — `4y 2mo`, `18d`,
  `3h`. A user who has typed `older:6mo` once already knows how to read
  this column; no second vocabulary to learn.
- **Secondary: absolute date on demand.** The existing selection/hover
  card already shows relative mtime ("modified 3 min ago" per
  tui-design.md); it gains the absolute timestamp next to it
  (`modified 3 min ago · 2022-05-01 09:14 UTC`) — a small, low-risk
  extension of a panel that already exists for exactly this purpose, not
  a new UI element.
- **The bar/heat cue is reserved for the *score* column, not the age
  column.** Age and score are two different numbers; encoding both as
  gradient bars would make the eye guess which bar means what. Score gets
  the bar (it's the one number the view exists to rank by); age stays
  plain text (it's the number you read to sanity-check the score, e.g.
  "oh, it's cold because it's 8 years old, not because the clock is
  wrong").

## Disclosure: is "cold = mtime, not atime" ever said in the UI?

Today: **no.** The sentence exists in the README's `older:` grammar row
and nowhere else — a user who never reads the query section of the README
can misread "cold" as "a file read daily but written years ago" without
ever being told otherwise. Given the brand-honesty requirement, every
option below carries a **short, persistent, always-visible** disclosure
next to wherever "cold"/"age"/"score" appears — not buried in `?`, not a
one-time toast that scrolls away. The exact wording proposed (short form
for the UI, long form for docs) is in each option's mockup and repeated in
the comparison table's "disclosure" row.

---

## Option A — dedicated mode (`o`, sibling of `t`/`b`)

A third in-place table mode: a flat, globally-ranked list of files by
score, structurally identical in spirit to `t` (files only, canonical
hardlink owner only, truncated past a cap, live path-in vs
path-widens-later distinction does not apply since this is necessarily
post-scan — see below).

```
▞ camembert   home › theo › projects › camembert                                                 ● scan complete
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 real size: 142.8 GiB     entries: 1,284,032 ▂▄▆█▆▄▂     errors: 3     hardlinked: 211 inodes
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 disk [██████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░]  71% occupied — this scan covers 96% of it
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 COLD · big & cold, whole scan, files only       cold = time since last WRITE (mtime) — atime is never read · ⓘ ?
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  #   score        disk        age       path                                            ┃  ● full-2022      31%
  1   ██████████  38.2 GiB    4y 2mo    /var/backups/db/full-2022-05-01.sql.gz           ┃  ● ubuntu-18.04   18%
  2   █████████░  22.7 GiB    6y 8mo    /home/theo/iso/ubuntu-18.04-desktop-amd64.iso    ┃  ● node_modules…  11%
  3   ████████░░  9.4 GiB    2y 11mo   /srv/build/target/debug/incremental/…-3fa1.o     ┃  ● backups.tar     9%
  4   ███████░░░  6.1 GiB    5y 1mo    /home/theo/Videos/family-2021-summer.mkv         ┃  ● others         31%
  5   ██████░░░░  4.8 GiB    3y 4mo    /home/theo/.cache/pip/http-v2/3f/a9/…            ┃ ╭──────────────╮
  6   █████░░░░░  3.2 GiB    1y 9mo    /var/log/journal/…/system@0007cf….journal        ┃ │    ╭────╮     │
  7   █████░░░░░  2.9 GiB    7y 0mo    /home/theo/old-laptop-backup.tar.gz    ⛓         ┃ │   ╱      ╲    │
  8   ████░░░░░░  2.6 GiB    2y 0mo    /opt/legacy-app/data/archive-2024Q1.dat           ┃ │  │  ●●●●  │   │
  9   ████░░░░░░  2.1 GiB    4y 6mo    /home/theo/Downloads/old-iso/debian-9.iso         ┃ │   ╲      ╱    │
 10   ███░░░░░░░  1.8 GiB    9mo       /home/theo/projects/dead/target/release/lib.rlib  ┃ │    ╰────╯     │
 11   ███░░░░░░░  1.6 GiB    3y 2mo    /srv/media/podcasts/2022/ep-014.mp3               ┃ ╰──────────────╯
 12   ██░░░░░░░░  1.4 GiB    11mo      /home/theo/.local/share/Trash/files/dump.sql      ┃
 13   ██░░░░░░░░  1.2 GiB    6y 3mo    /mnt/backup/old-photos-2019.zip                   ┃
 14   ██░░░░░░░░  980 MiB   2y 5mo    /var/cache/apt/archives/old-kernel.deb            ┃
 15   █░░░░░░░░░  760 MiB   1y 1mo    /home/theo/.cargo/registry/cache/…/old-crate.crate┃
 …    1000 of 84,213 files shown (flat_cap); widen with `Ctrl-K` / `>1G older:1y`        ┃
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 no entries marked
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 ↑↓ move · ⏎ open in tree · Space mark · d/a/n/m/s sort (s=score, default) · o back to tree · ? cheatsheet
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
```

**What the user sees**: a single flat, ranked list — press one key from
anywhere, get the answer, no navigation. Score is the primary sort (bar +
number), disk size and age both shown as plain columns so the score is
never a black box *in the UI* even though its formula is one in this
document. `⛓` for hardlinked files, exactly like `t`.

**Discovery**: `o` joins `SIMPLE` (stateless toggle, same shape as
`toggle_flat_top`/`toggle_breakdown`) — cheatsheet, footer, `--help`
equivalent, and README's Keys table all gain a row, matching the existing
"documented in the same change" rule.

**Composition**:
- *Filters*: composes exactly like `t`/`b` (D4, query-decisions.md) — the
  ranked list is computed over the active match set when a filter is
  applied, same "match set, not whole scan" rule.
- *Marking*: `Space` marks a row into the same basket `t` uses — real
  node ids, nothing special-cased, `⏎` jumps to the containing directory
  in tree view exactly like `t`'s D3 behavior.
- *Donut*: mirrors mode data, same "top-N + merged others" idea `t`
  already established. Slice color stays **identity-order** (amber →
  blue → green → mauve → sky), *not* recolored by score — reusing the
  bar for score inside the table avoids inventing a second color meaning
  for the wheel, and keeps the existing "color links table row to slice"
  rule intact instead of overloading it with a second signal.

**Cost**: the most expensive option here. Needs a new `ViewMode` variant,
a new mode-aware sort-key subset (score, plus disk/age as secondary
sorts), and — this is the part worth being honest about — its own
**correctly-scoped top-N selection**. `t`'s existing top-N is an
incrementally-maintained min-heap **keyed on `st_blocks`** (D2,
flat-view-decisions.md); a client-side re-sort of *that* heap by score
would silently miss any file that never made the size-keyed top-N in the
first place (a merely-average-sized but extremely old file could easily
be the single best answer to "big and cold" without ever entering a
top-1000-by-raw-size list). Getting Option A right therefore means either
a **second, score-keyed top-N fold**, or — the simpler, recommended
shape — **computing it post-scan only**, as a one-shot fold over the
frozen arena (reusing the exact-fold machinery `camembert-core/src/flat.rs`
already established, D6), the same choice the query language made for
the same reason (D2, query-decisions.md: a moving predicate/target
doesn't accumulate cheaply). During a scan, `o` would show "cold view
available once the scan completes" — the *filter's* precedent, not
`t`/`b`'s live-provisional one, and that's a deliberate, defensible
difference to flag rather than copy blindly.

**Mono/ASCII degradation**: score bar → `#` characters (`##########`),
color-coded score (amber intensity) → a plain number when `NO_COLOR` is
set; nothing here needs sextants or half-blocks, so it degrades cleanly
all the way to ANSI-16/mono without losing information, only styling.

**Below 100 columns**: path column truncates first (same abbreviation
rule as the breadcrumb/flat-mode path); the donut collapses to the header
mini-donut per the existing responsive rule, freeing the whole width for
the ranked table; score bar shrinks to 6 cells before dropping to a bare
number.

**Failure mode — freshly cloned/extracted tree** (all mtimes ≈ checkout
time):

```
 COLD · big & cold, whole scan, files only       cold = time since last WRITE (mtime) — atime is never read · ⓘ ?
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 ⚠ mtimes are nearly identical across this scan (spread: 6 minutes) — age carries ~no signal here; this list is
   effectively sorted by size alone. A `git clone` / archive extraction stamps every file at checkout time.
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  #   score        disk        age       path                                            ┃  ● target/…       44%
  1   ██████████  4.1 GiB    6 min      /home/theo/projects/camembert/target/debug/…      ┃  ● node_modules…  22%
  2   █████████░  2.8 GiB    6 min      /home/theo/projects/camembert/node_modules/…      ┃  ● others         34%
  3   ████████░░  1.9 GiB    6 min      /home/theo/projects/camembert/target/release/…    ┃
```

The honest response is a **detection banner**, not a silent degrade to a
size sort that *looks* like a cold-ranking: if the scanned set's mtime
spread (e.g. IQR, or "% sharing the same calendar day") falls under a
threshold, replace the disclosure line with the warning above. This
mirrors the project's existing house style for exactly this kind of
caveat (coverage-percentage wording, the dir-inode filter residual, the
hardlink provisional badge) rather than inventing a new one.

---

## Option B — sort key on the existing tree (`s` = score, cheapest)

No new mode. `SortKey` gains `Score`; the tree table gains an `age`
column that appears automatically whenever the active sort is `Mtime` or
`Score` (same idea as `p`'s apparent-size toggle, but auto-shown instead
of a separate key — one fewer key to remember).

```
▞ camembert   home › theo › var › backups                                                        ● scan complete
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 real size: 142.8 GiB     entries: 1,284,032 ▂▄▆█▆▄▂     errors: 3     hardlinked: 211 inodes
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 disk [██████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░]  71% occupied — this scan covers 96% of it
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 sort: score ▾ (age shown; cold = mtime, atime never read · ?)                     /home/theo/var/backups
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  score        disk        age       name                                                ┃  ● db              54%
  ██████████  38.2 GiB    4y 2mo    db/                                                   ┃  ● old-photos-20…  16%
  ████████░░  9.4 GiB    2y 11mo   incremental-snapshots/                                 ┃  ● weekly           9%
  ██████░░░░  4.1 GiB    9mo       old-photos-2019.zip                                    ┃  ● daily            6%
  █████░░░░░  2.0 GiB    1y 4mo    weekly/                                                ┃  ● others          15%
  ███░░░░░░░  640 MiB   3mo       daily/                                                  ┃ ╭──────────────╮
  █░░░░░░░░░  40 MiB    2h        .lock                                                   ┃ │  (same donut  │
                                                                                            ┃ │   as tree     │
                                                                                            ┃ │   view today) │
                                                                                            ┃ ╰──────────────╯
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 no entries marked
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 ↑↓ move · ⏎ open · ⌫ up · d/a/n/m/c/e/s sort (s=score) · p toggle apparent · Space mark · ? cheatsheet
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
```

**What the user sees**: the *same tree they're already in*, resorted —
no mode switch, no new screen to learn. `db/` sorts to the top because
its subtree total is large **and** its own directory mtime is old.

**Discovery**: one more letter in the existing sort-key row (`d a n m c e
s`), documented exactly like the other five in `EXTRA`/cheatsheet/README.

**Composition**: this is the strongest point in Option B's favor —
*everything already works*, because sorting is orthogonal to every other
feature. Filters already re-sort by any key on the match set; marks
already work on tree rows; the donut already mirrors the current
directory's children and just reorders/recolors identically to any other
sort. No engine change, no new top-N selection, no post-scan-only
restriction — the comparator runs over whatever rows are already loaded
for the visible directory.

**Cost**: lowest of the four options. A comparator function
(`ra.disk_or_score.cmp(&rb...)`) and one column. No new `ViewMode`, no
new heap, no new "provisional vs final" question (the tree view already
freezes marks post-scan and updates live pre-scan the same way every
other sort key does).

**Mono/ASCII degradation**: identical to Option A — a plain number column
needs no color/graphics capability at all.

**Below 100 columns**: the age column is the first to drop (same
priority as the apparent-size column today), score becomes name-only sort
order with no visible bar — the table degrades to "just a different tree
order," same as any other sort key would.

**The honest failure mode this option has that Option A does not**:
because tree rows include **directories**, and a directory's mtime is
*its own inode's*, not a recursive aggregate (`Row.mtime` doc, `view.rs`),
sorting by score at the directory level inherits a real, silent trap: a
directory holding a 500 GiB file untouched for eight years can show up as
"recently touched" (and therefore *not* cold) simply because someone
created an unrelated 40 KiB lockfile inside it yesterday. `db/` above
looks plausible only because its own mtime happens to track its biggest
file; that alignment is not guaranteed. This is the same caveat the
existing `m` sort already carries today, silently — Option B doesn't
introduce a new problem, but it also doesn't fix the pre-existing one,
and a *combined* score column makes the gap more visible (size sorts
don't imply an age claim; a "cold" score does). The disclosure line above
the table should say so plainly, not just "cold = mtime."

**Freshly cloned tree**: same detection banner as Option A, shown in the
sort-status line instead of a mode banner (`⚠ mtimes nearly identical
(spread 6 min) — score ≈ size order here`).

---

## Option C — ambient annotation (age on every row, no new mode/key)

Every row, in every mode, grows a small age cue — no toggle, no
discovery cost, always on.

```
▞ camembert   home › theo › var › backups                                                        ● scan complete
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 real size: 142.8 GiB     entries: 1,284,032 ▂▄▆█▆▄▂     errors: 3     hardlinked: 211 inodes
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 disk [██████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░]  71% occupied — this scan covers 96% of it
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 /home/theo/var/backups                                                    (age shown per row: mtime, not atime)
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  disk        %      name                                          ┃  ● db              54%
  38.2 GiB   27%    db/                              ❄ 4y 2mo      ┃  ● old-photos-20…  16%
   9.4 GiB   7%     incremental-snapshots/           ❄ 2y 11mo     ┃  ● weekly           9%
   4.1 GiB   3%     old-photos-2019.zip              ❄ 9mo         ┃  ● daily            6%
   2.0 GiB   1%     weekly/                                        ┃  ● others          15%
     640 MiB <1%    daily/                                         ┃ ╭──────────────╮
      40 MiB <1%    .lock                                          ┃ │  (same donut  │
                                                                     ┃ │   as today)   │
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 no entries marked
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 ↑↓ move · ⏎ open · ⌫ up · d/a/n/m/c/e sort · p toggle apparent · Space mark · ? cheatsheet · ❄ = old (mtime, not atime)
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
```

**What the user sees**: a `❄`-plus-age marker appended after rows old
enough to matter (a threshold, e.g. > 1 year), everywhere, all the time —
no key to remember, no mode to enter. `.lock`/`daily/` show nothing
because they're recent.

**Discovery**: free — the footer's legend line explains the glyph once;
nothing else to document as a *feature* since there's no key bound to it
(only a config toggle to turn it off, if wanted).

**Composition**: this is where the option runs into the design's already
-spent visual vocabulary. Bar/slice/name color is **identity** (rank
order, amber→blue→…) — already spoken for. Dim-italic is **excluded
mounts**. Coral is **errors**. A `❄` glyph is the only channel left that
doesn't collide, and it can't reach the donut at all without inventing a
second visual dimension on the wheel (the design's own reserved-but-
deferred "sunburst second ring" is the closest fit, and reopening that
early is exactly the kind of premature investment the reservations list
warns against). So: table gets an annotation, the donut stays silent on
age — a real, not cosmetic, limitation for a feature whose whole point is
"visible at a glance," since the glance historically includes the wheel.

**Cost**: cheap to build (a glyph + a threshold), but **expensive in
attention**: this is the only option that changes *every* row of *every*
existing view, permanently, rather than being opt-in like `t`/`b`/a sort
key. On a tree with many old files (which, per the failure mode below, is
most real-world trees), the marker becomes wallpaper — present on nearly
every row, conveying nothing, the opposite of "visible at a glance."

**Mono/ASCII degradation**: `❄` → `*` or `(old)` suffix; trivial, no
capability dependency.

**Below 100 columns**: the marker is the first thing dropped (it's the
least essential character on an already-tight row), silently disabling
the entire feature below the threshold — a bad look for a brand-honesty
feature to be the one thing that vanishes first under space pressure.

**Failure mode — fresh clone**: the opposite problem from A/B: instead of
a false top-of-the-ranking claim, *every* row would either show a
near-identical tiny age (useless, all "6 min") or — if the threshold is
tuned to skip that — the glyph simply never appears, silently, and the
user has no signal that the feature is even active on this tree. There is
no natural place to put the earlier "mtimes are nearly identical" banner,
because there is no dedicated mode/status line to put it on.

---

## Option D — a fourth idea: score-sort *inside* `t` (hybrid)

Instead of a new mode, add `Score` as a legal sort key **within the
existing flat-top-files mode**: `t` still opens the same global flat list
it does today (top-N by disk, live during the scan, provisional badge and
all), and pressing `s` inside it re-sorts *that already-selected list* by
score, gaining an age column the same way Option B's tree does.

```
▞ camembert   home › theo › projects › camembert                                                 ⣾ scanning… 61%
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 real size: 88.4 GiB (provisional)   entries: 812,004 ▂▄▆█▆▄▂     errors: 1     hardlinked: 140 inodes
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 disk [███████████████████████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░]  71% occupied — this scan covers 61% of it
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 FLAT TOP FILES · provisional, still scanning        sorted by score (age: mtime, not atime) · ⚠ see note below
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  #   score        disk        age       path                                            ┃  ● full-2022      31%
  1   ██████████  38.2 GiB    4y 2mo    /var/backups/db/full-2022-05-01.sql.gz           ┃  ● ubuntu-18.04   18%
  2   █████████░  22.7 GiB    6y 8mo    /home/theo/iso/ubuntu-18.04-desktop-amd64.iso    ┃  ● others          51%
  3   ████████░░  9.4 GiB    2y 11mo   /srv/build/target/debug/incremental/…-3fa1.o     ┃
  …   996 more (top 1000 by size, then re-sorted by score — see note)                    ┃
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 ⚠ this re-sorts the top-1000-by-size list, not a true whole-scan ranking by score: a merely-average-sized file
   that is exceptionally old would rank #1 by score but never enter this list in the first place.
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────
```

**What the user sees**: looks identical to `t` today, plus a column and a
sort key — cheapest possible way to expose *something* score-shaped
inside an existing, already-live, already-provisional-during-scan mode.

**Composition**: inherits every one of `t`'s existing behaviors for
free (jump-to-directory, marking, provisional badge, live during scan,
donut mirroring) — no engine change **if** the correctness caveat below
is accepted.

**Cost / the reason this is not the recommendation**: `t`'s top-N is
selected by an incrementally-maintained heap **keyed on `st_blocks`**
(D2, flat-view-decisions.md) — a fixed selection criterion baked into the
scan hot path. Re-sorting the *already-selected* 1000 rows by score is a
client-side operation (cheap, no engine change), but it is **answering a
different, narrower question** than "what is big and cold across the
whole scan" — it's "what is big-and-cold among the 1000 biggest files,"
silently. That's a correctness gap dressed as a feature, and the warning
banner above is the least-dishonest way to ship it without engine changes
— but a banner that has to explain away a wrong answer on every use is a
sign the shape is wrong, not a sign the banner is doing its job. Included
here as the strongest "fourth idea" candidate, and rejected in the
recommendation for exactly this reason.

**Mono/ASCII, below 100 cols, fresh-clone failure mode**: identical to
Option A's (same list machinery), plus the additional caveat above.

---

## Comparison

| | A — dedicated `o` mode | B — tree sort key `s` | C — ambient `❄` | D — score-sort inside `t` |
| --- | --- | --- | --- | --- |
| New key/mode | 1 new key, 1 new mode | 1 new key, 0 new modes | 0 | 0 new modes, 1 sort key inside `t` |
| Scope | whole scan, files only | current directory (files + dirs) | every row, every view | whole scan, **top-1000-by-size only** |
| Global "at a glance"? | yes | no — per directory | no — per row, no ranking | yes, but over a narrower, wrong-shaped set |
| Directory-mtime trap | avoided (files only) | present, inherited from `m` | present (dirs get the glyph too) | avoided (files only) |
| Correct top-N by score | yes (needs its own fold) | n/a (no cap, just a comparator) | n/a | **no** — inherits the size-keyed heap |
| Engine cost | new post-scan fold (or live heap, v2) | none — pure comparator | none | none, but wrong-shaped answer |
| Donut composition | full (identity color kept) | full, free (existing tree donut) | none (age can't reach the wheel) | full (existing flat donut) |
| Screen cost | one more mode (opt-in) | one column, opt-in by sort | permanent, on every row | one column, opt-in by sort |
| Mono/ASCII | clean (numeric + `#` bar) | clean | clean (`*`/`(old)`) | clean |
| < 100 cols | donut collapses, path truncates | age column drops first | glyph drops first (silently) | donut collapses, path truncates |
| Fresh-clone failure mode | explicit banner replaces disclosure | explicit banner in sort-status line | silent (no home for a banner) | explicit banner (plus the size-cap caveat) |
| mtime-not-atime disclosure | persistent mode-header line | persistent sort-status line | footer legend (easy to miss) | persistent mode-header line |
| Query-language overlap | complements (`>1G older:1y` still useful for narrowing before ranking) | complements | complements | complements |

## Recommendation

**Option A**, with two amendments baked in from the start: **files only**
(mirrors `t`'s existing rule, sidesteps the directory-mtime trap
entirely) and **post-scan only** (mirrors the query language's D2
reasoning — a correctly-scoped score-keyed top-N is not free to
maintain incrementally, and "available once the scan completes" is an
honest, already-precedented state to show meanwhile, rather than a
live number computed from the wrong top-N like Option D's). It is the
only option that gives a genuine whole-scan, at-a-glance ranking without
either the directory-mtime trap (Option B) or the wrong-subset trap
(Option D), and it reuses the entire `t`/`b` interaction family — mode
toggle, contextual Esc, donut mirroring, marking, jump-to-directory — so
the *design* risk is low even though the *engine* cost isn't zero.

The strongest argument against this recommendation: **it is also the
most expensive of the four to build, for a formula that is still being
prototyped in a parallel dossier.** Nothing here is falsifiable yet
against real usage — nobody has pressed `o` in anger. Option B costs a
comparator and a column, has zero engine risk, and delivers a real
(if directory-mtime-caveated, if per-directory-scoped) answer today,
reusing 100% of existing filter/mark/sort/donut machinery. A defensible,
cheaper sequencing is: ship B first as an interim while the scoring
formula proves itself and the post-scan fold for A gets designed
properly, then promote to A once there's evidence the global ranking is
worth the extra engine investment — rather than committing to the
expensive shape before knowing the formula is even right.
