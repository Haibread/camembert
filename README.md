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
> dedup, mount-boundary detection), the interactive browse-during-scan
> TUI, guarded mark-then-confirm deletion, the ordered dump-format v1
> writer (`--output`) and reader, the streaming **`camembert diff`**
> between two dumps, and the **ncdu import** (`camembert import`) are
> implemented.
> See [HANDOFF.md](HANDOFF.md) for the full design hypotheses and roadmap,
> `docs/design/` for the settled decisions, and
> [`docs/format/dump-v1.md`](docs/format/dump-v1.md) for the dump format.

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

`camembert` has one default mode and two subcommands:

| Command | What it does |
| --- | --- |
| `camembert [PATH]` | scan (interactive on a terminal, summary otherwise) |
| `camembert diff OLD.cmbt NEW.cmbt` | compare two dumps — see [Diff](#diff-what-grew) |
| `camembert import NCDU.json -o OUT.cmbt` | convert an ncdu export — see [Import](#import-from-ncdu) |

(To scan a directory literally named `diff` or `import`, prefix it:
`camembert ./diff`.)

When stdout is a terminal, `camembert` opens the **interactive mode** by
default (see below). With `--no-ui` (env `NO_UI`), or automatically when
stdout is a pipe or a file, it runs in **summary mode**: scan to
completion, then print totals plus the largest directories by real
(on-disk) size.

```bash
# Browse the current directory interactively (default on a terminal)
cargo run --release

# Summary mode: scan /var, print the top 10 directories
cargo run --release -- /var --no-ui --top 10

# Pin the worker-thread count (0 = auto: 2x CPU cores, capped at 8)
cargo run --release -- /home --threads 4

# Follow mount points into other filesystems (off by default:
# mount points are recorded but not descended into). Kernel
# pseudo-filesystems (/proc, /sys, cgroups, …) are NEVER descended
# into, even with this flag: their numbers are not disk usage
# (/proc/kcore alone claims ~128 TiB of apparent size).
cargo run --release -- / --cross-filesystems

# Verbose engine diagnostics on stderr (product output stays on stdout)
cargo run --release -- /var --no-ui --log-filter camembert_core=debug

# Scan and write a dump (works in both modes; see "Dump" below)
cargo run --release -- /var -o var.cmbt
```

## Interactive mode

The core promise of the interactive mode: the tree is **navigable while
the scan runs**. Directories appear as they are discovered, totals fill in
and re-sort live (snapshots at ~30 fps; directories with more than ~20k
children update at 4 Hz and show an "updating…" note), and a spinner marks
directories still being scanned. Quitting mid-scan cancels the scan.

| Key | Action |
| --- | --- |
| `↓` / `j`, `↑` / `k` | move the cursor |
| `Enter` / `l` / `→` | open the directory under the cursor |
| `Backspace` / `h` / `←` | go back up to the parent |
| `g` / `G` | jump to the top / bottom |
| `d` | sort by real (disk) size — the default, descending |
| `a` | sort by apparent size |
| `n` | sort by name (raw bytes, ascending) |
| `m` | sort by modification time |
| `c` | sort by item count |
| `e` | sort by subtree error count — jump straight to what could not be read |
| *(active sort key again)* | reverse the sort direction |
| `p` | show/hide the apparent-size column |
| `Space` | mark/unmark the row under the cursor for deletion, then move down |
| `u` | clear all marks |
| `D` | delete the marked entries (confirmation dialog; `y` confirms) |
| `q` / `Esc` / `Ctrl-C` | quit (cancels the scan if still running) |

**Provisional totals (hardlinks)**: while the scan is running, a
hardlinked inode is attributed to the directory where it was *first seen*.
If any hardlinks were encountered, the footer shows *"provisional totals
(hardlinks) — corrected at scan end"* until the scan completes — the
final numbers count each inode exactly once. The note only appears when
hardlinks actually exist in the tree.

Diagnostics (`tracing`) never touch the interactive screen: they are
discarded by default, or written to a file with `--log-file scan.log`
(env: `LOG_FILE`) when you need them while debugging.

### Deleting

Deletion is **mark-then-confirm** (like dua): `Space` marks rows —
marking a directory implies its whole subtree, and marks persist while
you navigate — the footer keeps a running "marked: N entries, size"
total, and `D` opens a confirmation dialog listing the count, the
cumulative on-disk size and the first few paths. Only an explicit `y`
deletes; any other key cancels.

Deleting from a disk-usage tool is exactly where you want guard rails,
so camembert stacks several:

- **Only after the scan.** During the scan the mark keys are inactive
  (the footer says so): the tree is being written by the scan engine,
  and deletion must account against a frozen, consistent tree.
- **Never outside the scanned root.** Paths are rebuilt from the scanned
  tree, never taken from anywhere else, and anything not strictly under
  the scan root is refused.
- **Mount points are refused at mark time** (and re-checked before
  deletion): an excluded mount was never scanned, so deleting it would
  act blind. Unreadable (error) directories stay markable — deleting a
  directory you cannot read is legitimate cleanup.
- **The filesystem is re-checked entry by entry** right before deletion:
  the entry must still exist and its file type (and, for directories,
  its device) must still match what was scanned. A file that changed
  into something else since the scan is skipped with a note — never
  deleted. Symlinks are removed themselves and never followed.
- **Failures never abort the batch.** Permission errors, vanished
  files, … are counted per entry ("deleted 10 (3.4 GiB freed), failed
  2 — see log") and detailed in the log; everything else proceeds.
- **Hardlinks are reported honestly**: deleting one link of an
  `nlink > 1` inode frees space only when the *last* link goes, so the
  dialog warns when the selection contains hardlinked files, and links
  that were never counted free 0 in the totals.

After a deletion the view rebuilds in place: totals in the header
shrink accordingly, and if the directory you were viewing was itself
deleted, the view climbs to the nearest surviving parent.

Planned next (see `camembert-core/src/delete.rs`): a warning when a
marked file is held open by a running process (`/proc/*/fd`), and an
opt-in XDG-Trash mode instead of unlinking.

## Summary mode

Example output:

```text
Scanned /usr/share/licenses in 0.04s
  total: 18.7 MiB real, 16.0 MiB apparent
  entries: 1713 (591 dirs)  errors: 0  excluded mounts: 0 (0 kernfs)

Top 5 directories by real size:
    18.7 MiB  /usr/share/licenses
     5.8 MiB  /usr/share/licenses/slack-desktop
```

Notes on the numbers (honesty is the point of this tool):

- **real** is `st_blocks * 512` (what the tree occupies on disk),
  **apparent** is `st_size`; sparse files, compression and tail slack make
  them legitimately disagree.
- Hardlinked inodes are counted **once**, attributed to their canonical
  link (the smallest path in raw-byte order) — deterministic across
  scans of an identical tree.
- Unreadable directories never abort the scan: they are counted in
  `errors` and the affected totals stay honest about what was not read.
  When `errors > 0` the summary lists the directories where the failures
  actually happened (direct counts, not subtree rollups), so an incomplete
  total is never unexplained; in the TUI, sort with `e` to find them.
- Excluded mounts split into real filesystems (descend with
  `--cross-filesystems`) and kernel pseudo-filesystems (`/proc`, `/sys`,
  cgroups… — never descended into: their numbers are not disk usage).
- Symlinks are never followed; they count with their own (link) size.

While a scan runs, a progress line (entries, dirs, errors, bytes so far)
is logged to stderr every second.

Every CLI option is also settable through an environment variable
(`SCAN_PATH`, `LOG_FILTER`, `LOG_FILE`, `THREADS`, `CROSS_FILESYSTEMS`,
`TOP`, `NO_UI`, `OUTPUT`, and for `diff`: `JSON_OUTPUT`, `THRESHOLD`);
see `cargo run -- --help` (and `--help` on each subcommand) for the full
reference, including the interactive-mode key map.

## Dump

`--output FILE` / `-o FILE` (env: `OUTPUT`) writes a **camembert-dump v1**
(`.cmbt`) once the scan completes — the interchange format scans are
diffed and re-browsed from (spec:
[`docs/format/dump-v1.md`](docs/format/dump-v1.md)). It works in both
modes: summary mode writes it after the summary; interactive mode writes
it as soon as the scan finishes (quitting mid-scan cancels the scan and
skips the dump). The file is written to `FILE.part` first and atomically
renamed, so a crash never leaves a truncated dump under the final name.

The container is JSON Lines inside a seekable zstd stream, so **stock
tools read it directly** — no camembert needed:

```bash
# Scan /var and dump it
camembert /var --no-ui -o var.cmbt

# Inspect with standard tools: one JSON object per line
zstdcat var.cmbt | jq .

# e.g. the 5 biggest directories by on-disk subtree size
zstdcat var.cmbt | jq -r 'select(.t == "d") | [.td, .path] | @tsv' \
  | sort -rn | head -5

# '-' streams the dump to stdout instead (summary mode only)
camembert /var --no-ui -o - | zstdcat | jq -c 'select(.t == "e")'
```

Format notes:

- One header line (`t:"h"`) declares the version and capabilities; a
  final `t:"e"` line marks clean completion. Directory lines (`t:"d"`)
  carry subtree totals (`ta`/`td`/`tn`/`te`); their child entries follow
  with per-entry sizes, mtime, and kind.
- Filenames are raw bytes: non-UTF-8 bytes appear percent-encoded
  (`%XX`), `%` as `%25`. Sibling order is always raw-byte order.
- `ino`/`dev` are JSON **strings**, and any integer ≥ 2^53 is emitted as
  a string too, so `jq`/JavaScript arithmetic never silently corrupts
  them.
- Hardlinked inodes keep full metadata on every link (`i`/`l` fields) but
  are counted once in totals, at the canonical link.

## Diff (what grew)

`camembert diff OLD.cmbt NEW.cmbt` answers *the* incident question — "what
changed since yesterday" — by streaming both dumps through a
constant-memory merge-join (neither tree is ever loaded whole; memory is
bounded by one directory block per side plus the top-N heaps, regardless
of dump size).

```bash
camembert /var --no-ui -o monday.cmbt
# ... a day later ...
camembert /var --no-ui -o tuesday.cmbt
camembert diff monday.cmbt tuesday.cmbt
```

Output: a summary (total disk/apparent/entry delta, counts of
added/removed/grown/shrunk/touched/type-changed entries and added/removed
directories), then **Top N directories by growth** (signed subtree disk
delta, straight from the dump totals — canonical hardlink attribution
included — biggest growth first, shrinkage negative) and **Top N entries
by growth**. `--top N` (env `TOP`, default 20) sizes both lists.

Change kinds: `added`, `removed`, `grown`, `shrunk`, `touched` (same
sizes, different mtime), `typeChanged` (file ↔ symlink/device/…, and file
↔ directory — a file replaced by a directory of the same name is
reported as one type change, not a remove + add).

### Machine output (`--json`, env `JSON_OUTPUT`)

JSON Lines on stdout — one `summary` object, then one object per top
directory and per top entry:

```json
{"t":"summary","oldRoot":"/var","newRoot":"/var","diskDelta":123,"apparentDelta":120,"entryDelta":2,"added":5,"removed":1,"grown":3,"shrunk":0,"touched":1,"typeChanged":0,"dirsAdded":1,"dirsRemoved":0}
{"t":"dir","path":"/var/log","change":"changed","diskDelta":123,"apparentDelta":120,"entryDelta":2}
{"t":"entry","path":"/var/log/syslog","change":"grown","diskDelta":100,"apparentDelta":100}
```

Same conventions as the dump format: paths are percent-encoded raw bytes
(`%XX` for non-UTF-8), and any integer with magnitude ≥ 2^53 is emitted
as a decimal string — parse both.

### Monitoring probe (`--threshold`, env `THRESHOLD`)

With `--threshold SIZE`, the exit code becomes the verdict:

```bash
camembert diff monday.cmbt tuesday.cmbt --threshold 2G --json
echo $?   # 0 = grew by at most 2 GiB (or shrank), 1 = grew more, 2 = error
```

Size syntax: decimal number + optional binary unit `K`/`M`/`G`/`T`/`P`
(1K = 1024 bytes), `iB`/`B` suffixes and fractions accepted — `500M`,
`2G`, `1.5GiB`, `1048576`. Only *growth* trips the threshold; shrinkage
exits 0.

Exit codes (also without `--threshold`): `0` OK, `1` growth above the
threshold, `2` error — unreadable file, not a dump, unsupported major
version, unordered dump (`"ordered":false`: re-dump with the ordered
writer; `camembert dump sort` is planned to upgrade in place), or a
truncated dump (missing `e` end marker — refused because the missing
tail would show up as phantom changes).

## Import (from ncdu)

`camembert import NCDU.json -o OUT.cmbt` converts an
[ncdu JSON export](https://dev.yorhel.nl/ncdu/jsonfmt) (`ncdu -o`,
major 1, minors 0–2; newer minors import with a warning) into an ordered
camembert dump. `-` reads stdin, `-o -` writes the dump to stdout:

```bash
ncdu -x -o- / | camembert import - -o baseline.cmbt
# or convert an old export you kept around:
zcat old-export.json.gz | camembert import - -o old.cmbt

# THE use case: diff an old ncdu export against a fresh camembert scan
camembert / --no-ui -o today.cmbt
camembert diff old.cmbt today.cmbt --top 30
```

The import is streaming (SAX-style pull parser, no full-file JSON value)
and tolerant of the format's history: raw non-UTF-8 bytes inside names
(pre-2.5 exports) are preserved as raw bytes, absent `dev` inherits the
parent's, `hlnkc`/`nlink` hardlinks are deduplicated by `(dev, ino)` and
re-attributed to their canonical (smallest-path) link, and ncdu's
unspecified sibling order is re-sorted, so the result is a first-class
ordered dump.

What maps how (and what is lost — the importer never invents data):

| ncdu | camembert dump |
| --- | --- |
| `name` | `n`/`path` (raw bytes, `%XX`-encoded) |
| `asize` / `dsize` | `a` / `d` |
| `ino`, `nlink`, `hlnkc` | `i`, `l` + hardlink dedup |
| `read_error` | `err:true` (+ `te` error counts) |
| `excluded: otherfs/othfs/kernfs` | never-scanned dir stub with `ex` |
| `excluded: pattern/frmlink/…` | entry `ex:"otherfs"` (reason collapsed, lossy) |
| `notreg` | non-regular entry, exact kind unknown (no `k`) |
| extended `mtime` (`ncdu -e`) | `m`; without `-e` mtimes import as `0` |
| extended `uid`/`gid`/`mode` | dropped (no extended storage yet) |
| `dev` of a non-hardlinked file | dropped (only hardlinks carry `dev`) |
| `hlnkc` without `ino` (very old) | not dedupable, counted fully |
| metadata block | ignored (as ncdu itself documents) |

Exit codes: `0` OK, `2` error (unreadable input, not an ncdu export,
unsupported major version, write failure).

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
