# Research digest: freeable column phase 1 (deleted-but-open files)

Factual input for HANDOFF "Suggested next steps" §1, "Freeable column, phase
1". Facts with sources and measurements, no recommendations. Gathered
2026-07-23 on a CachyOS (Arch-based) desktop, kernel 7.1.4-1-cachyos, via man
pages, kernel docs/patches, `lsof`/`lsfd` source and behavior, and live
experiments in the repo scratchpad (nothing in the repo itself was modified).

## 1. Enumeration mechanics

- `/proc/[pid]/fd/N` is a **magic symlink** (`proc_pid_fd(5)`): `readlink(2)`
  returns the last-known path; `stat(2)`/`fstatat(2)` **without**
  `AT_SYMLINK_NOFOLLOW` dereferences it straight to the live inode, even
  after unlink. Confirmed live: after `rm`-ing a 5 MiB open file,
  `os.stat('/proc/self/fd/N')` (Python's `os.stat` uses `stat(2)`, i.e.
  follows) returned `st_size=5242880 st_nlink=0`, the real numbers — while
  GNU `stat(1)` **without `-L`** printed the symlink's own info (`Size: 64`,
  "symbolic link"), because GNU coreutils' `stat(1)` defaults to `lstat(2)`
  unless `-L`/`--dereference` is given. This is a coreutils convention, not
  a procfs quirk — direct `fstatat(dirfd, name, 0)` (no `AT_SYMLINK_NOFOLLOW`)
  is what a Rust implementation wants, and it just works.
