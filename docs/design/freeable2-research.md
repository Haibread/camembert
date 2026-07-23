# Research digest: freeable column phase 2 (btrfs shared extents, hardlink siblings)

Factual input for HANDOFF "Suggested next steps" §1 continuation and
`freeable-attack-b.md`'s central non-additivity problem. Facts with sources
and measurements, no recommendations. Gathered 2026-07-23 on the same
CachyOS (Arch-based) desktop as `freeable-research.md`: kernel
`7.1.4-1-cachyos` (`Linux 7.1.4-1-cachyos #1 SMP PREEMPT_DYNAMIC Sat, 18 Jul
2026`), `btrfs-progs v7.1`, root filesystem `/home` on `/dev/nvme0n1p2`,
mounted `rw,noatime,compress=zstd:1,ssd,discard=async,space_cache=v2`. Every
experiment below ran unprivileged as a normal user, in a scratch directory
(`~/.cache/camembert-freeable2-exp`, real btrfs, **not** the tmpfs-backed
`/tmp` scratchpad) — nothing in the repo was touched. Two throwaway btrfs
subvolumes created for the snapshot test could not be deleted afterwards
without root (§3) and are left behind in that scratch directory.

Companion Rust source read for "what the scan already knows":
`camembert-core/src/scan/hardlink.rs`, `camembert-core/src/scan/owner.rs`,
`camembert-core/src/scan/worker.rs`, `camembert-core/src/tree.rs`.

## 1. FIEMAP mechanics

- **The ioctl**: `FS_IOC_FIEMAP` (`_IOWR('f', 11, struct fiemap)`, value
  `0xC020660B`), a read-only query — confirmed by using it thousands of
  times below with zero mutation of any file. `struct fiemap` is 32 bytes
  (`fm_start, fm_length: u64`; `fm_flags, fm_mapped_extents, fm_extent_count,
  fm_reserved: u32`) followed by a flexible array of 56-byte
  `struct fiemap_extent` (`fe_logical, fe_physical, fe_length: u64`;
  `fe_reserved64[2]: u64×2`; `fe_flags: u32`; `fe_reserved[3]: u32×3`).
  Python's `fcntl.ioctl` mutable-buffer path caps at 1024 bytes, forcing a
  16-extent-per-call batch size in the scratch tooling
  (`~/.cache/camembert-freeable2-exp/fiemap.py`) — not a kernel limit, a
  throwaway-script limit; a real implementation sizes the buffer to taste
  and pages via `fm_start = last.fe_logical + last.fe_length` until the
  `FIEMAP_EXTENT_LAST` bit (`0x1`) appears, which the scratch tool does
  correctly after an initial bug (first pass silently truncated a 32-extent
  file to 16 — a live reminder that FIEMAP *always* needs the pagination
  loop, never a single fixed-size call, for any file that might exceed the
  buffer).
- **When `FIEMAP_EXTENT_SHARED` (`0x2000`) is set** — confirmed live for all
  three mechanisms named in the prompt:
  - **Reflink copy** (`cp --reflink=always`): both source and copy show
    `SHARED` immediately after the copy, no sync needed for the flag itself
    (though see delalloc note below for freshly-written data in general).
  - **Snapshot**: `btrfs subvolume create subvol1`, wrote a 4 MiB file,
    `btrfs subvolume snapshot subvol1 snap1` — both the live file and its
    snapshot copy show `SHARED`. **Subvolume create/snapshot do not need
    root** on this system (ran as `theo`); see §3 for what *does*.
  - **Same-file dedup** (`FIDEDUPERANGE`, the generic ioctl that superseded
    the btrfs-only `BTRFS_IOC_FILE_EXTENT_SAME` and that `duperemove`/`bees`
    use today — neither tool is installed on this machine, so this was
    driven directly via a hand-rolled ioctl call,
    `~/.cache/camembert-freeable2-exp/dedupe.py`): two files written
    independently with `cp --reflink=never` (confirmed *not* shared,
    distinct physical addresses beforehand), then deduped —
    `bytes_deduped=1048576 status=0`, and afterwards **both files report
    `SHARED` at the same physical address**, with no `cp --reflink` ever
    invoked. Confirms dedup is a real, independent trigger, not just a
    reflink alias.
