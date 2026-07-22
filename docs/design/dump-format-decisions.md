# Dump format — decisions (co-design session, 2026-07-22)

Outcome of the co-design session over the
[options dossier](dump-format-options.md). These decisions are settled;
reopening one requires a new element, not re-litigation.

## D1 — Format family: Option A now, Option C later as cache

The v1 dump/interchange format is **Option A: JSON Lines in a zstd
seekable container**. SQLite (Option C) is deferred to wave 4 as an
**optional local cache/index derived from dumps** — regenerable and
deletable, never the interchange format. Option B (custom binary) is
dropped; its salvageable ideas (CRC-checked frames, PNG-style magic,
capability flags) are folded into A's spec where applicable.

## D2 — Hardlink attribution: deterministic canonical owner

Aggregate attribution of a hardlinked inode `(dev, ino)` is defined **in
the format spec**, not in reader code: the **canonical owner is the link
whose full path is smallest in the format's sort order** (raw-byte,
component-wise) among all links *seen by the scan*. The owner's directory
chain counts the full size; other links count 0 in aggregates but keep
their full per-entry metadata. Both the writer's aggregates and the differ
use this rule, making aggregates reproducible across scans of an identical
tree (kills the phantom-diff class of bugs).

## D3 — Sort key: raw name bytes

All ordering (sibling sort, DFS order, merge-join comparator) is defined on
the **raw filename bytes** (component-wise for paths). The percent-encoding
of non-UTF-8 bytes exists only to produce valid JSON strings; it is not the
sort key. Consequence: third-party tools that decode names and sort
naturally agree with camembert's comparator.

## D4 — u64 fields in JSON: strings above 2^53, inodes always

`ino` and `dev` are **always emitted as JSON strings**. Any other u64
field (sizes, counters) **must** be emitted as a string when its value is
≥ 2^53, and may be either below. Readers **must** accept both number and
string for every u64 field. Rationale: `JSON.parse`/jq-arithmetic silently
corrupt integers above 2^53 (empirically verified with a 63-bit inode).

## D5 — Low-disk behavior: degrade, but keep working

The tool never hard-fails a scan for lack of finalize space. If the
ordered finalize (which needs ~2× the compressed dump transiently) cannot
complete, the writer **keeps the unordered dump** (`"ordered":false` — a
fully valid, diff-upgradeable artifact) and prints a clear warning with
the upgrade path (`camembert dump sort`, possibly on another machine) and
the pipe alternative for next time.

## D6 — Naming and v1.0 scope

- Format name: `camembert-dump` (the `format` field of the header line).
- File extension: **`.cmbt`**.
- v1.0 field scope: the Option A schema as amended by D2–D5 (see the
  [spec](../format/dump-v1.md)). btrfs shared extents, atime, quotas,
  nanosecond mtime are explicitly *not* in v1.0 — they are additive minor
  bumps later.