- The `" (deleted)"` suffix is appended by the kernel's `d_path()` machinery
  when the dentry has been unlinked. A 2022 kernel patch
  ([lkml thread](https://lkml.iu.edu/hypermail/linux/kernel/2201.2/07063.html))
  fixed a related bug: the buffer for these symlinks used to be
  `PAGE_SIZE` (arch-dependent) via `__get_free_page()`; switched to a fixed
  `kmalloc(PATH_MAX, ...)` so a very long canonical path plus the suffix
  doesn't get truncated inconsistently across architectures.
- **The suffix reflects *this dentry's* unlink, not the inode's aggregate
  link count.** Live test: created `hl_a.txt`, hard-linked to `hl_b.txt`
  (`ln`), opened `hl_a.txt`, then `rm hl_a.txt` (one of two links).
  `readlink` on the fd still shows `hl_a.txt (deleted)`, but
  `st_nlink == 1` (the file is *not* actually gone — `hl_b.txt` keeps the
  inode alive, and that path is presumably still present elsewhere in the
  scanned tree). **`st_nlink == 0` is the ground truth for "no path
  anywhere references this inode any more"; the `(deleted)` string is not.**
  `lsof` gets this right: its own `+L1` filter is driven by a freshly
  `stat`ed `nlink` field (confirmed both by behavior — `lsof +L1` did *not*
  list the still-hardlinked file in the test above — and by the `nlink`/
  `nlink_def` fields in `lib/proc.c` of
  [lsof-org/lsof](https://github.com/lsof-org/lsof)). util-linux's `lsfd`
  is even more explicit: it ships a boolean `DELETED` column documented as
  *"reachability from the file system"*, and empirically its value tracks
  `NLINK == 0`, not the kernel string.
- **The suffix is genuinely ambiguous**, and the kernel documentation says
  so outright. `proc_pid_maps(5)`: *"the string ' (deleted)' is appended to
  the pathname... this is ambiguous too"* — a file legitimately named
  `notreallydeleted (deleted)` produces an identical-looking readlink
  target while `st_nlink == 1`. Reproduced live: `touch "foo (deleted)"`,
  open, readlink shows `.../foo (deleted) (deleted)`... actually shows
  `.../foo (deleted)` (the literal name already ends that way) with
  `st_nlink == 1`. **Conclusion: never string-match on `" (deleted)"`;
  always confirm with `st_nlink == 0`.**
- **Renames are tracked live**, not frozen at open time: opened a file,
  renamed it (`mv a b`), `readlink` immediately reports the new name `b`;
  after `rm b`, it reports `b (deleted)`. The kernel resolves the path at
  `readlink()` time from the dentry's current parent chain, not from a
  cached string — so a rename-then-delete shows the *final* name, never the
  original one the scan tree knew.
- **Bind mounts / mount namespaces**: the kernel documents an
  `"(unreachable)"` prefix mechanism for `d_path()` when a dentry can't be
  resolved from the calling (reading) process's mount-namespace root
  ([commit
  8df9d1a](https://github.com/torvalds/linux/commit/8df9d1a4142311c084ffeeacb67cd34d190eff74),
  `prepend_unreachable()` in `fs/dcache.c`). This is real and kernel-
  documented but **not reproduced live in this pass**: an `unshare --user
  --map-root-user --mount` experiment with a private tmpfs bind-mounted at
  `/tmp` inside the namespace was still fully resolvable from *outside* the
  namespace, most likely because the child mount namespace inherited
  "shared" propagation from the host's default mount setup (systemd
  typically marks `/` as `shared`), so the new mount propagated back up
  rather than staying private. Reproducing a genuinely unreachable case
  would need an explicit `mount --make-rprivate` (or a real container
  runtime using `pivot_root`) — flagged as unconfirmed-live but
  kernel-documented.
- **overlayfs**: kernel overlayfs sets `f_path` to point at the overlay
  dentry specifically so `/proc/pid/fd` paths are correct
  ([lkml](https://lkml.iu.edu/hypermail/linux/kernel/1506.2/02337.html)) —
  no special handling needed. **fuse-overlayfs** (userspace FUSE overlay,
  common in rootless Podman) is a real exception: deleted-but-open files
  become **completely inaccessible** through the fd, breaking even
  `truncate()` on them — a
  [documented bug](https://github.com/containers/fuse-overlayfs/issues/175)
  that also breaks Python's `tempfile` and `pytest`. On fuse-overlayfs,
  phase 1 would simply see nothing (or errors) for such files — not a
  false positive, but a coverage gap worth naming.

## 2. What holds space besides plain deleted regular files

Live sweep of this desktop (505 processes, see §6) broke down every fd whose
`readlink` ended in `" (deleted)"`:

| Category | Count | Notes |
|---|---|---|
| `memfd` (`/memfd:name (deleted)`) | 603 | Anonymous RAM-backed file (`memfd_create(2)`); **never had a disk path**, always shows deleted. Backing store is tmpfs/shmem, not the scanned filesystem — must be excluded from a "freeable disk bytes" claim, or reported separately as RAM. |
| `shm`/`/dev/shm`/anon_inode | 418 | Similar — POSIX/SysV shared memory, again not disk-backed on most systems. |
| O_TMPFILE-style (`/tmp/#12345 (deleted)`, no `O_TMPFILE` name ever) | 3 | Genuine disk-backed (created via `open(..., O_TMPFILE)`), just never had a filename — always shows as deleted, `st_nlink == 0` from birth, correctly counted the same as a deleted regular file once linked via `linkat(AT_EMPTY_PATH)` semantics. |
| Regular deleted files, `st_nlink == 0` | 82 fd entries → 66 unique `(dev,ino)` | The actual target: `.cache` files, Chromium/Electron `leveldb` log/ldb files held by multiple renderer PIDs simultaneously (see §5), ~117.6 MiB deduped total on this idle desktop. |
| Deleted-suffix but `nlink > 0` (other hardlink survives) | 3 | Confirms §1's ambiguity is not just theoretical — happens on a live system too. |

- **mmap'd deleted files**: `/proc/[pid]/maps` shows the same `(deleted)`
  suffix on the pathname field of file-backed mappings
  (`proc_pid_maps(5)`). Confirmed live: mmap'd a file, unlinked it, the
  `maps` line showed `.../mmaptest.bin (deleted)` with the mapped range and
  device:inode (`00:34 1065209`). `/proc/[pid]/map_files/<range>` also
  resolves (readlink) to the same deleted path, giving a second, `fd`-
  independent way to find these — a file can be mmap'd-and-deleted with
  **no open fd at all** (mapping alone keeps the space allocated), so a
  `map_files`/`maps` pass catches cases an `fd/*` sweep misses entirely.
  However: **`stat()`ing through `/proc/[pid]/map_files/<range>` itself
  requires `CAP_SYS_ADMIN` or (since Linux 5.9) `CAP_CHECKPOINT_RESTORE` in
  the *initial* user namespace** — confirmed live, `EPERM` on our own
  process's own mapping despite being same-uid (`proc_pid_map_files(5)`;
  restriction added to avoid exposing mappings not otherwise visible to the
  reader). So `maps` (readable under the ordinary ptrace-access rule) gives
  the deleted path and the mapped byte range, but getting the *authoritative*
  `st_blocks`/`st_size` of that inode via `map_files` needs elevated
  privilege most callers won't have. The mapped-range length from `maps`
  is a usable (page-rounded, possibly partial) lower-bound size proxy when
  `map_files` stat is unavailable.
- **loop devices**: not tested live (would need root to attach a loop
  device); by the same mechanism, a loop-mounted deleted backing file would
  show up as a deleted regular file under the loop-owning process's `fd/`
  (or under `losetup`'s backing-file listing) — same detection path,
  not verified experimentally this pass.
- **Unlinked directories**: rmdir requires an empty directory, so an "open
  unlinked directory" (`fd` held via `open(dir, O_DIRECTORY)` then the dir
  removed) holds no reclaimable *data* blocks beyond the directory's own
  metadata block(s) — out of scope for "freeable bytes" in any meaningful
  amount, not investigated further.
- **What `lsof +L1` shows vs. misses**: `+L1` filters to `nlink < 1`,
  i.e. exactly the `st_nlink == 0` ground truth from §1 — it does **not**
  distinguish memfd/shm from genuine disk files in its default output
  (they all show `nlink 0`, "REG" type, with the memfd/shm path prefix as
  the only tell). It does not walk `map_files`/mmap-only holders by
  default. **Phase 1 can honestly claim**: fd-held deleted regular files
  (including `O_TMPFILE`) filtered by `st_nlink == 0` and by excluding
  `memfd:`/shm/anon_inode paths and any dentry not on the scanned
  filesystem's `st_dev` (see §3). It can note, but not fully solve without
  `map_files` privilege, the mmap-only-no-fd case.

## 3. Attribution to the scan

- After unlink, the path is gone from the tree camembert built, so the
  `readlink` string is the *only* textual link back to "where this used to
  live" — and §1 already showed it can be: stale after further renames (no
  — renames are live, so it's actually the *final* path, which is truthful
  but may no longer match anything the scan tree remembers if the rename
  moved it out of the scanned subtree), ambiguous with legitimately-named
  files, subject to the "unreachable"/mount-namespace caveat, and, per
  `proc_pid_fd(5)`/general procfs behavior, **not guaranteed valid UTF-8**
  (kernel paths are arbitrary byte strings; `readlink(2)` returns raw
  bytes — a Rust implementation must use `OsString`/`Vec<u8>`, not assume
  `String`, and handle `PATH_MAX`-ish truncation gracefully for
  pathologically long names, per the buffer-sizing patch in §1).
- **`st_dev` matching against the scanned filesystem is the robust
  fallback**, and arguably the primary signal: disk space is reclaimed
  per-filesystem, not per-directory — a deleted file's blocks return to
  whichever mounted filesystem's free-space counter, regardless of which
  directory it used to live under. Matching `(dev, ino)` pairs from the
  `/proc` sweep against the `st_dev` of the scan root already gives a
  correct, path-independent filter: "this deleted inode belongs to the
  filesystem I'm scanning," full stop — no need to trust the path string
  for the *decision* of whether it's in scope, only (optionally) for
  *display*.
- **Practical implication for attribution**: bytes freeable by closing a
  given fd are trustworthy at the **filesystem-total** level (add to the
  scanned mount's "freeable" figure) but only *heuristically* attributable
  to a **directory** in the tree (best-effort using the last-known path,
  when it parses as UTF-8 and falls under the scanned root) — the
  directory-level number can be wrong (moved-then-deleted files, deleted
  files from an already-excluded subtree, `/proc`'s own truncation of very
  long names) in ways the filesystem-level number cannot.

## 4. Permissions and visibility

- `proc_pid_fd(5)`: reading (`readlink(2)`) the symlinks in
  `/proc/[pid]/fd/` is governed by a **ptrace access mode
  `PTRACE_MODE_READ_FSCREDS`** check — same rule as `/proc/[pid]/maps`.
  In practice this means: same effective UID (or the target process is
  "dumpable" to you), or the reader has `CAP_SYS_PTRACE`. Confirmed live:
  `ls /proc/1/fd` (root-owned PID 1, us as regular user) → `Permission
  denied`.
- **`hidepid` mount option** (`proc(5)`, since Linux 3.3): `hidepid=0`
  (default, this machine: confirmed via `findmnt -no OPTIONS /proc` →
  bare `rw,nosuid,nodev,noexec,relatime`, no `hidepid=`) lets everyone see
  every `/proc/pid` directory (though `fd/` contents inside are still
  gated by the ptrace check above); `hidepid=1` additionally hides
  sensitive files like `cmdline`/`status` from non-owners;
  `hidepid=2` makes other users' `/proc/pid` directories invisible
  entirely (can't even enumerate PIDs). A `gid=` mount option can exempt
  a trusted group. Servers and hardened desktops sometimes set
  `hidepid=2`; this machine (typical desktop default) does not, but the
  per-fd ptrace check applies regardless of `hidepid`.
- **Quantified degradation on this desktop**: of 505 processes, only
  140 (28%) were owned by the invoking user (`theo`); 324 (64%) were
  root-owned system/session daemons, the rest a handful of system service
  users (`101`, `70`, `dbus`, `avahi`, ...). A live sweep as this user
  could read `fd/` for exactly those 140 own-uid processes; `ls
  /proc/<other-uid-pid>/fd` failed with `EACCES` for the rest (measured:
  365 of 505 `fd/` directories denied). **On a typical desktop, running
  unprivileged still sees your own big consumers (browser, editor, dev
  tools) since they run as you — the miss is mostly system daemons, which
  rarely hold large deleted user files.** On a multi-user server (a
  webserver running as `www-data`, a database as `postgres`, etc.), an
  unprivileged sysadmin account would miss essentially all of the
  interesting deleted-file holders unless run as root or with
  `CAP_SYS_PTRACE` — this is the classic reason the serverfault-style
  advice (§8) always says "run lsof as root."
- **Containers**: a container's PID namespace means a host-side scan
  (run as root on the host) still enumerates the container's processes
  under their host PIDs and can inspect their `fd/` normally (subject to
  UID mapping — rootless containers remap UIDs, so the host-visible owner
  UID may not be 0 even for a "root-in-container" process, but the ptrace
  check still applies to whatever UID the kernel resolves it to). Not
  independently reproduced this pass beyond the mount-namespace test in
  §1.
- **`/proc` absent (chroot)**: if the process running camembert is chrooted
  into an environment with no `/proc` mounted, `/proc/self/fd` and
  `/proc/[pid]/fd` simply don't exist — `opendir`/`open` fail with `ENOENT`
  (or `ENOTDIR` if some unrelated `/proc` path exists but isn't procfs).
  Not independently reproduced live this pass (blocked by lacking a
  minimal static binary to `chroot` into for the test — `busybox` wasn't
  installed on this machine and a bare `chroot` needs *something* to
  exec); this is standard, well-documented procfs behavior, not a special
  case worth an experiment. **Implication for camembert: detect `/proc`
  unavailability up front (e.g., a failed `open("/proc/self")`) and
  degrade the freeable-column feature to "unavailable here" rather than
  fail the whole scan.**

## 5. Dedup and sizing

- **Same inode, many holders**: on the live desktop sweep, the Claude
  desktop app's Chromium-based renderer processes held the *same* deleted
  IndexedDB leveldb file open across up to 5 different PIDs simultaneously
  (e.g. `(dev=58, ino=1834719)` held by PIDs `5775, 146026, 30421, 13715,
  188323` — all the same `.../IndexedDB/.../000096.log (deleted)`).
  Counting per-fd here would inflate the reported freeable total by up to
  5x for that one file. **Dedup key: `(st_dev, st_ino)`.**
- **`st_blocks * 512` is the real space figure**, not `st_size`. Confirmed
  with a sparse file: `truncate -s 1G`, wrote only the first 4 KiB, then
  unlinked while open — `st_size` stayed `1073741824` (1 GiB, the nominal
  size) while `st_blocks * 512` correctly reported `4096` (the actually
  allocated bytes, i.e. the true freeable amount on `rm`/close). Using
  `st_size` for a sparse deleted file would wildly overstate freeable
  space.
- **Files still being written during the sweep**: confirmed a file's
  `st_size` can differ between two `stat` calls a few instructions apart
  if the holding process is actively writing. A "sweep" is a series of
  independent point-in-time snapshots, one per fd — there's no
  cross-process locking and none is needed; a growing/shrinking deleted
  file just reports whatever it was at the instant of that particular
  `stat`. This is the same staleness every `du`/`ncdu`-style tool already
  lives with for ordinary files (self-corrects on the next sweep) — not a
  new problem, but worth stating rather than assuming atomicity.
- 512-byte block size for `st_blocks` is POSIX-fixed regardless of the
  underlying filesystem's actual block size (`stat(2)`: *"st_blocks... is
  in 512-byte units"*), so no per-filesystem block-size lookup is needed
  to convert it to bytes.

## 6. Cost

- **Sweep complexity**: O(nr_processes × nr_fds_per_process) `readdir` +
  `readlink` + `fstatat` calls — no way around visiting every fd, but each
  syscall is cheap and there is no recursion or hashing beyond the final
  `(dev,ino)` dedup.
- **Measured on this machine** (single-threaded Python 3, `os.listdir` +
  `os.readlink` + `os.stat` per fd, no threading): 505 processes, 6559 fds
  total (own-uid visible subset — see §4), **37 ms** wall time (two runs:
  37.7 ms and 36.4 ms). Of those fds, 1103 carried the `(deleted)` marker;
  after filtering to `st_nlink == 0` and excluding `memfd:`/shm paths, 66
  unique `(dev,ino)` regular deleted files remained, totaling **≈117.6
  MiB** of deduped freeable space on this otherwise-idle desktop (mostly
  Chromium/Electron leveldb churn and a KDE sycoca cache file).
- **Cross-checked against existing tools** on the same machine: `lsof +L1`
  (forks a full-featured C program, resolves sockets/pipes/users/etc. too)
  took **226 ms** for its narrower "deleted only" output (1106 lines);
  `lsfd` (util-linux, Rust-adjacent-quality modern C) took **747 ms** to
  enumerate *all* 72,251 open-file-table entries system-wide (all
  processes, all fd types, thread-level detail) — a much bigger job than
  camembert's phase-1 scope. **A dedicated, narrowly-scoped
  `fd/` + `stat` + `(dev,ino)` dedup sweep is trivially fast** (tens of
  milliseconds even in an unoptimized single-threaded scripting-language
  prototype) and does **not** need threading to stay off the UI's critical
  path; it could even run to completion before the main directory scan
  starts, or as a fire-and-forget background pass, with no engineering
  pressure to parallelize it.
- **TOCTOU races**: confirmed live — killed a short-lived process mid-
  "would-be-sweep", then both `os.listdir(fd_dir)` and
  `os.readlink(fd_path)` raised plain `FileNotFoundError` (`ENOENT`). No
  crash, no special-casing needed beyond "treat ENOENT as this fd/pid no
  longer exists, skip it" — exactly as benign as the equivalent race in a
  filesystem walk (a file vanishing between `readdir` and `stat`).

## 7. Reuse for the deletion open-file warning

- The same sweep (`fd/*` → readlink → `fstatat`) naturally produces, for
  every currently-open file system-wide, its `(dev, ino)`. The
  "deleted-but-open" filter of §1–2 is just `st_nlink == 0` (plus the
  memfd/shm exclusion); the "is this entry-marked-for-deletion currently
  open by someone" check for the warning is the **same data, unfiltered**:
  build a `HashSet<(dev, ino)>` (or a `HashMap` to also carry which PIDs)
  of every open file's `(dev, ino)`, then look up each candidate-for-
  deletion entry from the scan tree against it. No extra syscalls, no
  extra permission requirements — it's the same walk, and can be done in
  the same pass that already collects the deleted-file data, just without
  discarding the `st_nlink != 0` entries.
- Practical cost delta: none measured beyond keeping a larger hash set
  in memory (6559 entries on this desktop, trivial) instead of discarding
  most of them.

## 8. Prior art

- **`lsof +L1`** (documented in `lsof(8)`): the closest existing tool.
  Filters open files to `nlink < 1`. Confirmed via source
  (`lib/proc.c`, `nlink`/`nlink_def` fields) and live behavior (§1, §6)
  that it's driven by a fresh `stat`'s link count, not the kernel's
  `(deleted)` string — i.e. it already avoids the ambiguity flagged in
  §1. Doesn't separate memfd/shm from genuine disk files in its default
  view, doesn't walk `map_files`.
- **`lsfd`** (util-linux ≥ 2.35-ish, installed here as util-linux 2.42.2):
  a modern, more structured `lsof` alternative with an explicit
  `DELETED <boolean>` column ("reachability from the file system"),
  `NLINK`, `SIZE`, `INODE`, `MNTID` columns and a filter expression
  language (`-Q 'DELETED == 1'`). Confirmed live that its `DELETED`
  values track `NLINK == 0`. No dedicated "disk-usage" framing — it's a
  process/file inspector, not a du-style aggregator, and reports raw rows
  rather than a deduped total.
- **`lsof-org/lsof` issue
  [#65](https://github.com/lsof-org/lsof/issues/65)**, "Show deleted
  files": a user asks for exactly this feature (surfacing deleted-but-open
  files to debug vanishing disk space) to be shipped as part of `lsof`
  itself, referencing their own `lsdf` shell-script wrapper. Confirms this
  is a recognized, recurring pain point independent of camembert.
- **ncdu, gdu, dua-cli, pdu, baobab, WizTree, filelight, WinDirStat**: no
  evidence found (web search, project docs) that any mainstream
  disk-usage analyzer surfaces deleted-but-open files today. This appears
  to be an open niche, not a solved-and-copied feature.
- **What sysadmins actually do** (the serverfault-class advice, confirmed
  across multiple independent write-ups — Red Hat KB, nixCraft, Baeldung,
  kerneltalks, F5 KB): run `lsof +L1` (as root, since the target processes
  are usually not the sysadmin's own), identify the PID and path, then
  either (a) restart/`kill -HUP` the holding process so it reopens its log
  file fresh, or (b) if the process can't be restarted, truncate the file
  **through the still-open fd** (`> /proc/PID/fd/N` as the file's owner,
  or `truncate -s0 /proc/PID/fd/N`) to reclaim space without closing the
  descriptor or disrupting the process. This is the workflow the
  guilty-PID display should make discoverable, not replace — camembert
  surfacing "PID 1234 (httpd) is holding 1.8 GiB of deleted
  `/var/log/access.log`" is the diagnostic step these guides currently
  do by hand with `lsof | grep deleted`.

## Open design questions

Not decisions — flagging for the actual design pass:

1. Should memfd/shm/anon_inode entries be surfaced at all (as a separate
   "RAM, not disk" line) or silently excluded from phase 1? They're real
   host memory pressure but not disk-usage in the sense this tool reports.
2. How to handle the `map_files`-requires-`CAP_SYS_ADMIN` gap: skip
   mmap-only (no-fd) deleted files entirely in phase 1 (simplest, honest,
   probably fine since fd-held cases dominate in practice), or attempt
   `map_files` and silently degrade the size estimate to the mapped-range
   length from `maps` on `EPERM`?
3. Directory-level attribution of freeable bytes: best-effort via the
   last-known path (with `st_dev` as the hard filter and non-UTF-8/
   "unreachable"/truncated paths falling back to "unattributed, this
   filesystem") — is a wrong-but-plausible directory number worse than no
   directory number at all for user trust?
4. Whether phase 1 needs privilege-escalation guidance/prompting at all
   (run as root to see other users' fds) given §4's finding that a
   desktop user already sees their own big consumers unprivileged, while
   a server sysadmin usually does not.
5. Reuse of the sweep for §7's open-file warning: same pass or a
   deliberately separate one (freshness/timing tradeoffs if the main scan
   is long-running and the sweep is done once up front vs. re-run at
   deletion time).
6. Whether to attempt the loop-device and unlinked-directory cases at all
   (both unconfirmed live this pass, both likely rare/low-value) or
   explicitly scope them out of phase 1 in the docs.