- **When it clears**: overwriting one reflink sibling (COW) breaks sharing
  for that copy; once the *other* sibling becomes the sole referencer, its
  own `SHARED` bit clears too — reproduced with a subvolume/snapshot pair:
  after overwriting `subvol1/data.bin`, `snap1/data.bin` (now the only
  reference to the old extent) reported plain `LAST` with **no** `SHARED`.
  **Unlink timing**: reflinked a 262 KiB file, then `os.unlink()`'d the
  copy and polled `FIEMAP` on the original in a tight loop with no sleep —
  **`SHARED` cleared in 14.1 µs, on the first poll**, i.e. effectively
  synchronous with `unlink()` returning. **No wait for a transaction commit
  was observed** (btrfs's default commit interval is 30 s; nothing here
  waited anywhere near that) — extent-refcount bookkeeping for the
  sharing check is evidently visible in-memory (delayed-ref state) the
  instant the last reference drops, not gated on commit.
- **Delalloc / `FIEMAP_FLAG_SYNC`**: wrote 64 KiB, no `fsync`/`sync`,
  queried immediately without the flag — `fe_physical=0`, flags
  `LAST|UNKNOWN|DELALLOC` (`0x7`): the data is not yet allocated on disk,
  FIEMAP correctly says so rather than lying. With `FIEMAP_FLAG_SYNC` set,
  the **same unflushed file** immediately returned a real physical address
  (`LAST|ENCODED`, i.e. compressed — the default mount compresses on
  write). **Cost of `FIEMAP_FLAG_SYNC`, measured** (200 freshly-dirtied
  64 KiB files, single ioctl call each, no batching): **7.0 µs/call without
  the flag vs. 51.1 µs/call with it — ~7.3× more**, because the flag
  forces synchronous writeback of that file's dirty pages before mapping.
  For a scan walking files that may still have dirty pages (recently
  written, not yet synced), unconditional `FIEMAP_FLAG_SYNC` both costs
  more *and* has unbounded tail latency (writeback of a large dirty file
  could stall the call); without it, freshly-written data simply reports
  as unknown/delalloc rather than wrong.
- **Extent granularity**: btrfs's compression chunk size is 128 KiB — a
  16 MiB single-byte-repeated file split into ~127 extents of 128 KiB
  logical length each (`filefrag -v`), all `encoded`. The minimum extent
  is one filesystem block (4 KiB): a sparse file with a single 4 KiB
  write inside an 8 MiB nominal size produced exactly one 4 KiB extent at
  the correct logical offset, nothing for the rest (holes are absence of
  extents in FIEMAP's output, not a distinct flag — no `HOLE` bit exists
  in the current `fe_flags` set, see the flag table below).
- **Measured per-call cost** (native ioctl, Python overhead included;
  `_fiemap_once`/`fiemap` in the scratch tool, warmed up first):

  | File | Size | Extents | Cost |
  |---|---|---|---|
  | `C.bin` (random, incompressible) | 4 MiB | 1 | **6.4 µs** (full walk) |
  | `A.bin` (repeated byte, compressed) | 4 MiB | 32 | **46.4 µs** (full walk) |
  | `z.bin` (zeros, compressed, fragmented) | 100 MiB | 801 | **1208.8 µs** (full walk) |

  Roughly linear in extent count (~1.45–1.5 µs/extent, ~5–8 µs fixed
  per-call overhead in this Python harness — a native Rust caller avoids
  most of that fixed overhead, which is dominated by interpreter/struct-pack
  cost, not the syscall itself). **Extrapolation to a 100k-file scan**: most
  ordinary files are 1–3 extents, so ≈100k × (6–15 µs) ≈ 0.6–1.5 s of added
  wall time if run synchronously per file during the walk — noticeably more
  than phase 1's whole-system `/proc` sweep (23 ms total, `freeable.rs`
  header comment) because this cost is *per scanned file*, not per open fd.
  The distribution has a long, unpredictable tail: a single pathologically
  fragmented compressed file (the 801-extent case) costs as much as ~150
  average files by itself.

`fe_flags` bits observed live (from `linux/fiemap.h`, decoded in the scratch
tool): `LAST=0x1`, `UNKNOWN=0x2`, `DELALLOC=0x4`, `ENCODED=0x8` (compressed
or encrypted), `NOT_ALIGNED=0x100`, `DATA_INLINE=0x200`, `DATA_TAIL=0x400`,
`UNWRITTEN=0x800`, `MERGED=0x1000`, `SHARED=0x2000`.

