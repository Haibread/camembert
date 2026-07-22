# Research digest: dump/export formats for disk-usage analyzers

Factual input for the dump-format design (HANDOFF.md §4). Facts with sources,
no recommendations. Gathered 2026-07-22.

## 1. ncdu export format

**Two distinct formats now exist.**

### JSON format (`-o`, ncdu ≥1.9), spec: <https://dev.yorhel.nl/ncdu/jsonfmt>

- Top-level: JSON array `[majorver, minorver, metadata, directory]`. Major
  version must be `1`. Minor version: `0` for ncdu 1.9–1.12, `1` for
  1.13–1.15 (extended-mode addition), `2` since ncdu 1.16 (adds `nlink`).
- `metadata` object: `progname`, `progver`, `timestamp` — documented as
  **ignored on import**.
- Tree nesting: a directory is a JSON array whose first element is an
  "infoblock" (object) for the dir itself, followed by child
  infoblocks/subdirectory-arrays. Recursive nested JSON, not flat.
- Per-entry fields: `name` (≤32768 bytes, not necessarily UTF-8; ncdu ≥2.5
  rejects invalid encodings it used to accept), `asize` (`st_size`), `dsize`
  (`st_blocks * S_BLKSIZE`), `dev` (**absent ⇒ inherit parent's dev**), `ino`
  (**only emitted since 1.16 when `st_nlink > 1`**), `nlink` (since 1.16),
  `hlnkc` (bool, redundant with `nlink>1`), `read_error`, `notreg`,
  `excluded` (enum: `pattern`, `otherfs`/`othfs`, `kernfs`, `frmlink`).
  Extended info only with `-e` (since 1.13): `uid`, `gid`, `mode`, `mtime`.
- **Streamability**: the spec itself recommends a stream-based (SAX-style)
  JSON parser for multi-million-file exports.
- **Known upstream problem**: in multithreaded scan mode the JSON exporter
  buffers the entire tree in memory before writing — the stated reason the
  binary format was created.

### Binary format (`-O`, ncdu ≥2.6), spec: <https://dev.yorhel.nl/ncdu/binfmt>

- Created to fix the JSON exporter's full-tree-in-RAM problem and to support
  browsing trees too large for RAM.
- 8-byte magic `\xbf ncduEX1`. File = sequence of length-prefixed *and*
  length-suffixed blocks (bidirectional scanning) of two types: **Data**
  (type 0) and **Index** (type 1, exactly one, must be last).
- Data blocks: numbered sequentially, each a **single Zstandard frame**
  (content size via `ZSTD_getFrameContentSize()`); max block size 16 MiB − 1.
- Index block: array of `(offset:40bit, length:24bit)` pointers to data
  blocks, plus a `root_itemref`.
- Items encoded as **CBOR maps** (19 recognized integer keys), linked via
  `prev` (singly-linked list) and `sub` (dir → last child).
- Designed for **multithreaded writers with minimal synchronization** (each
  thread fills independent data blocks; index consolidated at the end) and
  for lazy partial reads.
- Caveat: the index block is **written last** — a writer killed mid-scan
  leaves data blocks but no valid index; the tree is unreachable.

## 2. Other tools

- **gdu** (`-o`): JSON explicitly modeled on ncdu's schema (compat fixes in
  <https://github.com/dundee/gdu/commit/045ad4d>), interoperable with ncdu.
- **dua-cli**: `-p json` / `-p csv` output; exact schema not documented in
  README (open gap).
- **duc**: no dump file — persistent **database index** (Tokyocabinet by
  default; LevelDB, SQLite3, LMDB as build options). Tested on >500M files,
  multi-PB (<https://duc.zevv.nl/>). Stores per-directory cumulative sizes.
- **WizTree**: flat CSV export (`/export=`), columns: File Name (folders end
  in `\`, recursive totals), Size, Allocated (leading-zero flag on hardlinks
  = "doesn't consume extra space"), Modified, Attributes bitmask, files/
  folders counts. Flat, pre-aggregated, not a tree.

## 3. Sizing estimates, 10 M entries

- **ncdu-style JSON**: measured data point — 10k files ≈ 600–700 KiB
  uncompressed, ≈100 KiB gzipped, scales linearly → **≈60–70 B/entry raw,
  ≈10 B/entry gzipped** (basic fields). 10 M entries ≈ **600–700 MB raw,
  ~100 MB compressed**. With `-e` extended info, estimated (not sourced)
  ~1.2–1.7 GB raw.
- **Compact binary record** (name interned u32, asize u64, dsize u64,
  mtime u64, uid u32, ino u64, parent u32): **44 B/record** unpadded,
  ~48 B aligned → **440–480 MB** per 10 M-entry dump before compression.
  Same order of magnitude as HANDOFF §3's in-memory arena estimate.
- **Diff-memory implication**: loading two 10 M-entry dumps fully ≈ 1 GB RSS
  before diff bookkeeping. A **sorted-order streaming merge-join** needs only
  a constant window per side — turns ~1 GB into a few MB. This is the direct
  payoff of a deterministic sort-order guarantee in the format.

## 4. Candidate serialization technologies (Rust)

| Format | Maturity | Streaming write | Partial/lazy read | Schema evolution | Crates |
|---|---|---|---|---|---|
| JSON Lines + zstd | Very mature | Trivial (line append) | Line-granular | Self-describing, additive | `serde_json` + `zstd` |
| MessagePack | Mature | Yes (writer over `io::Write`) | Sequential streaming | Only in named-field mode | `rmp-serde` |
| CBOR | `serde_cbor` archived; `ciborium` / `minicbor` maintained | Yes, native multi-value streams | Sequential streaming | Named/tagged fields, additive | `ciborium`, `minicbor` |
| bincode | Mature | Yes | Not self-describing, no skipping | **None** (caller's problem) | `bincode` |
| postcard | Mature, no_std | Yes | Same as bincode | **None** (explicit non-goal) | `postcard` |
| Cap'n Proto | Mature | Message-sequence framing | **Zero-copy random access** | Native (field ordinals) | `capnp`, `capnpc` |
| FlatBuffers | Mature | **No — whole buffer in memory before finalize** | Excellent (mmap) | Native | `flatbuffers` |
| Parquet | Mature | Row-group buffered | Columnar pushdown, needs footer | Table-model migrations | `parquet`, `arrow` |
| SQLite | Very mature | WAL-incremental | Full SQL random access | `ALTER TABLE` migrations | `rusqlite` |

zstd seekable-frame format: Rust support exists (`zstd-seekable` /
`zeekstd` crates) for random access into compressed streams.

## 5. Streaming diff prior art

- **mtree** (FreeBSD/NetBSD): spec files are **kept in filename-sorted
  order**; compare mode sorts both specs and produces `comm(1)`-style output
  — primary-source confirmation that **sorted emission enables an
  O(1)-memory merge-join diff**. (<https://man.freebsd.org/mtree>)
- **btrfs send**: linear command stream derived from the FS's own b-trees,
  not a generic diff — FS-internals-driven, not portable prior art.
- **restic** `diff`: `+`/`-`/`U`/`M`/`T`/`?` classification; deterministic
  JSON encoding required for dedup; sort-order guarantee of tree nodes not
  confirmed (open gap).
- **borg**, **tmutil compare**: internal algorithms/memory bounds not found
  (open gap).

## 6. Crash / partial-write tolerance

- **JSON Lines**: best-in-class — every complete line is valid; one corrupt
  trailing line max.
- **ncdu JSON tree**: not crash-tolerant (unbalanced brackets).
- **ncdu binary**: not crash-tolerant (index required, written last).
- **MessagePack/CBOR streams**: like JSONL — valid values up to truncation.
- **bincode/postcard**: depends entirely on caller-side framing.
- **Cap'n Proto**: tolerant at message boundaries if written as a sequence
  (application-level convention).
- **FlatBuffers**: worst case — whole buffer lost on mid-write death.
- **Parquet**: truncated file entirely unreadable (footer at end).
  (<https://parquet.apache.org/docs/file-format/data-pages/errorrecovery/>)
- **SQLite WAL**: most battle-tested — checksummed, length-prefixed WAL
  records; torn writes discarded on recovery. (<https://sqlite.org/wal.html>)

## Open gaps

- dua-cli exact JSON schema.
- duc per-entry byte cost in its actual schema.
- restic tree-node sort-order guarantee and diff memory bound.
- borg / tmutil diff algorithms.
- Measured ncdu `-e` JSON size at 10 M-file scale.
