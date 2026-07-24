# Traversal dedup — dossier (2026-07-24)

Decision-ready dossier for the traversal-dedup work listed in
[HANDOFF.md](../../HANDOFF.md) step 9 and promised by the
[scan-tree addendum](scan-tree-decisions.md). **Not settled** — this is
research, options, an adversarial attack on those options, and one
recommendation, for a co-design session. Nothing here is binding until
it lands in a decisions doc.

All measurements and syscall probes below were taken on the dev machine
(`uname -r` → `7.1.4-1-cachyos`, btrfs root with a flat `@`-subvolume
layout, snapper `.snapshots`, live containerd/Docker overlays). No
`mount`/`umount`/`sudo` was run: every observation is read-only.

---

## Problem

Since 2026-07-24 camembert crosses filesystem boundaries by default
(`--one-filesystem` is the opt-out). The boundary test is one line in
`camembert-core/src/scan/worker.rs`:

```rust
if stat.dev != ctx.dev { /* mount point: classify, then decide */ }
```

`st_dev` tells you *that* two paths are on different filesystems. It
cannot tell you *why* two paths are on the **same** one, nor whether two
different `st_dev` values name data you already counted. Three accepted
caveats follow (README "Honest numbers", `--one-filesystem` help text,
scan-tree addendum). They are three different problems wearing one
label, and the first one is not the same shape as the other two.

### Case 1 — btrfs snapshot subvolumes (and any nested subvolume)

Reproduced on this machine, no root needed:

```
$ stat -c 'dev=%d ino=%i %n' / /.snapshots
dev=36 ino=256 /
dev=79 ino=256 /.snapshots

$ grep -c snapshots /proc/self/mountinfo
0
```

`/.snapshots` has a **different `st_dev`** from `/` (79 vs 36) and does
**not appear in `/proc/self/mountinfo`** — it is a nested subvolume
reached by walking, not a mount. Today's code sees `stat.dev != ctx.dev`,
calls `classify_mount`, gets btrfs magic (not kernfs), and descends. On a
snapper root with N kept snapshots, the same file content is walked N+1
times.

(On *this* machine `/.snapshots` is `drwxr-x--- root:root`, so an
unprivileged scan gets `EACCES` and the damage is masked by permissions.
That is luck, not design — the openSUSE/`@`-layout default is readable
in plenty of setups, and `/home/.snapshots`-style per-subvolume snapshots
frequently are.)