## 2. What "freeable" means with shared extents, and the compression trap

- **The naive per-file model is correct only when nothing marked-shared is
  co-selected.** Per-file freeable = sum of extent lengths *not* flagged
  `SHARED`, because a shared extent frees nothing on this file's own
  deletion — the last referencer (possibly a snapshot never visible from
  the scanned tree) is what actually triggers reclaim. §3 below shows this
  naive model both under-counts (two mutually-shared siblings, deleting
  both frees everything, naive per-file sum says zero) and — per
  `freeable-attack-b.md`'s original example — over-counts under additive
  aggregation. Neither failure mode is rare; both are the direct subject of
  the attack report this research grounds.
- **What "shared with anyone" includes, confirmed live**: in-tree reflinks
  (§1), snapshots of a subvolume the scan may or may not also be walking
  (§1), and dedup (§1) all set the identical bit with no way to tell them
  apart from FIEMAP alone. A file can therefore be `SHARED` because of a
  snapshot the scan never visits (outside the scanned root, or excluded),
  and FIEMAP gives no hint of *that* — only that the extent is shared with
  *something*.
- **Compression breaks the honest-figure question in a way not anticipated
  by the phase-1 sparse-file precedent.** `st_blocks * 512` was correct for
  sparse files (phase 1, confirmed again here) but is **not** correct for
  compressed files:
  - Wrote 200 MiB of `/dev/zero` (a file, not a hole — `dd` writes real
    zero bytes, it does not create sparseness on its own). `df --output=avail
    -B1 /home` before/after showed a real disk delta of **only ~6.9–7.3 MB**
    (two independent trials on 100 MiB and 200 MiB zero writes both showed
    ~30:1 ratios, consistent with 128 KiB chunks each rounding up to one or a
    few 4 KiB compressed blocks). A same-size **incompressible** (`/dev/urandom`)
    file showed a `df` delta of **~100 MiB for 100 MiB written — no
    reduction**, as expected.
  - Despite that huge real difference, **`stat`'s `st_blocks` (and hence
    `du`, `ls -s`, and even `btrfs filesystem du`) reported the full
    logical size for *both* files** — `size=209715200 blocks=409600` (i.e.
    `st_blocks*512 == st_size`, no compression credit at all) for the
    all-zero file, identical to the random one. **`btrfs filesystem du -s`**
    likewise printed `200.00MiB Total / 200.00MiB Exclusive` for the
    all-zero file — the same figure it gives an incompressible file of the
    same size.
  - Cross-checked against `compsize`'s own documentation (not installed
    locally, fetched from its GitHub source and corroborated by a web
    summary): "*standard Unix tools cannot reflect compression because
    they report logical (uncompressed) sizes*" — `compsize` exists
    specifically because `du`/`ls`/`stat`/`btrfs fi du` all fail to surface
    real compressed disk usage. **Conclusion for the "which FIEMAP field is
    honest" question: neither.** `fe_length` is explicitly the *logical*
    extent length for a compressed (`ENCODED`) extent — `filefrag -v`
    visibly reports the same block-count span for `physical_offset` as for
    `logical_offset` on every compressed extent tested, which is
    misleading (it looks 1:1) rather than a real physical/compressed
    length; the kernel's `btrfs_fiemap` does not expose the on-disk
    (compressed) byte count through FIEMAP at all. Getting the true
    compressed footprint requires the extent tree walk in §3
    (`disk_num_bytes` from `TREE_SEARCH_V2`), which is the one thing
    `compsize` does that nothing else here does.
  - Practical implication: on a `compress=zstd` mount (this machine's
    default), `st_blocks`-based freeable figures — the exact mechanism
    phase 1 relies on for deleted-open files, and the naive fallback for
    phase 2's own unshared-bytes count — **silently overstate** freeable
    bytes for any compressed file, by whatever the compression ratio
    happens to be. This is a new, non-sparse-file failure mode phase 1's
    research did not have to consider (phase 1 never depends on compressed
    *file content* sizing beyond blocks already correctly attributed by the
    normal scan's own `disk` column, which itself inherits this same
    logical-vs-physical gap — worth noting that the tree's existing `real`/
    `disk` size column has presumably had this same blind spot since the
    scan engine was built, independent of freeable phase 2).
