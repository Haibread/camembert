# camembert dump format v1.0 — specification (DRAFT)

Status: **draft** — implements the decisions in
[dump-format-decisions.md](../design/dump-format-decisions.md); wording and
edge cases may still be tightened during MVP implementation, semantics may
not drift without a new design decision.

## 1. Overview

A camembert dump (extension **`.cmbt`**) is a stream of JSON Lines inside a
zstd *seekable* container. It is:

- **streamable**: written incrementally during a multithreaded scan;
- **crash-tolerant**: every fully-flushed frame is readable; a killed
  writer leaves a valid (degraded) dump;
- **self-describing and versioned**: one header line; unknown keys and
  record types are ignored by readers;
- **tool-friendly**: `zstdcat dump.cmbt | jq` works with stock tools.

## 2. Container

The byte stream is a sequence of **independent zstd frames** (target
~512 KiB uncompressed each), followed by a **seek table in a zstd
skippable frame** (the standard zstd seekable format). Standard `zstd -d`
/ `zstdcat` decode the whole stream and silently skip the seek table.

- Frames are independent: decompression can start at any frame boundary.
- A frame boundary always falls between two lines (no line spans frames).
- The seek table is optional (absent on pipes and crashed dumps).

## 3. Line grammar

The decompressed content is newline-terminated JSON objects (JSON Lines).
The `t` key selects the record type; **a line without `t` is a file-system
entry**, the common case.

| `t` | record | cardinality |
|---|---|---|
| `"h"` | header | exactly 1, first line |
| `"d"` | directory | 1 per scanned directory |
| `"s"` | block defaults | 0..1 per directory block |
| *(absent)* | entry (child of the last `d`) | 0..n per block |
| `"x"` | frame index | 0..n, after the last block |
| `"e"` | end / clean-completion marker | 0..1, last line |

Readers **must** ignore unknown keys on any line and skip lines with an
unknown `t` (see §10).

## 4. Names and sort order

- Filenames are raw bytes. To form a valid JSON string, bytes that are not
  part of valid UTF-8 are emitted as `%XX` (uppercase hex) and a literal
  `%` as `%25`. The encoding is injective and reversible.
- **The sort key is the decoded raw bytes**, not the encoded form
  (decision D3). Sibling order and the diff comparator compare raw bytes
  ascending.
- Paths compare **component-wise**: split on `/`, compare components as
  raw byte strings left to right. (Whole-string comparison would misorder
  `foo.bar` vs `foo/x`.)
- `.` and `..` never appear as entries.

## 5. Numbers (decision D4)

- `ino` and `dev` are **always JSON strings** (decimal).
- Every other u64 field is a JSON number when < 2^53 and **must** be a
  decimal string when ≥ 2^53.
- Readers **must** accept number or string for all u64 fields.

## 6. Records

### 6.1 Header (`t:"h"`)

```json
{"t":"h","format":"camembert-dump","v":1,"minor":0,"prog":"camembert",
 "progver":"0.1.0","ts":1753142400,"root":"/var","dev":"64769",
 "sem":"blocks","ext":true,"ordered":true,"allino":false}
```