Note what a `(dev, ino)` seen-set would do here: **nothing**. The
snapshot's root inode is `256`, same as `/`'s, but its `dev` is 79 — a
different key. Every file inside likewise carries a different `dev`. The
seen-set never fires. This case is not a traversal-identity problem at
all; it is an **extent-sharing** problem, and camembert already owns the
machinery for it (freeable phase 2's FIEMAP exclusive floor).

### Case 2 — bind mounts within the same filesystem

Creating one requires root, so per the working constraint it was not
attempted. The mechanism is documented and the *detector* is verifiable
here without creating anything (see Case 3's mountinfo evidence and the
`STATX_ATTR_MOUNT_ROOT` probe in Research §3): a same-filesystem bind
mount keeps its source's `st_dev` and `st_ino`, so **neither the current
check nor `--one-filesystem` can see it**, and the subtree is walked
twice — `nlink == 1` files and every directory double-counted, since the
hardlink registry only tracks `nlink > 1`.

`rsync`'s man page is the only mainstream tool that says this out loud:
*"rsync treats a 'bind' mount to the same device as being on the same
filesystem"* (`man rsync`, `--one-file-system`).

### Case 3 — the same filesystem mounted at several paths

This machine has seven of them:

```
$ awk '{print $1, $2, $3, $4, $5}' /proc/self/mountinfo
40  1  0:33 /@      /
57  40 0:33 /@root  /root
58  40 0:33 /@srv   /srv
70  40 0:33 /@tmp   /var/tmp
65  40 0:33 /@cache /var/cache
215 40 0:33 /@log   /var/log
59  40 0:33 /@home  /home
```

All seven share `major:minor` `0:33`. Field 4 (the *root of the mount
within the filesystem*) differs for each — `/@`, `/@root`, `/@srv`, … —
which is exactly why they are **not** duplicates. Change any two of those
field-4 values to be equal (or prefix-contained) and you have the
double-count. This is the field a bind mount or a second mount of the
same subtree would expose, and it is unavailable from `st_dev` alone.

### Case 4 (NEW) — overlayfs lower/upper aliasing

Not in the accepted-caveat list. Present and live on this machine:

```
792 40 0:85 / /var/lib/docker/rootfs/overlayfs/9604… rw,relatime shared:727 -
  overlay overlay rw,lowerdir=/var/lib/containerd/…/snapshots/1678/fs:…,
  upperdir=/var/lib/containerd/…/snapshots/1679/fs,workdir=…,index=off
```

The overlay mount lives at `/var/lib/docker/rootfs/overlayfs/<hash>` with
its own anonymous device (`0:85` and `0:86` for the two live mounts
here). Its `lowerdir`/`upperdir` are **ordinary directories under
`/var/lib/containerd/…`, on the root btrfs subvolume (`0:36`)**. Both are
inside any scan of `/` or `/var`. A crossing scan walks the same bytes
twice — once as the merged overlay, once as the backing layers.

No `(dev, ino)` test catches this: per
`Documentation/filesystems/overlayfs.rst`, an overlay object may report
"an `st_dev` from the lower filesystem or upper filesystem", `st_ino`
"will only be unique when combined with `st_dev`", and both "can change
over the lifetime of a non-directory object" (copy-up). The only reliable
signal is the `lowerdir=`/`upperdir=` super-options string in mountinfo.

### Case 5 (NEW) — ZFS `.zfs/snapshot`, which the scan itself materialises

`.zfs/snapshot/<name>` entries are **automount points**. `snapdir=hidden`
(the default) only hides `.zfs` from `readdir`; it does not block the
automount. camembert calls `statx`/`statat` with
`AtFlags::SYMLINK_NOFOLLOW` and **nothing else** (`worker.rs:590`,
`worker.rs:599`, `uring.rs:146`) — `AT_NO_AUTOMOUNT` is not set. Under
`snapdir=visible`, walking `.zfs/snapshot` therefore *triggers* the
mounts and then descends into every one of them: the whole dataset,
once per snapshot. Any dedup plan precomputed at scan start is stale by
construction here, because the traversal creates the mounts it was
supposed to plan for.

Not verified on a live pool (no ZFS on this box) — flagged as
needs-a-rig, not asserted.

### Case 6 (NEW, small) — a bind-mounted *file*

`mount --bind file1 file2` gives one inode two paths with `nlink == 1`.
The hardlink registry (`owner.rs`, `is_hardlink = kind != Dir && nlink > 1`)
skips it, so it counts twice. Minor in bytes, but it shows the
`nlink > 1` gate has a second blind spot beyond directories.

### What is *not* a case

**Symlink loops.** camembert never follows symlinks (`O_NOFOLLOW` below
the root, `SYMLINK_NOFOLLOW` on every stat). Unlike `find -L` (*"File
system loop detected"*) or `du -L`, camembert has no loop to detect. A
**bind loop** (`mount --bind /a /a/b`) is a genuine infinite recursion
for any walker and is a Case-2/3 instance, not a symlink one.

**Reflink / `cp --reflink` / `duperemove` copies.** Two distinct inodes
sharing every extent. No traversal mechanism can see this; the FIEMAP
exclusive floor already does. Explicitly out of scope, and the name
"traversal-dedup" must not be allowed to imply otherwise.

---

## Research

### 1. What other tools actually do

| Tool | Dedups | Silently double-counts | Documented promise |
|---|---|---|---|
| GNU `du` 9.11 | hardlinked files always; **directories only if `hash_all`** | bind mounts, multi-mounts, snapshots | *"If two or more hard links point to the same file, only one of the hard links is counted."* (`info coreutils 'du invocation'`) |
| `rsync` 3.4.4 | nothing dev/ino-based | bind mounts, admitted | *"rsync treats a 'bind' mount to the same device as being on the same filesystem"* (`man rsync`, `-x`) |
| GNU `tar` 1.35 | nothing | everything above | *"Stay in local file system when creating archive."* (`man tar`) |
| GNU `find` 4.11 | nothing (but loud `-L` loop detection) | everything above | *"Ignore files on other devices"* (`man find`, `-xdev`) |
| `ncdu` 2.9.2 | hardlinked files (`H` indicator, `--shared-column`) | directory hardlinks/firmlinks, admitted | BUGS: *"Directory hard links and firmlinks (MacOS) are not supported… will thus get scanned and counted multiple times."* |
| `gdu` 5.36.1 | hardlinks | bind mounts (by omission) | *"Hard links are counted only once."* (README) |
| `dust` 1.2.4 | hardlinks | bind mounts (by omission) | *"Dust will not count hard links multiple times"* (README) |
| `diskus` 0.9.0 | hardlinks on Unix; **no `-x` flag at all** | junctions on Windows, admitted | *"On Windows, diskus doesn't respect hardlinks or junction points… counting such entries multiple times."* |
| `btrfs fi du` 7.1 | **shared extents**, incl. overlapping | (different problem) | *"report a count of total bytes, and exclusive (not shared) bytes… a 'set shared' value… takes into account overlapping shared extents"* (`man btrfs-filesystem`) |

The load-bearing finding is in GNU du's own source
(`coreutils/src/du.c`):

```c
/* Hash all dev,ino pairs if there are multiple arguments, or if
   following non-command-line symlinks, because in either case a
   file with just one hard link might be seen more than once.  */
hash_all = (optind + 1 < argc || symlink_deref_bits == FTS_LOGICAL);
…
&& (hash_all || (! S_ISDIR (sb->st_mode) && 1 < sb->st_nlink))
&& ! hash_ins (di_files, sb->st_ino, sb->st_dev)
```

du **has** the directory `(dev, ino)` seen-set. It refuses to switch it
on for the ordinary single-root recursive scan — the exact shape of
every camembert invocation. So: the state of the art is that hardlink
dedup is universal, `-x`-family flags are universally parent-vs-child
device comparisons that no tool claims stop at a bind mount, general
directory identity dedup is unsolved everywhere, and extent-sharing is
attempted by exactly one tool, for one filesystem.

There is a real gap. Whether it is the gap worth closing is the
Displacement question at the end.

### 2. `/proc/self/mountinfo` — and the trap in its own man page

`man 5 proc_pid_mountinfo` (man-pages 6.18) documents field 3 as
*"major:minor — the value of `st_dev` for files on this filesystem (see
`stat(2)`)"*.

**That is false on btrfs, verified here.** All seven btrfs mounts report
`0:33`, while `stat` reports `st_dev` = 36, 53, 54, 55, 56, 57, 58 for
`/`, `/root`, `/srv`, `/var/cache`, `/var/tmp`, `/var/log`, `/home`.
btrfs allocates an anonymous block device per *subvolume*, and (per
Corbet, ["The Btrfs inode-number epic"](https://lwn.net/Articles/866582/))
"these numbers do not show up in files like /proc/self/mountinfo even if
subvolumes are explicitly mounted".

**Any implementation that matches mountinfo's `major:minor` against
`st_dev` is a silent no-op on the most common modern Linux root
filesystem.** The mount plan must be keyed by `stat`-ing each mount
point, not by parsing its device numbers.

`/proc/self/mounts` and `/etc/mtab` (byte-identical here) carry neither
field 4 nor any device field, so they are unusable for this. Optional
fields carry propagation tags (`shared:N` on every mount here,
`master:N` absent) — informative for `--rbind` peer groups, not needed
for the dedup decision itself.

`camembert-core/src/scan/media.rs` already has a fixture-tested
mountinfo parser with correct octal unescaping; it deliberately discards
fields 1–4. Extending it is a small, well-precedented change.

### 3. `statx` — the primitives that actually answer this

`man 2 statx` on this machine (man-pages 6.18) documents all of the
following, and a compiled probe confirmed each one live:

```
path         dev    ino  mnt_id      MNT_ID_UNIQUE SUBVOL subvol MOUNT_ROOT
/            0:36   256  2147483744  yes           yes    256    yes
/home        0:58   256  2147484102  yes           yes    257    yes
/srv         0:54   256  2147484078  yes           yes    259    yes
/var/log     0:57   256  2147484097  yes           yes    262    yes
/.snapshots  0:79   256  2147483744  yes           yes    265    no
/usr         0:36   269  2147483744  yes           yes    256    no
/usr/bin     0:36   368  2147483744  yes           yes    256    no
/boot        259:1  1    2147484107  yes           no     0      yes
/tmp         0:52   1    2147484066  yes           no     0      yes
```

Read that table carefully — it is the whole design:

- **`STATX_ATTR_MOUNT_ROOT`** (Linux 5.8, `stx_attributes`, no mask bit
  required) is `yes` for exactly the mount points and `no` for `/usr`
  and for `/.snapshots`. It is a **VFS-level** property: it is set on a
  same-filesystem bind mount too, where `st_dev` is silent. This is the
  detector Case 2 needs.
- **`stx_subvol`** (`STATX_SUBVOL`, Linux 6.10, "supported by bcachefs
  and btrfs") gives 256 for `/` *and* `/usr` (same subvolume), 265 for
  `/.snapshots`. It changes at a subvolume boundary **whether or not the
  subvolume is separately mounted** — the nested `/.snapshots` case that
  mountinfo cannot see. And it matches each mount's `subvolid=` super
  option exactly.
- **`stx_mnt_id`** is identical (`2147483744`) for `/`, `/usr` and
  `/.snapshots`: same mount, three different subvolumes. Mount identity
  and subvolume identity answer different questions; the table shows
  both are needed.
- `stx_dev_major/minor` ≡ `st_dev`, confirmed for every path.
- Requesting `STATX_MNT_ID | STATX_MNT_ID_UNIQUE` returns only the
  `_UNIQUE` bit in `stx_mask` (`got_mnt_id=0, got_uniq=1` in the probe).
  A reader must accept either bit. `STATX_MNT_ID_UNIQUE` (Linux 6.8)
  exists because plain mount IDs are recycled after umount.

Forward/backward compatibility, probed:

```
unknown mask bit 0x400000: ret=0  stx_mask=0x9fff  (bit cleared)
STATX__RESERVED (0x80000000):     ret=-1 errno=22 (EINVAL)
```

Unknown mask bits are safely ignored and cleared from `stx_mask`, so a
6.10 mask on a 5.15 kernel degrades by itself, self-describingly.
`stx_attributes_mask` says whether `MOUNT_ROOT` is meaningful. Only
`STATX__RESERVED` is fatal.

`rustix` 1.1.4 exposes `stx_mnt_id` and `stx_subvol` as public fields on
`rustix::fs::Statx`, and `StatxAttributes::MOUNT_ROOT`, but its
`StatxFlags` stops at `MNT_ID` — `MNT_ID_UNIQUE` (0x4000) and `SUBVOL`
(0x8000) need `StatxFlags::from_bits_retain`. Small seam, worth naming.

Cost of widening the mask, measured on `/usr/bin` (~2 000 entries × 200
reps, quietest of three runs):

```
base            982 ns/call
base+MNT_ID_UNQ 985 ns/call
base+SUBVOL     995 ns/call
base+both      1000 ns/call
base           1002 ns/call (recheck)
```

Within noise (≤2 %) on this machine, warm. Not a licence to skip
`scripts/bench-compare.sh` — see the Attack.

`AT_NO_AUTOMOUNT` exists precisely to stop a scanner materialising
automounts (Case 5). camembert does not currently pass it.

### 4. Filesystem magics, and what `statfs` cannot tell you

`/usr/include/linux/magic.h`: `BTRFS_SUPER_MAGIC 0x9123683E`,
`XFS_SUPER_MAGIC 0x58465342`, `EXT2_SUPER_MAGIC 0xEF53` (ext2/3/4 share
it — magic alone cannot tell them apart), `OVERLAYFS_SUPER_MAGIC
0x794c7630`, `TMPFS_MAGIC 0x01021994`, `NFS_SUPER_MAGIC 0x6969`,
`AUTOFS_SUPER_MAGIC 0x0187`, `FUSE_SUPER_MAGIC 0x65735546`,
`BCACHEFS_SUPER_MAGIC 0xca451a4e`. **ZFS is absent** (out-of-tree); its
`0x2FC12FC1` would have to be hardcoded, or detected from the mountinfo
fstype string.

`statfs` exposes **no** subvolume identifier: `f_type` is
`BTRFS_SUPER_MAGIC` for every subvolume and `f_fsid` is whole-filesystem.
`statfs` cannot do this job.

### 5. btrfs subvolume detection without root, ranked

- `st_ino == 256` — `BTRFS_FIRST_FREE_OBJECTID`, confirmed on every
  subvolume root here including the nested `/.snapshots`. Necessary,
  not sufficient: it flags "a boundary" without naming which subvolume.
- `BTRFS_IOC_GET_SUBVOL_INFO` / `BTRFS_IOC_SUBVOL_GETFLAGS` —
  **verified unprivileged** (UID 1000, no `CAP_SYS_ADMIN`): returned
  `treeid=256 name=@`, `treeid=257 name=@home`, `treeid=262 name=@log`,
  `flags=0`. Gives the read-only bit too.
- `BTRFS_IOC_TREE_SEARCH` / `_V2` — **root only**, verified: `EPERM`.
  (`btrfs subvolume list /` fails for the same reason.)
- `statx STATX_SUBVOL` — one syscall, unprivileged, filesystem-declared,
  covers bcachefs too. Strictly better than all of the above where the
  kernel is ≥ 6.10.

Could **not** verify the read-only flag on an actual snapshot:
`/.snapshots` is mode 0750 root:root here, so every probe stops at
`EACCES` before reaching an ioctl. Flagged as unverified.

### 6. NFS, overlayfs, and the non-Linux futures

**NFS.** One `st_dev` per client-side mount; the same export mounted
twice yields two different `st_dev` values, so `(dev, ino)` dedup fails
outright. `f_fsid` is server-policy-dependent and distinct filesystems
can collide on it (NFS-Ganesha wiki; linux-nfs list) — not a safe key.
The practical discriminator is the mountinfo source string
(`server:/export`), which is text, not a guarantee. Separately: **btrfs
exported over NFS collapses every subvolume to the root's device
number** (nfsd cannot present the anonymous devices), so the Case-1
signal disappears entirely over NFS.

**overlayfs.** Covered under Case 4. `xino` composes a persistent unique
`st_ino` from the real inode plus an fsid, but it is not universal and
does not make the overlay's identity equal its layer's. `lowerdir=` /
`upperdir=` in mountinfo super-options are the only reliable link.

**autofs.** Already excluded by magic (`0x0187`) in `KERNFS_MAGICS`;
what is *behind* an autofs trigger is not.

**macOS / APFS.** `(st_dev, st_ino)` works. Firmlinks join the read-only
System and writable Data volumes, and the two sides are **distinct APFS
volumes** with distinct `st_dev` — so a firmlink reads as an ordinary
cross-volume boundary and needs no special case. `getmntinfo(3)` has no
field-4 equivalent, so mountinfo-style bind detection does not port;
macOS has no Linux-style bind mounts to detect either.

**Windows.** `GetFileInformationByHandleEx(…, FileIdInfo, …)` returns
`FILE_ID_INFO` = 128-bit `FileId` + `VolumeSerialNumber` — the direct
analogue of `(dev, ino)`, so a seen-set ports with a wider key. Caveats
(Raymond Chen, *The Old New Thing*, 2022-01-27): the 64-bit id "is not
guaranteed to be unique on ReFS", `0xFFFF…FF` means "did not fit", zero
means unsupported, and ids are reused after deletion. Junctions and
mount points are `FILE_ATTRIBUTE_REPARSE_POINT` + `FSCTL_GET_REPARSE_POINT`,
which is a *better* `MOUNT_ROOT` than Linux's — it tells you the target
too. `diskus` documents that it double-counts junctions on Windows;
that is the bar.

### 7. Cost of a naive seen-set, measured

Directory share of a tree, measured here (`find -xdev`):

```
/usr                      27 608 dirs / 609 424 non-dirs  → 4.3 %
this repo (with target/)  10 303 dirs / 229 976 non-dirs  → 4.3 %
```

So the D4 target of 10 M entries is roughly **450 k directories** on
typical trees (source/dev trees run richer — freeable2's attack found
20–30 % on some; take 1–3 M as the pessimistic band).

`FxHashSet<(u64, u64)>` — the exact type the owner already uses for
hardlinks — measured by RSS delta on this machine:

```
n=  450 000  cap=  458 752  RSS +8.7 MB  20.3 B/entry  insert 25 ns  lookup 10 ns
n=1 000 000  cap=1 835 008  RSS + 34 MB  35.7 B/entry  insert 40 ns  lookup 19 ns
n=3 000 000  cap=3 670 016  RSS + 68 MB  23.8 B/entry  insert 34 ns  lookup 24 ns
```

(hashbrown: 16 B payload + 1 control byte per slot, power-of-two
capacity, 7/8 max load — so per-entry cost swings 20–36 B depending on
how far past a doubling `n` lands, and a rehash transiently holds both
tables.)

**Verdict: 9–34 MB against a 450 MB budget. The memory objection to the
naive seen-set is wrong, and this dossier will not pretend otherwise.**
The case against it is semantic (see Attack A), not budgetary.

By contrast, a mount-keyed structure is **36 entries** on this machine
and low thousands on a busy container host — free at any scale.

### 8. Where the check can live

`docs/design/scan-tree-decisions.md` D1: work-stealing workers, one
owner thread that is the sole arena writer. Reading the code:

- The **descend decision is worker-side** (`worker.rs:443-493`). The
  worker allocates `child_token`, bumps `pending_jobs`, and pushes the
  `Job` onto its local deque, where another worker can steal it
  immediately. Only afterwards does the batch reach the owner.
- The owner has **no way to cancel a queued job**. `WorkerShared` has a
  global `abort: AtomicBool` and nothing per-job. `integrate()` also
  asserts `child_dirs_seen == batch.child_dirs`.

**Therefore an owner-side check cannot prevent traversal.** It could
only suppress accounting after the duplicate subtree has already been
walked, paying full I/O and full arena memory for nodes it then has to
zero. Any option that actually saves work must decide on the worker,
before the `local.push(Job { … })`.

That is the constraint that separates the options: a worker-side check
against **mutable shared state** needs a lock or shards on the hottest
path in the program (the project's own scan-tree research prices a
contended mutex at ~125 ns vs ~7 ns uncontended); a worker-side check
against **immutable precomputed state** costs a hash lookup and nothing
else.

`classify_mount` already pays an `openat` + `fstatfs` at every
`st_dev` change — an existing, per-mount, off-the-hot-path hook.

---

## Options

### Option A — universal directory `(dev, ino)` seen-set, first-visit wins

**Mechanism.** Workers share a set of every directory's `(st_dev,
st_ino)`. Before pushing a child job, insert; if already present, skip
the descent and flag the entry. Sharded (16–64 shards by hash) or behind
a `Mutex`.

**Fixes.** Cases 2, 3 and 6 (extend to files and it also catches the
bind-mounted file, at O(all entries) memory). Any future same-inode
aliasing, including directory hardlinks on filesystems that allow them.
Bind loops terminate.

**Misses.** **Case 1 entirely** — a snapshot subvolume's objects have a
different `st_dev`, so the key never matches (proven above: `/` dev 36,
`/.snapshots` dev 79). Case 4 (overlay identity is not layer identity).
Case 5 (mounts created mid-scan get fresh, unseen identities). NFS
same-export-twice.

**Cost.** Measured: 8.7 MB @ 450 k dirs, 34 MB @ 1 M, 68 MB @ 3 M;
25–40 ns insert, 10–24 ns lookup. Cheap.

**Concurrency.** Must be worker-side (§8). One shared mutable structure
touched once per directory by every worker, at the descend point.
Sharding recovers most of it, but it is new contention on the hot path
and would need `bench-compare.sh` warm **and** `--cold`.

**Failure modes.** Traversal-order dependence (below). Inode reuse
during a long scan silently omits a real subtree. NFS/overlay identity
weakness. A deliberate bind mount the user wants to see at both paths
gets collapsed with no explanation.

**Non-Linux.** Ports cleanly: macOS `(st_dev, st_ino)`; Windows
`FILE_ID_INFO` with a wider key and the ReFS caveats.

### Option B — mountinfo-driven mount-root dedup, precomputed

**Mechanism.** At scan start, read `/proc/self/mountinfo`. For each
mount, `stat` its mount point to get `(st_dev, st_ino)` — **not** its
`major:minor` field, which is wrong on btrfs (Research §2). Group mounts
by `(major:minor, field-4 root)` with **prefix containment**, not
equality. Within a group keep the mount whose mount-point path is
smallest in raw-byte order — the same canonical rule as dump-format D2 —
and mark the rest as aliases. Hand workers an immutable
`Arc<FxHashSet<(u64, u64)>>` of mount-point identities to skip.

**Fixes.** Cases 2 and 3 exactly, deterministically, with no shared
mutable state and no order dependence.

**Misses.** Case 1 entirely — `/.snapshots` has no mountinfo line
(proven). Case 4 unless super-options are parsed. Case 5 by
construction. Mounts the scanning user cannot `stat` leave holes in the
plan.

**Cost.** O(mounts): 36 entries here. One 10 ns hash lookup per
directory (~4.5 ms at 450 k dirs), or zero extra if gated behind a
boundary signal (Option D). O(mounts) `stat` calls at scan start.

**Concurrency.** Immutable `Arc`, read-only from every worker. Nothing
to lock, nothing to shard, no owner change.

**Failure modes.** TOCTOU between reading mountinfo and walking.
Unreadable mountinfo (some containers) → degrade to today. Mount
namespace mismatch: scanning a container rootfs from the host sees the
host's mounts, symmetrical to the freeable panel's known caveat.

**Non-Linux.** Does not port. macOS `getmntinfo(3)` has no field 4;
Windows needs reparse-point enumeration instead.

### Option C — filesystem-aware: B plus per-fs subvolume knowledge

**Mechanism.** B, plus at every `st_dev` change with no mountinfo entry:
on btrfs (by magic), treat `st_ino == 256` as a subvolume root, read
`BTRFS_IOC_SUBVOL_GETFLAGS` (unprivileged, verified) and skip read-only
ones as snapshots; skip `.zfs` by name on ZFS; parse `lowerdir=` /
`upperdir=` for overlays.

**Fixes.** Nominally all of 1–5.

**Misses.** Correctness, mostly — see Attack C. Also: bcachefs,
future filesystems, and anything the branch list has not been updated
for.

**Cost.** Moderate code, one ioctl per subvolume boundary (O(subvols)),
per-fs branches that rot.

**Concurrency.** Same as B for the plan; per-boundary ioctls are
O(boundaries), off the per-entry path.

**Non-Linux.** The per-fs half is Linux-specific by definition.

### Option D — statx-native boundary classification + a visible alias ledger; snapshots labelled, not deduped

**Mechanism.** Add `STATX_MNT_ID_UNIQUE | STATX_SUBVOL` to `STATX_MASK`
and read `STATX_ATTR_MOUNT_ROOT` out of the `stx_attributes` camembert
already receives. Every directory entry then classifies its own boundary
with **zero extra syscalls**:

| `st_dev` vs parent | `MOUNT_ROOT` | meaning | action |
|---|---|---|---|
| same | set | **same-filesystem bind mount** | consult the plan |
| changed | set | ordinary mount crossing | consult the plan |
| changed | unset | **nested subvolume** (Case 1) | label, do not dedup |
| same | unset | ordinary directory | descend |

`stx_subvol` supplies the same nested-subvolume signal filesystem-declared
rather than inferred, and names *which* subvolume. All four rows are
verified in the Research §3 table.

The **plan** is Option B's, consulted only where `MOUNT_ROOT` is set —
so the plan lookup is O(mounts), not O(dirs).

Aliases are **visible, never silent**: a new
`ExcludedReason::Alias { canonical }`, a summary/footer line ("2.4 GB
reached twice — counted at `/srv/data`, alias at `/data`"), a dump field,
and `--count-aliases` to opt out.

Subvolume boundaries are **not** deduped. They are recorded, tagged, and
handed to the existing exclusive-floor machinery (freeable2 D1/D3),
which is the mechanism that already answers "these bytes are shared"
with a fraction instead of a coin flip.

**Fixes.** Cases 2, 3 exactly and deterministically. Case 1 *reframed*
(labelled + floor-accounted). Case 6 if the file-level check is added at
the same boundary. Case 5 mitigated by `AT_NO_AUTOMOUNT`, which is a
one-flag change independent of everything else here.

**Misses.** Case 4 unless B's super-options parsing is folded in.
Reflink copies (correctly — the floor's job). NFS same-export.
Everything below kernel 5.8.

**Cost.** statx mask widening measured within noise (≤2 %) warm on this
box; plan O(mounts); lookups O(mounts). Essentially free — *pending the
mandated bench.*

**Concurrency.** Nothing shared and mutable. Immutable `Arc` plan,
per-entry classification from data already in the `statx` result. No
owner change, no lock, no shard. The best fit of the four.

**Non-Linux.** The syscalls are Linux-only, but the *shape* ports:
Windows' `FILE_ATTRIBUTE_REPARSE_POINT` is a strictly better
`MOUNT_ROOT`, and macOS needs no bind detection.

---

## Attack

Hostile review of the four options above, in the register of the
"Attack findings" sections of the other decisions docs. Findings are
numbered per option; severity is stated, not implied.

### Attack A (universal `(dev, ino)` seen-set)

1. **FATAL for its headline claim — A does not fix Case 1.** A snapshot
   subvolume's every object carries a different `st_dev` (36 vs 79,
   proven on this machine), so the set never matches. The feature would
   ship as "traversal dedup" and leave the loudest of the three caveats
   — the one on every snapper desktop — exactly where it is. Two thirds
   of a fix sold as three thirds.
2. **SERIOUS — it reintroduces the phantom-diff bug class dump-format D2
   was written to kill.** Which of two aliased copies "wins" is decided
   by work-stealing order, which is nondeterministic. The *total* is
   stable; the *attribution* is not. `diff` merge-joins on path, so a
   subtree that held the bytes in run 1 and zero in run 2 produces a
   phantom `Shrunk` plus a phantom `Grown`, at directory granularity
   where the numbers are large. D2 solved exactly this for hardlinks
   with a path-ordered canonical; A cannot use that rule because it must
   decide *before* descending, when it does not yet know the other
   path.
3. **SERIOUS — the "put it on the owner" placement is impossible as
   stated.** By the time the owner sees the batch, `child_token` is
   allocated, `pending_jobs` is bumped, and the job is stealable
   (`worker.rs:483-493`). There is no per-job cancel — only the global
   `abort` — and `integrate()` asserts `child_dirs_seen ==
   batch.child_dirs`. Owner-side A saves no I/O and no arena memory; it
   only zeroes numbers after paying for them. So A is a worker-side
   shared-mutable-state change on the descend path, which is the one
   place D1's architecture was designed to keep contention-free.
4. **SERIOUS — inode reuse lies in the silent direction.** A directory
   deleted and recreated during a long scan can reuse its inode number.
   A then skips a *real* subtree, under-reporting with no signal
   whatsoever. Every other camembert degradation announces itself
   (`Coverage::Exceeds`, "spans N filesystems", the errno breakdown);
   this one cannot.
5. **SERIOUS — is dedup even right here?** A user with two genuine
   copies of a tree gets both counted (distinct inodes) — good. But a
   user who *deliberately* bind-mounts a dataset at a second path,
   precisely so it shows up in two places, gets it silently halved. The
   correct behaviour is not "dedup" or "don't", it is "count once, say
   so, loudly". A has no ledger and no vocabulary for that.
6. **ANNOYING (self-correction) — the memory objection I expected to
   lead with is wrong.** Measured 8.7 MB at 450 k directories, 34 MB at
   1 M. Against a 450 MB budget this is 2–8 %. Anyone arguing against A
   on RSS grounds has not measured it; the case against A is semantic.
7. **ANNOYING — `apply_removal` refuses `EXCLUDED` nodes**
   (`RemovalError::Excluded`, `tree.rs:545`). If the alias is marked
   excluded, the user cannot mark or delete through that path and gets a
   bare refusal. For a bind alias that is arguably correct (deleting
   through either path deletes the same bytes, and allowing both would
   double the freed estimate), but only if the refusal explains itself.

### Attack B (mountinfo mount-root plan)

1. **FATAL for Case 1 — B is blind to nested subvolumes by
   construction.** `/.snapshots` is a real subvolume with a distinct
   `st_dev` and **zero** mountinfo lines (verified). B must say "fixes 2
   of 3" on the tin, or it is mis-sold.
2. **FATAL if implemented naively — mountinfo's `major:minor` is not
   `st_dev` on btrfs.** All seven mounts here report `0:33` while `stat`
   reports 36/53/54/55/56/57/58. An implementation that matches the
   mountinfo device field against the scanner's `st_dev` does nothing at
   all on the most common modern Linux root filesystem, and does it
   silently, passing every test written on tmpfs. The plan **must** be
   keyed by `stat`-ing each mount point. Note that the man page itself
   asserts the false equivalence — this is a trap, not an oversight.
3. **SERIOUS — equality on field 4 is insufficient.** Bind `/a` at `/b`
   and `/a/x` at `/c`: roots `/a` and `/a/x` are unequal, both survive,
   and `/c` still duplicates `/b/x`. B needs prefix containment over
   `(device, root)` — a prefix structure, not a hash set. Any
   implementation that ships set equality has a correctness hole shaped
   exactly like the bug it was built to fix.
4. **SERIOUS — the plan is stale before it is used, and camembert is the
   one staling it.** ZFS `.zfs/snapshot` and autofs materialise mounts
   *on access*, and camembert's stats do not pass `AT_NO_AUTOMOUNT`. The
   scan creates the mounts the plan failed to anticipate. B has no
   answer; the mitigation (`AT_NO_AUTOMOUNT`) is orthogonal to B.
5. **ANNOYING — the plan needs `stat` access to every mount point.** A
   mode-0700 root-owned mount point yields no key and a silent gap. The
   count of such gaps must be reported, not swallowed.
6. **ANNOYING — whose mountinfo?** `/proc/self/mountinfo` is the
   scanner's namespace. Scanning a container rootfs from the host, or a
   host tree from inside a container, sees the wrong mount table. This
   is the same class as the freeable panel's documented mount-namespace
   caveat and must be disclosed in the same voice, not assumed away.
7. **COSMETIC — canonical drift across runs.** If the user unmounts one
   of an aliased pair between two scans, the bytes move path and the
   diff shows it. Inherent to the problem rather than to B, but the
   diff needs vocabulary for "this moved because an alias went away",
   or it looks like data moved.

### Attack C (filesystem-aware)

1. **FATAL — "read-only ⇒ snapshot" is false in both directions.**
   `btrfs subvolume snapshot` without `-r` makes a *writable* snapshot;
   `btrfs subvolume create` + `property set ro true` makes a read-only
   *non*-snapshot. A rule keyed on the ro bit mislabels both, and the
   mislabel is invisible.
2. **FATAL (conceptual) — skipping a snapshot is not the honest
   answer.** A snapshot's bytes are CoW-shared and *partially*
   exclusive. Skipping it understates (its diverged extents are real,
   freeable-by-deleting-the-snapshot bytes); counting it in full
   overstates. C forces a binary choice where the truth is a fraction —
   and camembert already ships the machinery that computes that
   fraction (freeable2 D1/D3, the FIEMAP exclusive floor). C would ship
   a *worse* answer than the one already in the binary. The right move
   is to route Case 1 to the floor, not to the dedup.
3. **SERIOUS — `st_ino == 256` buys nothing over the `st_dev` change.**
   It is `BTRFS_FIRST_FREE_OBJECTID`, an implementation detail with no
   ABI promise, and it is true of every subvolume root including mounted
   ones. The discriminator is already the `st_dev` change; adding 256 is
   fragile-looking code that changes no outcome.
4. **SERIOUS — `.zfs` by name is a name heuristic in a codebase that has
   otherwise refused them,** and it is wrong under `snapdir=visible`
   where the user may legitimately want the snapshot contents. It also
   does not stop the automount, which is the actual damage.
5. **SERIOUS — the user's own framing is the argument against C.** "On
   n'aura pas que btrfs à terme." A per-filesystem branch list is a
   maintenance liability that silently degrades to nothing on every
   filesystem nobody wrote a branch for — bcachefs subvolumes today,
   whatever ships next.
6. **COSMETIC — one part of C is worth keeping.** Parsing `lowerdir=` /
   `upperdir=` is the only mechanism that catches Case 4, and it has
   nothing to do with subvolumes. Extract it; drop the rest.

### Attack D (my own recommendation, attacked hardest)

1. **SERIOUS — D introduces a third kernel tier and this dossier was
   written on a 7.1 box.** `MOUNT_ROOT`/`MNT_ID` need 5.8,
   `MNT_ID_UNIQUE` 6.8, `SUBVOL` 6.10. The project already gates FIEMAP
   at 6.1. On RHEL 9 (5.14) or Debian 12 (6.1) the subvolume signal is
   absent and D degrades to B — i.e. to "fixes 2 of 3". Selling D's
   completeness on the dev machine's kernel would be exactly the
   dishonesty this project exists to avoid. The capability must be
   reported per scan, like `Coverage::Exceeds` and the compress caveat
   are.
2. **SERIOUS — `MOUNT_ROOT` tells you *that*, never *which*.** D still
   needs B's plan to decide whether a mount is an alias, so it inherits
   B findings 2, 3, 5 and 6 in full, and adds a coupling: the plan must
   be keyed on `(dev, ino)` of the mount root, which is precisely what
   B5 says can be missing. When it is, D confidently announces "mount
   root!" and then has nothing to look it up with. The fallback must be
   "descend and count", never "skip".
3. **SERIOUS — this is a scan-hot-path change and the measurement above
   is not sufficient.** The ≤2 % figure is warm cache, sync `statx`,
   one directory, one machine, with two of three runs too noisy to read.
   `STATX_MASK` is shared with the io_uring path (`uring.rs`), which was
   never measured here at all, and CLAUDE.md mandates
   `scripts/bench-compare.sh` warm **and** `--cold` before/after for
   anything touching `scan/`. No exceptions, including "it's just a mask
   bit".
4. **ANNOYING — the typed-wrapper seam.** `rustix` 1.1.4 has no
   `StatxFlags::MNT_ID_UNIQUE` or `::SUBVOL`; D needs
   `from_bits_retain(0x4000 | 0x8000)`. Small, but this codebase has
   been deliberate about staying on typed wrappers, and hand-rolled bits
   are how mask/attribute confusion gets in.
5. **ANNOYING — the `fstatat` fallback has none of this.** Under
   `ENOSYS`, seccomp or gVisor, `statx_supported` flips off and D's
   entire classification disappears. D must define that state explicitly
   ("aliasing check not run") rather than quietly reverting to today's
   behaviour while still claiming the feature.
6. **ANNOYING — mount IDs must never reach a dump.** `stx_mnt_id` is
   boot-scoped and (without `_UNIQUE`) reused after umount. It is a
   scan-time discriminator only.
7. **ANNOYING — the alias ledger is a new dump field and a new UI
   surface.** Additive minor bump, like `er` was, so cheap — but it is
   scope the recommendation must own rather than wave at, along with
   `--count-aliases` in `--help` and the README.
8. **SERIOUS — "label, don't dedup" leaves the loudest complaint
   unfixed.** Under D, `camembert /` on a snapper root still multiplies
   snapshot bytes by the snapshot count, and answers with a caption and
   a floor figure. That is defensible and it is honest, but a user
   staring at 3× their disk capacity will not feel the difference
   between "wrong" and "wrong, with a label". The mitigations are real
   (the gauge already refuses to fabricate 100 %; `--one-filesystem`
   already avoids it; the floor quantifies the sharing) but they are
   mitigations, and the dossier should not pretend the caption closes
   the case.
9. **ANNOYING — the name oversells.** D only ever collapses *mount
   aliases*, never distinct inodes. It does nothing about reflinked or
   `duperemove`d copies, which is what many users mean by "double
   counting". "Traversal dedup" must not be allowed to imply extent
   dedup in the release notes.
10. **COSMETIC — deletion interaction, mostly benign.** An alias marked
    `EXCLUDED` hits `RemovalError::Excluded`; correct for a bind alias
    (one deletion frees both paths; permitting both would double the
    freed estimate) but needs the reason threaded into the message. The
    deletion executor's confirm-time `(dev, ino)` re-anchoring is
    unaffected — it anchors on the canonical path, which is the one that
    was walked.
11. **The acceptance test writes itself, and it is a bind loop.**
    `mount --bind /a /a/b` is unbounded recursion for any walker. D
    stops it — but *only* if B3's containment logic exists and *only* if
    the mount predates the scan (B4). Without both, D recurses to fd
    exhaustion. Any implementation must ship with a depth/repeat guard
    that is independent of the plan, because the plan is the thing that
    can be wrong.

### Cross-cutting

- **Reproducibility across runs.** A: no (order-dependent, see A2). B,
  C, D: yes — the decision is precomputed from a path-ordered plan
  before any worker starts, so two scans of an unchanged system agree.
  This is the single sharpest discriminator between A and the rest, and
  it is a direct consequence of the D2 canonical rule the project
  already committed to.
- **Interaction with the hardlink canonical owner (dump D2).** No
  conflict: hardlink dedup is per-inode and post-scan; alias dedup is
  per-mount and pre-scan. But if an alias subtree is skipped, its links
  never enter `hardlink_links`, so the canonical (smallest raw-byte
  path) is chosen among *seen* links only — which is exactly what D2
  already says ("among all links seen by the scan"). The invariant
  holds; the wording needs no change; a test should pin it.
- **Interaction with `apply_removal`.** Covered in A7/D10. The negative
  delta propagation is unaffected — an alias contributes zero to
  aggregates, so removing it (if it were permitted) would subtract
  nothing.

---

## Recommendation

**Ship Option D, scoped small, and route Case 1 to the exclusive floor
rather than to dedup.**

Concretely:

1. Widen `STATX_MASK` with `MNT_ID_UNIQUE | SUBVOL` and read
   `STATX_ATTR_MOUNT_ROOT`. Classify every boundary with the four-row
   table above. Bench warm and `--cold` before and after, as mandated.
2. Build B's mount plan at scan start — mountinfo, `stat` each mount
   point for its `(dev, ino)` (**never** its `major:minor`), group by
   `(device, root)` with prefix containment, canonical = smallest
   raw-byte mount-point path (dump D2's rule, reused).
3. Consult the plan **only where `MOUNT_ROOT` is set**. Skipped aliases
   become `ExcludedReason::Alias { canonical }`, surfaced in the
   summary, the TUI, and the dump.
4. Pass `AT_NO_AUTOMOUNT` on every stat. This is independent of
   everything above, costs nothing, and closes Case 5's worst edge.
5. Add an unconditional repeat-boundary guard (a small
   `(dev, ino)`-of-mount-roots set on the traversal path, O(mounts)) so
   a bind loop terminates even when the plan is wrong.
6. Do **not** dedup subvolume boundaries. Record `stx_subvol`, tag the
   subtree, and let freeable phase 2's exclusive floor answer how much
   of it is real.

**Default or flag?** Default **on** for mount aliases; `--count-aliases`
opts out. The justification is narrow and should stay narrow: a mount
alias is the *same inodes*, and one `rm -rf` frees both paths, so
counting them twice is not a different opinion about the data, it is
arithmetic that does not correspond to any disk. Everything less
certain than that — subvolumes, reflinks, two genuine copies — stays
counted, because for those the double is not obviously wrong.

**Does it change what the headline total means?** Yes, and this must be
said in Honest numbers rather than absorbed. Today: *bytes reachable by
walking*. After: *distinct bytes reachable by walking*. For a
disk-usage tool the second is the right definition — but it is a
definition change, and a user whose total drops after upgrading is owed
the alias line that explains where the bytes went. That line is the
feature, as much as the dedup is.

**Deferred:** overlayfs `lowerdir=`/`upperdir=` aliasing (Case 4 —
real, present on this machine, but wants its own mechanism); NFS
same-export detection; the bind-mounted-file case; Windows/macOS
ports; any extent-level dedup, which belongs to the floor and not here.

---

## Displacement

*What does this work displace, and does the thesis agree with that
trade?*

On the table: **freeable phase 2 slice 3** (floor figures on flat-view
rows, filtered-total floor sums, breakdown groups) and the **age /
"big and cold" score view**, whose dossier is in progress.

Honest answer: **traversal dedup should not displace slice 3, and this
dossier's own analysis is the reason.**

The thesis is honest answers to real questions. Traversal dedup is a
correctness item, not a new question — it fixes an arithmetic error the
crossing-by-default flip created. That argues for doing it. But look at
what it actually fixes: Cases 2 and 3 are server and container shapes
(bind mounts, multi-mounts). The case that hits an ordinary desktop —
Case 1, the snapper root that reports 3× the disk — is the one this
dossier concludes **should not be fixed by dedup at all**, because the
honest answer to "how much is that snapshot really costing me" is a
fraction, and the thing that computes that fraction is the exclusive
floor. Slice 3 is what puts that fraction in front of the user on
ordinary rows.

So the sequencing follows from the analysis rather than from taste:

1. **Slice 3 first.** It finishes a flagship that is two-thirds shipped
   and it is the real answer to the loudest of the three caveats.
   Option D leans on it.
2. **Then D's cheap half** — the statx classification, the mount plan,
   `AT_NO_AUTOMOUNT`, the loop guard. Small, self-contained, no
   architectural change, no shared mutable state, and it retires two
   documented caveats and one undocumented case (Case 5).
3. **Then the age view**, which opens a question no competitor answers,
   versus a correctness fix every competitor also gets wrong.

The one thing that should *not* wait for either is `AT_NO_AUTOMOUNT`:
it is a one-flag change, it prevents camembert from mutating the system
it is measuring, and nothing else in this dossier depends on it.

If the trade is refused and dedup goes first, the thesis still survives
— it is a real error and fixing it is not wrong. But the version of it
that would go first should be D's cheap half, never A, and never a
version that claims to have fixed the snapshot case.