- **Sparse + compression interplay**: an 8 MiB nominal file with a single
  4 KiB write at offset 1 MiB reflinked cleanly — FIEMAP reported exactly
  one extent (the written 4 KiB range) as `SHARED` after the reflink; the
  (much larger) hole needed no handling, since FIEMAP simply never
  reported it. No interaction bug found; holes and compression are
  orthogonal in what FIEMAP exposes.

## 3. Alternatives to per-file FIEMAP, and the inclusion-exclusion question

- **`BTRFS_IOC_TREE_SEARCH_V2`** (`compsize`'s actual mechanism — fetched
  and read its source directly): walks `BTRFS_EXTENT_DATA_KEY` items for a
  given inode, reading `disk_num_bytes` (real on-disk/compressed length),
  `ram_bytes` (uncompressed logical length), `num_bytes`, and the
  compression type; a `seen_extents` radix tree keyed by
  `disk_bytenr >> 12` distinguishes exclusive (first reference) from
  shared (later references) bytes. **Confirmed root-only, live, on this
  machine**: a hand-built ioctl call (struct layout independently verified
  against `/usr/include/linux/btrfs.h` via a compiled `sizeof`/`offsetof`
  probe — `btrfs_ioctl_search_key` is 104 bytes, not the 88 a naive read of
  the header comments would suggest, because `unused1..4` are `u64` not
  `u32`) against our own file, opened via our own directory fd, as the
  owning unprivileged user, returned `PermissionError(1, 'Operation not
  permitted')`.
- **`BTRFS_IOC_LOGICAL_INO`** (the "who else references this extent"
  query — exactly the primitive inclusion-exclusion needs): tested via
  `btrfs inspect-internal logical-resolve -P <addr> .` (the CLI wrapper),
  using a physical address obtained from our own unprivileged FIEMAP call
  on a live reflink pair. Result: **`ERROR: logical ino ioctl: Operation
  not permitted`**, same as the raw ioctl. So the one API that would answer
  "shared with whom" directly is unavailable without root on this system.
- **`btrfs subvolume show`/`delete`, even on a subvolume the calling user
  created and owns, also failed** (`Could not search B-tree: Operation not
  permitted`; `deletion failed with EPERM`) — the two scratch subvolumes
  created for the snapshot test in §1 could not be cleaned up afterward.
  This is a stronger restriction than the commonly cited "unprivileged
  subvolume delete works since kernel 4.18 for the owner" — either a
  CachyOS-specific hardening or a kernel-config difference from upstream
  defaults; **not independently traced to a specific sysctl or LSM policy
  this pass** (checked `unprivileged_userns_clone`, no relevant
  `user_subvol_rm_allowed`-style knob found under `/proc/sys` or `sysctl -a`
  grep). Flagged as observed-not-explained.
- **The good news: FIEMAP alone, unprivileged, is enough for
  inclusion-exclusion *restricted to files the scan itself has already
  visited*.** This is the load-bearing finding for `freeable-attack-b.md`'s
  central problem. Concrete experiment, exactly as requested: created
  `A.bin` (4 MiB, incompressible-ish repeated byte, so extent boundaries
  are stable) and reflinked it to `B.bin` in the same directory. Naive
  per-file "sum of non-shared bytes": **`A.bin` → 0, `B.bin` → 0** (every
  extent in both is 100% shared — a purely additive-or-naive model claims
  deleting either, or both, frees nothing). **True freeable if both are
  deleted** (computed as the union of distinct `(physical, length)` extent
  pairs across every file in the set — i.e., correlating FIEMAP's own
  `fe_physical` field across sibling files, no LOGICAL_INO involved):
  **4,194,304 bytes — the full logical size**, because once both
  in-scope referencers are gone, nothing else (visible to this
  computation) still points at those extents. **This is precisely what
  `btrfs filesystem du -s` itself does, unprivileged, confirmed live**:
  given `pair/X.bin` and `pair/Y.bin` (1 MiB, mutually reflinked)
  individually, each reports `Exclusive 0 / Set shared 1.00MiB`; given the
  **directory** `pair` as one argument, it reports `Total 2.00MiB /
  Exclusive 0.00B / Set shared 1.00MiB` — the naive sum would be 2 MiB of
  "shared," but the tool correctly collapses it to the true 1 MiB of
  distinct shared bytes referenced by the set. The official documentation
  (`btrfs-filesystem(8)`, fetched from `btrfs.readthedocs.io`) says this
  outright: *"du** calculates disk usage... using FIEMAP... **set shared**
  takes into account overlapping shared extents, hence it isn't as simple
  as adding up shared extents."* — an independent, authoritative
  confirmation that (a) the non-additivity problem is real and
  well-known enough that btrfs's own tooling documents it explicitly, and
  (b) it is solvable **without root**, by extent-address correlation over
  a *known, already-FIEMAP'd set of files*, which is exactly what a
  camembert scan already has (every scanned file, if phase 2 FIEMAPs each
  one). The boundary this cannot cross: sharing with anything the scan
  did **not** FIEMAP (an excluded subtree, a snapshot elsewhere, another
  subvolume) stays invisible — the SHARED bit will still be set, but there
  is no address to correlate against, so it must fall into an
  "unattributable, possibly shared outside the selection" bucket rather
  than being claimed as freeable *or* silently dropped.
