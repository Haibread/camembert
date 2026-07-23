# Adversarial review — Option A (sweep ledger)

> Verdict: **SURVIVABLE WITH AMENDMENTS** — the architecture is sound and
> the isolation claim is *literally true* in the code; the fatal-looking
> holes are all in the framing ("the gauge is the semantically correct
> home") and in three unstated lifecycle/honesty gaps, not in the shape.

## The through-line flaw

A's whole pitch rests on one equation: *freeable bytes are a subset of
the disk gauge's `used`, so the gauge is where they belong*. That
equation is exact **only for a single-filesystem, single-subvolume scan
with `--cross-filesystems` off**. The moment you cross a filesystem or
scan a btrfs subvolume layout — i.e. two of the most common real
configurations on the machines this feature targets — the gauge's
`statvfs` scope and the sweep's `st_dev` scope diverge, and the headline
number sits next to a bar it is no longer a fraction of. The doc never
reconciles the two scopes; it asserts they are the same line of truth.
They are not.

Verified against the code:

- `disk_space()` (ui.rs:305) does **one** `statvfs` on the scan root path,
  captured once at UI startup. `used = capacity − f_bfree` — a
  **single-filesystem** figure for whichever fs the root path lives on.
- The sweep's scope is `st_dev ∈ {devices actually walked}`. With
  `--cross-filesystems` that set spans **multiple real filesystems**
  (ScanOptions.cross_filesystems, scan.rs:267; per-dir `DirMeta.dev`,
  tree.rs:263).

So the gauge describes one disk; the freeable total can sum bytes across
several. The doc's claim that the freeable figure "*explains the gauge's
gap*" is only true when both are the same device.

## Findings (severity-ranked)

### FATAL-on-`--cross-filesystems` — freeable can exceed the gauge's `used` [1]

Scan `/` (a 20 GiB root SSD, 90% used) with `--cross-filesystems`,
crossing into `/mnt/backup` (a separate 4 TB HDD). A `postgres` process
holds a deleted 30 GiB WAL segment open on `/mnt/backup`. `st_dev` of
`/mnt/backup` is in the walked-device set, so the sweep counts it. The
gauge renders:

```
 disk ██████░░ 20 GiB · 90% used · this scan covers 4% of used · 30 GiB freeable (deleted, still open)
```

`30 GiB freeable` against a `20 GiB` disk at `90% used` (= 18 GiB used) is
visibly nonsensical: freeable > used > capacity, on the line whose entire
job is filesystem honesty. This is not a rounding wart — it is the
feature's headline number contradicting the widget it is glued to, on the
tool whose thesis is "never show a number you can't stand behind." As
written this is a *self-inflicted* dishonest number, the exact sin A was
built to avoid.

Amendment (mandatory): scope the **gauge suffix** to the root
filesystem's `st_dev` only, and label it as such
(`· 1.2 GiB freeable on this filesystem`). Freeable found on *other*
crossed devices belongs in the `f` panel under a per-filesystem
breakdown, never summed onto the root gauge. That keeps every number a
fact about the line it sits on.

### SERIOUS — btrfs subvolumes: the number silently undercounts [2]

The mirror image of [1], and more insidious because it's an
*under*-count that reads as reassurance. On the default Arch/openSUSE
btrfs layout (`@` at `/`, `@home` at `/home`, `@snapshots`, …), **each
subvolume has its own anonymous `st_dev`** but they all draw from **one
physical free-space pool** — `statvfs` on any of them reports the whole
filesystem. Scan `/` (subvol `@`, `st_dev = X`) without
`--cross-filesystems`; a process holds a deleted 8 GiB file open under
`/home` (subvol `@home`, `st_dev = Y ≠ X`). The sweep's device filter
drops it (`Y ∉ {X}`). The gauge's `used` **includes** those 8 GiB
(whole-pool statvfs), so the residual the doc says freeable "explains"
is 8 GiB larger than the freeable figure shown. The user reads
"0 B freeable," concludes the df/du gap is something else, and the one
tool that promised to explain it quietly didn't.

This is not exotic — it's the distro default on two major ecosystems, and
`st_blocks × 512` on btrfs is itself only a loose proxy once extents are
shared with a snapshot (closing the inode frees nothing that a snapshot
still pins). The doc's "Excluded, stated in docs" list (§2) names memfd,
mmap-only, and loop devices but **not** sibling-subvolume deleted files
or CoW-shared-extent overcounting. Coverage honesty is a headline claim;
this is an unlisted, common gap.

Amendment: document the subvolume/CoW limitation explicitly in the
coverage footer and README, in the same breath as the ptrace one. If the
gauge suffix is root-fs-scoped per [1], at least state that "on this
subvolume" ≠ "on this btrfs pool."

### SERIOUS — the deletion-warning reuse inherits the ptrace gate but not its honesty caveat [3]

The reuse claim (§3, §4) is architecturally clean — `open_file_index`
really is the same walk minus the `st_nlink==0` filter — but the doc gives
the freeable *panel* a coverage footer ("365 of 505 processes
unreadable") and gives the *delete-confirm warning* **nothing**. Both
reads go through the identical `PTRACE_MODE_READ_FSCREDS` gate (research
§4). Concrete: you run `camembert` as yourself (not root); you mark a
file that `postgres` (uid `postgres`) holds deleted-open; `open_file_index`
gets EACCES on postgres's `fd/`, finds no holder, and the modal shows the
reassuring *absence* of a warning. You confirm, the space isn't freed,
and — worse than no feature — you were actively told nothing was wrong.
The warning fails **open** precisely in the multi-user server case that
motivates it (research §8).

Amendment: the delete-confirm warning must carry the same coverage
disclaimer as the panel ("N processes unreadable — a holder may be
invisible without root"), or it's a false-reassurance machine.

### SERIOUS — the warning also races itself open under load [4]

§4 says confirmation is "not blocked" on the advisory `open_file_index`
walk. On the 10k-process server where the walk is slowest (linear in
fds — research §6 measured 37 ms for 505 procs, so ~0.7 s+ for 10k), the
fast-fingered admin presses `y` before the warning lands. The warning is
async-useless exactly when it's slowest to compute, i.e. exactly on the
big server where a wrong deletion is most expensive. The doc frames the
non-blocking choice as pure UX kindness; the cost is that the safety net
is absent under load. This is defensible (blocking teaches hatred) but
must be *stated* as "advisory, may not have arrived," not sold as a
guard rail.

### SERIOUS — `--output -` (dump to stdout) vs the summary line [5]

§4: "`--no-ui` mode runs it inline after totals and prints one summary
line." The doc never mentions the `-o -` case. In summary mode, every
stdout line is gated behind `!dump_to_stdout` (main.rs:562–642) precisely
because the dump binary stream owns stdout — even the error report is
suppressed (main.rs:629). A naively-added freeable summary line would
inject text into a zstd dump stream and corrupt it. Not hard to fix
(follow the existing gate), but the lifecycle section is incomplete: it
enumerates interactive / `--no-ui` and omits the one stdout mode that
actively forbids extra prints.

Amendment: gate the summary freeable line on `!dump_to_stdout`, and say
so in §4.

### ANNOYING — the "scanned-device set" is undersold as free [6]

§3: "takes the scanned-device set (a small addition to `ScanOutcome`;
`DirMeta.dev` already carries per-directory devices)." Verified: the
owner tracks **no** device set today (owner.rs collects `excluded_dirs`/
`excluded_kernfs` counters and per-`DirMeta.dev`, nothing aggregated).
So the set must be materialized — either a new `FxHashSet<u64>` fed at
`add_dir`, or an O(dirs) post-scan pass over every `DirMeta`. Cheap
(one insert per directory), but "already carries" hides that the
aggregate doesn't exist. Trivially fixable; flagged only because the doc
prices it at zero.

### ANNOYING — discoverability is worse than §9.2 admits [7]

The doc concedes the headline hangs on one gauge suffix. The code makes
it worse in two ways it doesn't mention:

- **Zen mode hides the gauge entirely** (`cards_and_gauge_heights` returns
  `(0,0)` in zen — ui.rs:1038/2164; state.rs:273 "no disk gauge"). A zen
  user has *zero* surface for the feature — not even the one line.
- **The single gauge line is already full.** It renders
  ` disk [bar] <cap> · X% used · this scan covers Y% of used ` and sizes
  the bar via `saturating_sub` of the text width (ui.rs:1179–1182).
  Appending `· 1.2 GiB freeable (deleted, still open)` (~38 cols) drives
  `bar_width` to 0 on an 80-column terminal — the gauge loses its bar to
  make room for the suffix. The headline cannibalizes the widget it rides
  on.

The `f` binding itself is clean (`f` is unused in the keymap; sort keys
are d/a/n/m/c/e). But "notice the gauge" is a weaker hook than §9.2 lets
on. The proposed scan-end toast is the right patch; without it the
feature is close to invisible.

### COSMETIC / LATENT — no dump-viewer today, but the trigger is a trap for one [8]

The task worried about "import-from-dump sessions where no live /proc
corresponds to the dump." Good news for A: **there is no interactive
dump-viewing mode** (main.rs dispatch: `scan` / `diff` / `import` only;
`diff` and `import` are non-interactive — main.rs:407–421). The sweep's
"fire on Phase→Done" trigger only ever runs over a *live* scan of the
local machine, so §4's lifecycle is clean for every current entry point.
Credit where due.

The latent risk: the dump-format work (seekable zstd, ordered records)
makes a future "open a `.cmbt` in the TUI" mode plausible, and it would
hit the same `Phase::Done` transition. `st_dev` scoping mostly saves it
(a foreign tree's devices won't match local `/proc` inodes → freeable ≈ 0,
honestly showing nothing) — but a dump *made on this machine of this
filesystem* and reopened later would match local `/proc` deleted files on
the same `st_dev` against a **stale historical tree**, silently mixing
live process state into a frozen snapshot. Cheap to guard (tie the sweep
to a live-scan marker, not to `Phase::Done`); worth a one-line note so
the trap isn't rediscovered the hard way.

### COSMETIC — "gauge line sums both layers" glosses phase-2 overlap [9]

§8's phase-2 composition ("in phase 2 the gauge line can sum both
layers") assumes phase-1 (deleted-open, not-in-tree) and phase-2
(per-entry, in-tree) freeable are disjoint. Mostly true, but a
deleted-but-open file on btrfs can *also* have CoW-shared extents; its
`st_blocks` already counts extents a snapshot pins. Summing a
phase-2 shared-extent figure onto it risks double-counting the same
physical blocks. Minor, and shared with every du-class tool, but "sum
both layers" deserves a "modulo shared extents" asterisk.

## What survived the attack (genuinely)

- **The isolation claim is TRUE, verified.** A new `freeable.rs` needs
  nothing from `tree.rs`, `view.rs`, or the dump writer. `Row`
  (view.rs:61) and `DirTotals` are untouched; `ScanOutcome` (scan.rs:453)
  gains at most a device set. No arena, snapshot, dump, or diff change is
  forced. This is the doc's strongest claim and it holds.
- **Dump/diff exclusion is correct, not just defensible.** `diff`
  streams two dumps non-interactively and never touches `/proc`; putting
  freeable in a dump really would compare process populations. §6 is
  right.
- **The phase-2 growth path is credible.** The "excluded-reason side-map
  pattern" A points at is real (`Tree.excluded: FxHashMap<NodeId,
  ExcludedReason>`, tree.rs:322), the reserved in-bar "Freeable segment
  (wave 2)" is real (tui-design.md:66–68), and a `Row` freeable field is
  a clean add. A does **not** paint phase 2 into a corner: phase-1 bytes
  are legitimately not-in-tree and stay off the bars; phase-2 per-entry
  bytes get the reserved segment. Nothing built here has to be unbuilt.
- **The core filter/sizing/dedup science is sound** (`st_nlink==0`,
  `(dev,ino)` dedup, `st_blocks×512`, memfd exclusion) — that's the
  common ground all three options share and the research backs it.
- **TOCTOU / process-death handling is a non-issue** (ENOENT = skip,
  research §6). No finding.

## Verdict: SURVIVABLE WITH AMENDMENTS

A is not killable — its skeleton is the honest one, its blast radius is as
small as advertised (confirmed in code), and its phase-2 story is clean.
But it does **not** survive as written: the "gauge is the semantically
correct home" claim is false under `--cross-filesystems` and btrfs
subvolumes (findings [1], [2]), and the deletion-warning reuse ships a
false-reassurance path the doc doesn't acknowledge ([3]). A courtesy pass
would have waved those through because the *architecture* is right; the
*spec sheet oversells the number's coherence*.

Required amendments (in severity order):

1. **Scope the gauge suffix to the root filesystem's `st_dev`** and label
   it "on this filesystem." Cross-device freeable goes in the panel with a
   per-filesystem breakdown — never summed onto a single-fs gauge. [1]
2. **Document the btrfs-subvolume and CoW-shared-extent gaps** in the
   coverage footer and README, alongside the ptrace and mmap-only gaps
   already listed. [2]
3. **Give the delete-confirm warning the same coverage disclaimer** as the
   panel; state it is advisory and may miss holders it can't read. [3]
4. **State the warning's non-blocking race** as a limitation, not just a
   UX choice. [4]
5. **Gate the `--no-ui` freeable line on `!dump_to_stdout`** and add the
   `-o -` case to §4. [5]
6. Correct §3's "already carries" to "materialize a device set (cheap)."
   [6]
7. Ship the scan-end toast (or accept near-invisibility in zen/narrow
   modes); note the gauge-bar cannibalization on 80-col terminals. [7]
8. Tie the sweep trigger to a live-scan marker, not `Phase::Done`, so a
   future dump-viewer can't sweep local `/proc` against a foreign or stale
   tree. [8]

With 1–3 done, A is the honest option it claims to be. Without them, it
prints exactly the kind of incoherent number its own thesis forbids.