- `format` must be `camembert-dump`; `v` is the major version (readers
  refuse a major they don't know), `minor` the additive level.
- `root`: scan root path (encoded like names). `dev`: its device (string).
- `sem`: size semantics of defaults — `"blocks"` (st_blocks×512, the
  default) or `"apparent"`.
- `ext`: extended metadata (uid/gid/mode) present.
- `ordered`: see §7. `allino`: every entry carries `i` (default: only
  `nlink > 1`).

### 6.2 Directory (`t:"d"`)

Opens a directory block; subsequent `s`/entry lines belong to it until the
next `d`.

```json
{"t":"d","path":"/var/log/nginx","a":4096,"d":4096,"m":1753100000,
 "u":0,"g":4,"p":493,"nf":12,"nd":1,"ta":104857600,"td":105906176,
 "tn":14,"te":0}
```

- `path`: full path (encoded). `a`/`d`/`m`: the directory inode's own
  apparent size, disk size, mtime. `u`/`g`/`p`: uid/gid/mode (ext only).
- `nf`/`nd`: direct file/subdir counts.
- `ta`/`td`/`tn`/`te`: subtree totals — apparent, disk, inode count,
  unreadable-children count. **Present only in ordered dumps** (computed
  at finalize). Subtree totals apply hardlink attribution (§8).
- `err:true`: the directory itself could not be read (`nf`/`nd` 0, totals
  cover the directory inode only).
- Subdirectories are **not** repeated as entry lines in the parent block;
  parenthood is implied by `path`.

### 6.3 Block defaults (`t:"s"`)

Optional; resets at each `d` line. Sets block-local defaults for `u`, `g`,
`p`, `dev` so common values are elided from entry lines (mtree `/set`
style).

### 6.4 Entry (no `t`)

```json
{"n":"access.log","a":9437184,"d":9441280,"m":1753141000}
{"n":"rotated%FF.gz","a":1048576,"d":1052672,"m":1750000000,"i":"393221","l":2}
{"n":"latest","k":"l","a":11,"d":0,"m":1740000000}
```

| key | meaning | presence |
|---|---|---|
| `n` | name (encoded, §4) | always |
| `a` | apparent size (`st_size`) | always |
| `d` | disk size (`st_blocks`×512, bytes) | always |
| `m` | mtime, unix seconds | always |
| `k` | kind: `l` symlink, `b`/`c` block/char dev, `f` fifo, `s` socket | non-regular files only |
| `u` `g` `p` | uid, gid, mode bits | ext mode; elided when equal to block default |
| `i` | inode (string) | when `nlink>1`, or always if `allino` |
| `l` | nlink | when `nlink>1` |
| `dev` | device (string) | when ≠ block default |
| `err` | `true`: stat/read failed | on error |
| `ex` | excluded: `"pattern"` `"otherfs"` `"kernfs"` `"frmlink"` | when excluded |

The `ex` enum values are ncdu-compatible; unknown values must be preserved
as opaque strings.

### 6.5 Frame index (`t:"x"`) and end marker (`t:"e"`)

```json
{"t":"x","f":412,"p":"/var/log/nginx"}
{"t":"e","entries":10000000,"dirs":947213,"errors":340,
 "ta":1893459827345,"td":1912345678901,"elapsed":41.2}
```

- `x`: frame ordinal `f` → first `d`-path in that frame. Present in
  ordered dumps; enables binary search for lazy browse.
- `e`: **presence of the `e` line is the clean-completion marker** (the
  header is written first and cannot know). A dump without `e` was
  interrupted.

## 7. Ordering tiers (header `ordered`)

- **Tier 1 — always**, even in a killed writer's output: each directory
  block is contiguous and its entry lines are sorted by raw name bytes.
- **Tier 2 — iff `"ordered":true`**: `d` lines appear in DFS preorder with
  siblings sorted by raw name bytes (equivalently: total order by
  component-wise path comparison, §4). Ordered dumps enable the
  constant-window streaming merge-join diff.

Unordered dumps (`"ordered":false`) are produced on pipes and by degraded
finalize (decision D5); `camembert dump sort` upgrades them.

## 8. Hardlink attribution (decision D2)

For each `(dev, ino)` with `nlink > 1`, the **canonical owner** is the
link whose full path is smallest under the §4 comparator, among the links
seen by this scan. Subtree totals (`ta`/`td`/`tn`) count the inode's sizes
at the owner only; other links contribute 0 to aggregates but keep full
per-entry metadata. Writers and differs must apply the same rule.

## 9. Crash tolerance and degraded states

| state | on disk | readable? |
|---|---|---|
| clean, ordered | header … blocks … `x` … `e` + seek table | fully |
| clean, unordered (pipe / D5 degrade) | header … blocks (completion order), `e` | yes; sort to upgrade |
| killed mid-scan | header … blocks, torn last frame | yes minus the torn frame; no aggregates; `repair` upgrades |
| killed mid-finalize | `.part` intact (final written via rename) | as killed-mid-scan |

A torn zstd frame is detected by the frame checksum and dropped whole
(~8k entries worst case). The finalize step transiently needs ~2× the
compressed dump size on disk; when unavailable the writer degrades per D5
instead of failing.

## 10. Versioning and evolution

1. Readers ignore unknown object keys; new per-entry/per-record fields are
   a **minor** bump.
2. Readers skip lines with unknown `t`; new record types are a minor bump.
3. Unknown enum values (`ex`, `k`) are preserved opaquely.
4. **Major** bump only for changes that would silently corrupt a diff:
   the comparator, the name encoding, the number encoding, line framing,
   or the hardlink attribution rule.
5. Capabilities are declared in the header (`ext`, `ordered`, `allino`),
   never inferred from data.

Planned additive fields (not in 1.0): `mn` (mtime nanos), `at` (atime),
`bt` (btrfs shared-extent bytes), quota records.

## 11. ncdu interoperability

ncdu's JSON export (both 1.x minors) maps 1:1 onto entry fields
(`asize→a`, `dsize→d`, `ino/nlink→i/l`, `read_error→err`, `excluded→ex`,
extended `uid/gid/mode/mtime→u/g/p/m`; absent `dev` inherits the parent's,
which the importer resolves). Import = SAX-parse the nested tree, sort
siblings by raw bytes, emit through the ordered writer. Export back to
ncdu JSON is the same mapping reversed (also covers gdu).