- **`BTRFS_IOC_INO_LOOKUP`** (dirid → path resolution, used by `subvolume
  list` and friends): not independently tested this pass (no reachable
  subvolume tree once the search ioctls above proved root-gated); same
  ioctl family as `TREE_SEARCH_V2`/`LOGICAL_INO`, so treated as
  likely-root-only by extension, not confirmed live.
- **Cost comparison**: `btrfs filesystem du -s` recursively over
  `~/.cache` (190,106 files, `find -type f | wc -l`) took **2.0–2.6 s
  wall** across two runs (`0.39s user + 1.6–1.75s system`), vs. plain
  `du -sh` over the identical tree at **0.36 s** — **roughly 6–7×
  slower**, consistent with FIEMAP calls (with their per-extent backref
  check for the `SHARED` bit) costing meaningfully more than a bare
  `stat`. Extrapolated per-file: **≈13.5 µs/file** for `btrfs fi du` vs.
  **≈1.9 µs/file** for `du`, in the same ballpark as the direct FIEMAP
  timings in §1.

## 4. Hardlink siblings

- **What the scan already tracks, read directly from source**:
  `camembert-core/src/scan/owner.rs` populates `hardlink_links: Vec<HardlinkLink>`
  for every `nlink > 1` non-directory entry, and each `HardlinkLink`
  (`camembert-core/src/scan/hardlink.rs:24`) carries `node`, `dev`, `ino`,
  and **`nlink`** — the raw `st_nlink` from `stat`, a whole-filesystem
  fact independent of what the scan walked. `hardlink.rs::reattribute`
  already groups these by `(dev, ino)` into `Vec<NodeId>` per inode
  (`groups: FxHashMap<(u64, u64), Vec<NodeId>>`) for the existing
  first-seen → canonical-owner re-attribution. **The exact data phase 2
  needs already exists**: for a given `(dev, ino)` group, `group.len()` is
  "links this scan actually found," and `links[0].nlink` (identical across
  the group, since `nlink` is a property of the inode, not the link) is
  "links that exist anywhere on that filesystem." **`group.len() ==
  nlink` ⇒ every link is inside the scanned tree**; `group.len() < nlink`
  ⇒ at least one sibling lives outside what was scanned (a different
  subtree not walked, an excluded path, or genuinely outside the scan
  root) and cannot be accounted for.
- **Verified live** with a minimal reproduction: `ln selection/f1
  outside/f2` (`dev=58, ino=2264066, nlink=2` from either path). If a scan
  root were `selection/` alone, the registry would see exactly one link
  (`group.len()==1`) while `st_nlink==2` — correctly signalling "a sibling
  exists that this scan cannot see," without needing to know *where* it
  is.
