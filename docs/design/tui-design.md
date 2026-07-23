# TUI visual design (co-design sessions, 2026-07-22)

Settled direction for the interactive UI; supersedes the plain-table MVP
look. Reopening a decision requires a new element. The reasoning trail
(mockups, explored-and-rejected directions) lives in the session history;
this records the outcome.

## Direction: "Dashboard cockpit"

A mix of the explored targets 1A (table + side panel) and 1C (cockpit),
with the wheel as identity centerpiece:

- **Header**: `▞ camembert` signature glyph, **clickable breadcrumb**
  (path segments jump), scan spinner.
- **Metric cards row**: total real size · entries (with a braille
  **sparkline** of scan throughput) · errors (**clickable** → sort by
  errors) · hardlinked inodes. Rounded borders, one accent color each.
- **Disk gauge**: one slim line under the cards — `statvfs` capacity,
  "% occupied, this scan covers N% of occupied". Future home of quota
  display (wave 3).
- **Main split**: table left (size, proportion bar, name; bar color =
  entry's identity color), **donut camembert** right — the current
  directory's children as slices, rendered in **half-blocks** (`▀▄`,
  ~square pixels), **sextants** (2×3, Unicode 13) on capable terminals.
  Slice color == bar color == name color (the eye links data across
  panels). Slices grow live during the scan. A hover/selection card under
  the table shows mtime ("modified 3 min ago"), item count, % of parent,
  errors.
- **Deletion basket**: marks form a persistent basket strip above the
  footer (count + size); `v` opens a review list. Confirm flow unchanged
  (`D`, `y`).
- **Toasts**: top-right, transient (dump written, deletion done, scan
  finished).
- **Footer**: key hints, D3 hardlink provisional note, degraded-cadence
  note.
- **`?`**: floating key-map cheatsheet. **`Ctrl-K`**: command palette —
  reserved; ships with the filter/query language (wave 3) as its UI.

## Interaction

- Mouse everywhere: click rows and pie slices to navigate, wheel scrolls,
  breadcrumb and cards clickable. Keyboard stays complete (current map).
- Micro-animations at 30 fps (existing loop): eased bar fills, donut
  morph on navigation, no animation longer than ~150 ms; `--no-motion`
  (env NO_MOTION) disables.
- Responsive: below ~100 columns the wheel collapses to a header
  mini-donut; `z` zen mode = table only.

## Color and capabilities

- **Truecolor by default** with a Tokyo-Night-family palette; palette is
  identity (amber `#e0af68` = signature accent).
- Capability ladder, detected at startup: truecolor → 256 → ANSI-16 →
  mono; sextants → half-blocks → ASCII bars (`#`). `NO_COLOR` honored;
  `--color auto/always/never` (env COLOR).
- **Light terminals**: OSC 11 background query; light palette variant.
- **Config file**: `camembert.toml` (XDG config dir) + `--theme`
  (env THEME): `tokyo-night` (default), `light`, `high-contrast`.
- Errors always the same coral family; excluded mounts dim italic.

## Design reservations (no code now, habitat ready)

1. **Diff skin** (wave 2): opened with two dumps, metric cards show
   deltas ("+2.3 GiB since yesterday"), bars become signed growth bars,
   the donut shows growth share.
2. **Freeable segment** (wave 2): each bar can render its actually-
   freeable fraction as a bright segment against the dim total — the UI
   home of the "libérable ≠ taille" thesis.
3. **Sunburst upgrade**: the donut can grow a second ring
   (grandchildren, shaded from the parent's color) with click-to-zoom —
   explored, liked, deferred until after the single-ring donut ships.
4. **Kitty-graphics opt-in**: pixel-perfect wheel when the terminal
   advertises support — opt-in only (the handoff's exclusion targeted
   *mandatory* graphics protocols).

## Implementation slices (in order)

1. Layout skeleton: header + metric cards + gauge + split main + footer;
   truecolor palette + capability ladder + NO_COLOR.
2. Donut renderer (half-blocks, then sextants) + identity colors shared
   table/wheel + live slice growth.
3. Mouse support + clickable breadcrumb/cards + hover card.
4. Basket strip + `v` review; toasts; `?` cheatsheet.
5. Animations + `--no-motion`; responsive collapse + zen mode.
6. Themes + config file + OSC 11 light detection.

## README hero image

`docs/images/tui.png` is a static render of this design (not a live
terminal capture — no real tree matches the illustrative numbers), built
as an HTML/CSS mockup using the exact palette from `ui/theme.rs` (rank
order: amber → blue → green → mauve → sky) and the same half-block donut
math as `ui/wheel.rs`, screenshotted with headless Chromium at 2x scale
and autocropped. Regenerate it whenever this design doc changes in a way
that would make the image misleading (new panels, changed palette,
changed layout) — it does not need to track every code change.