- **Per-entry freeable semantics — and why this is non-additive too, same
  root cause as §3.** "All links inside the *entry/selection*" is not the
  same test as "all links inside the *scan*." A marked directory `D`
  might contain only a subset of a hardlink group even when the scan (as
  a whole) saw every link — e.g., group has 2 links, one under `D`, one
  elsewhere in the same scanned tree but outside `D`. Deleting `D` alone
  frees 0 (the other link keeps the inode alive), which the *scan's*
  `group.len() == nlink` check alone would not catch — that check answers
  "does the whole scan account for every link," not "does this specific
  selection." A correct per-entry (or per-selection) answer needs, for
  each hardlink group, the *subset* of `group`'s nodes that fall under the
  candidate selection, compared against the *full* group (not against
  `nlink`) for "does deleting exactly this selection remove every link the
  scan knows about" — and even that is only sound when `group.len() ==
  nlink` (no unseen siblings) to begin with. This mirrors §3's
  extent-inclusion-exclusion problem almost exactly: a scalar precomputed
  per directory cannot express it, because whether a given link "counts"
  depends on which other nodes are co-selected, not on the node alone.
  `tree.rs`'s existing `HARDLINK_EXTRA` flag and the deletion dialog's
  "frees nothing unless last link" warning already model the single-file
  case; phase 2 generalizes it to arbitrary selections.
- **Partial semantics**: no partial-freeing case exists for a single
  inode's data (POSIX unlink either drops the link count to 0, freeing
  everything, or leaves the inode alive, freeing nothing) — unlike shared
  extents, hardlinks are all-or-nothing per inode. The only "partial"
  aspect is at the *selection* level: some inodes in a marked subtree may
  qualify (all links inside), others may not, and the sum over qualifying
  ones is additive **within a single evaluation of one fixed selection**
  — the non-additivity is about comparing *different* selections against
  each other or trying to precompute a reusable per-directory number, not
  about summing within one.

## 5. Other filesystems

- **XFS**: kernel reflink support (`FIEMAP_EXTENT_SHARED`-capable, via the
  same generic `FS_IOC_FIEMAP`/`FIDEDUPERANGE` ioctls) landed in the
  4.9-rc1 merge window (web search: LKML pull request thread, Oracle Linux
  and "The Ongoing Struggle" blog corroborate). `cp --reflink` on XFS
  works the same way as btrfs from userspace, and coreutils ≥ 9 defaults
  to `--reflink=auto` so a plain `cp` silently reflinks when the target
  filesystem supports it (this bit us once during testing — an initial
  same-content-file dedup experiment on btrfs showed `SHARED` *before* the
  dedup ioctl ran at all, because the plain `cp` had already auto-reflinked
  the copy; had to force `--reflink=never` to get genuinely independent
  extents first). **Not reproduced live**: creating an XFS filesystem
  requires ≥300 MiB (`mkfs.xfs` refused a smaller image) and mounting it
  needs a loop device; both a plain `mount -o loop` and an
  `unshare --user --map-root-user --mount` attempt (to get a rootless loop
  mount) failed — the latter with `mount failed: Permission denied`,
  consistent with loop-device attachment needing real host-level
  `CAP_SYS_ADMIN` that a user namespace's mapped-root does not confer.
  Documented from sources only, as anticipated in the task brief.
- **ZFS**: historically no per-file API and no reflink at all. **This has
  changed**: OpenZFS 2.2 (2023) added Block Cloning via a pool-level Block
  Reference Table (BRT) (Klara Systems article; Hacker News thread on the
  2.2.0 RC merge), gating `cp --reflink` behind an *experimental*,
  off-by-default tunable (`zfs_bclone_enabled`). Critically — confirmed via
  an OpenZFS GitHub discussion thread (`openzfs/zfs#16024`) — **Block
  Cloning is tracked per-pool/per-vdev, "knows nothing about datasets," and
  it is "impossible to provide per-dataset statistics for Block Cloning."**
  If per-dataset accounting is infeasible even for ZFS's own tooling,
  per-file accounting is a fortiori not exposed. **The original "ZFS: show
  nothing rather than invent" stance is confirmed, and now on stronger
  grounds than "ZFS has no reflinks"** — it now does, but still offers no
  API a per-file tool could query. No live ZFS system was available to
  test.
- **ext4**: no reflink support, confirmed absent from every source found;
  phase 2 there really is hardlinks only, exactly as the task brief
  assumed. Not separately re-derived beyond confirming no contrary
  evidence turned up.
- **Detection mechanism, and reuse of existing scan code**: `statfs`'s
  `f_type` magic is exactly what `camembert-core/src/scan/worker.rs`
  already uses for kernfs exclusion (`classify_mount`, `fstatfs` at mount
  boundaries only — "per-mount, not per-dir" cost, per its own comment).
  The same call site can add real-filesystem branches. Verified against
  `/usr/include/linux/magic.h` via a compiled probe (not just documentation):
  **`BTRFS_SUPER_MAGIC = 0x9123683e`** (also independently confirmed via
  `filefrag`'s own `Filesystem type is: 9123683e` output on this machine's
  live btrfs mount), **`XFS_SUPER_MAGIC = 0x58465342`** (ASCII `"XFSB"`).
  **`ZFS_SUPER_MAGIC` is not defined in this system's `linux/magic.h` at
  all** (ZFS-on-Linux is out-of-tree); web sources (gopsutil's disk package,
  multiple independent listings) consistently give **`0x2fc12fc1`** as the
  stable value ZFS reports via `statfs`, but this was not independently
  confirmed against a live ZFS mount. **`EXT4_SUPER_MAGIC = 0xef53`** —
  worth noting this magic is shared verbatim with ext2 and ext3; `statfs`
  alone cannot distinguish "ext4" from "ext2/3," only "some ext2 family
  filesystem" (matches long-standing, well-known kernel behavior, not
  something this pass discovered).

## 6. Prior art

- **`btrfs filesystem du`** (`btrfs-progs`, this machine's v7.1): per its
  own documentation (`btrfs.readthedocs.io`, fetched directly) and
  confirmed live throughout §2–3, it (a) uses FIEMAP, not the extent-tree
  walk, (b) works fine unprivileged, (c) correctly deduplicates
  shared-with-set-members across multiple files/a whole directory argument
  (§3), and (d) is compression-blind — reports full logical bytes for both
  a 100%-compressible and a 0%-compressible file of the same nominal size
  (§2). It is the closest existing prior art to "phase 2, unprivileged,"
  and its own docs candidly admit the non-additivity problem this research
  was tasked with grounding.
- **`compsize`** (source fetched from GitHub, not installed locally): the
  one tool here that gets the *real* compressed byte count right, because
  it walks `BTRFS_EXTENT_DATA_KEY` items directly via
  `BTRFS_IOC_TREE_SEARCH_V2` and reads `disk_num_bytes`. Confirmed live
  that this ioctl family requires privilege this machine's normal user
  does not have (§3) — so whatever `compsize` can see, an unprivileged
  camembert cannot, without either dropping the compression-accurate
  figure or requiring root.
- **`qdirstat`**: its own documentation
  (`doc/Btrfs-Free-Size.md`, fetched from GitHub) describes the exact
  `df`-lies-about-free-space problem this whole feature exists to fix, and
  explicitly says it does **not** shell out to `btrfs fi usage`/`df`/`show`
  internally, in part *because* those need root — i.e., prior art
  confirming the same privilege wall found independently in §3, and a
  real tool's decision to punt rather than cross it. Separately, qdirstat
  has a "config option to ignore hard links" and once "takes allocated
  size into account to close the gap between sizes reported by QDirStat
  and `du`" (GitHub issue/changelog search) — no evidence found of it
  attempting shared-extent-aware freeable accounting.
- **`dua`, `gdu`, `pdu`, `WizTree`, `filelight`, `WinDirStat`**: no
  evidence found (web search) of any of these attempting btrfs
  shared-extent or cross-selection hardlink freeable accounting — same
  "open niche" conclusion as phase 1's research reached for deleted-open
  files.

## 7. Staleness / lifecycle

- **Extent sharing is volatile in a strictly stronger sense than phase
  1's `/proc` state.** Phase 1's ledger goes stale only when a process
  opens or closes a file (bounded by process activity on this machine).
  Extent sharing can change from **any write anywhere on the filesystem**
  by **any process**, including ones camembert has no visibility into at
  all (no `/proc`-style enumeration exists for "who else has a reflink to
  this extent" short of the root-only `LOGICAL_INO`, §3) — a snapshot taken
  by a backup job, another user's `cp --reflink`, a scheduled `duperemove`
  run, or btrfs's own balance/defrag operations can all flip `SHARED`
  without camembert's scan doing anything. §1 showed the flip itself is
  near-instantaneous (14.1 µs) once triggered — the uncertainty is entirely
  in *when* an external trigger happens, not in how fast the kernel
  reflects it.
- Given §1's cost measurements (0.6–1.5 s extrapolated for a 100k-file scan,
  with a long fragmentation-driven tail) and §3's finding that
  cross-file correlation needs the *whole* candidate set's FIEMAP data
  gathered together (it is not a per-file, cacheable scalar — the answer
  depends on which other files are co-selected), a per-scan eager sweep
  (phase 1's model) and a lazy, on-demand, per-selection recomputation are
  both live options with real, measured costs on each side; this research
  does not adjudicate between them (see open questions).

## Open design questions

Not decisions — flagging for the actual design pass:

1. **Inclusion-exclusion scoping**: §3 shows FIEMAP-based physical-address
   correlation gives correct, unprivileged, root-free inclusion-exclusion
   *restricted to files the scan has itself visited*. Is that restricted
   correctness ("freeable if you delete this selection, accounting for
   everything this scan can see, but blind to snapshots/excluded
   subtrees/other subvolumes it can't") honest enough to display as a
   number, or does the invisible remainder need its own explicit "possibly
   also shared with things outside this scan" caveat attached to every
   figure this mechanism produces? `btrfs fi du`'s own docs don't surface
   this caveat to its users at all.
2. **Lazy vs. eager evaluation**: unlike phase 1's single whole-scan sweep,
   a phase-2 shared-extent figure is a function of *which nodes are
   co-selected* (§3, §4), which is naturally a "recompute for this
   selection, on demand" shape rather than a "precompute once, look up
   forever" shape. Eager (FIEMAP every scanned file during/after the scan,
   build the full extent→referencer map once) costs the §1/§3 numbers
   up front but makes every subsequent selection query free; lazy
   (FIEMAP only the files under whatever the user currently has selected)
   costs ~nothing until the user asks, but repeats work every time the
   selection changes and cannot warn about non-additivity before the user
   looks. Which better fits a tool whose core promise is "instant
   navigation, numbers settle in behind you"?
3. **Compression's logical/physical gap (§2) is bigger than phase 2**:
   `st_blocks`/`du`/`btrfs fi du` all overstate freeable-by-deletion on any
   compressed file, by up to the compression ratio (30:1+ observed for
   trivially-compressible data). This affects the *existing* `real`/`disk`
   size column too, not just the new freeable one — is fixing it in scope
   for phase 2 at all, or a separate, pre-existing gap to name and defer?
4. **The privilege wall is total, not partial**: every btrfs-specific API
   that would give a *precise* answer (`TREE_SEARCH_V2` for real
   compressed bytes and "who shares this," `LOGICAL_INO` for extent
   referencer lists, even `subvolume show`/`delete` on one's own
   subvolume) needed root on this machine. Only bare FIEMAP (SHARED bit +
   physical address, both usable unprivileged) is available to a normal
   user. Is a root-only "precise mode" (real compressed bytes, true
   cross-snapshot referencer counts) worth designing as an opt-in
   privilege escalation, mirroring phase 1's D6/§4 treatment of `/proc`
   coverage gaps, or is FIEMAP-only considered sufficient for all
   non-root runs and precision left off the table entirely?
5. **Hardlink per-selection semantics (§4)**: the scan-wide `group.len() ==
   nlink` check (already buildable from existing `HardlinkLink` data) only
   answers "does the whole scan know every link of this inode," not "does
   this specific marked selection contain every link the scan knows
   about." Surfacing a correct per-entry number needs a selection-time
   subset check against each hardlink group, not a scan-time scalar — is
   that computed the same way as (and at the same time as) the shared-
   extent per-selection pass, given both are structurally the same
   problem?
6. **CachyOS's stricter-than-documented subvolume permissions (§3)**: this
   machine denied `btrfs subvolume show`/`delete` on a self-owned
   subvolume, which several sources describe as unprivileged-safe since
   kernel 4.18. If this is a distro-wide hardening rather than an
   anomaly of this one box, it affects how confidently camembert can
   assume *any* unprivileged btrfs introspection beyond bare FIEMAP will
   work across the user base it targets — worth a second data point on a
   different distro before relying on more than FIEMAP unconditionally.
