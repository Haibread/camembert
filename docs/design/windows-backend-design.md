# Windows scan backend — T1 design

Companion to the platform seam introduced in `camembert-core/src/scan/linux.rs`.
Status: **design, not settled decisions** — but no longer a speculative one.
The three load-bearing assumptions in §7 were measured on a real Windows
Server 2022 box on 2026-07-26 and all three held; §2's compression table and
§7's transcripts are observations, not inferences.

Scope is tier T1: camembert compiles on `x86_64-pc-windows-msvc`, traverses,
and reports honest sizes and hardlink-deduplicated totals. `freeable`,
`fiemap` and `delete` stay `cfg`-gated off — the first two have no Windows
equivalent worth faking (see HANDOFF), the third has no WTF-8 encoder yet.

## The decision this rests on

`windows-sys` is a T1 dependency. A `std`-only walker had two holes that
share one key: `std` exposes no allocation size and no link count on Windows
(`MetadataExt::{file_index, volume_serial_number, number_of_links}` are all
`#[unstable(feature = "windows_by_handle")]`, tracking issue #63010, still
open at 1.88). Taking the dependency keeps `sem` at `"blocks"` and hardlink
dedup on, which is what the project's thesis requires.

## 1. Dependency

```toml
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61.2", features = [
    "Win32_Foundation",
    "Win32_Security",              # OBJECT_ATTRIBUTES's field types need it
    "Win32_Storage_FileSystem",
    "Win32_System_IO",             # IO_STATUS_BLOCK
    "Win32_System_SystemServices", # IO_REPARSE_TAG_*  <- NOT in Storage_FileSystem
    "Wdk_Foundation",              # OBJECT_ATTRIBUTES
    "Wdk_Storage_FileSystem",      # NtQueryInformationByName, FILE_STAT_INFORMATION
] }
```

`windows-sys` 0.61 links through `windows-link` + `raw-dylib`: no import
libraries, no `windows-targets`, nothing for a CI runner to set up. Its
`rust-version` is 1.71, comfortably under the workspace's 1.88.

Two feature placements are counter-intuitive and cost a compile error if
guessed: the reparse tags live in `Win32_System_SystemServices`, not in
`Win32_Storage_FileSystem`; and `Win32_Security` is required not for any
security API but because `OBJECT_ATTRIBUTES` has two `SECURITY_*` pointer
fields.

`NtQueryInformationByName` is a user-mode binding — windows-sys links it to
`ntdll.dll`, not `ntoskrnl.exe`.

## 2. Allocated size: `AllocationSize`

Three candidates, and the reasons for rejecting two:

- `GetCompressedFileSizeW` is path-based (a full path re-resolution per
  file, unbatchable) and, for an ordinary uncompressed file, documented to
  return the logical size rather than the cluster-rounded one — which would
  erase exactly the small-file slack `st_blocks` exists to surface.
- `FSCTL_QUERY_ALLOCATED_RANGES` returns a range map, not a scalar, costs a
  `DeviceIoControl` plus an `ERROR_MORE_DATA` retry loop per file, and its
  own support table says ReFS: No.
- **`FILE_STANDARD_INFO.AllocationSize`** is cluster-granular by
  specification (MS-FSCC §2.4.38), sparse-aware, and — decisively — comes
  back *inside the directory listing* via `FILE_ID_EXTD_DIR_INFO`, at no
  per-entry cost.

libuv uses exactly this mapping as its `st_blocks` equivalent
(`src/win/fs.c`: `st_blocks = AllocationSize >> 9`). Cluster sizes are
powers of two ≥ 512, so the shift is exact.

**Measured 2026-07-26 on Windows Server 2022 (build 20348), NTFS:
`AllocationSize` IS compression-aware.** 4 MiB of repeating ASCII:

| | EndOfFile | AllocationSize | GetCompressedFileSizeW |
|---|---|---|---|
| uncompressed | 4 197 600 | 4 198 400 | 4 197 600 |
| after `compact /c` | 4 197 600 | **528 384** | 528 384 |
| 4 MiB random, compressed | 4 194 304 | 4 194 304 | 4 194 304 |

So `real` is honest on Compact-OS and `compact /c` files — the risk that
would have made camembert's headline number wrong on any system-drive scan
does not exist.

The same run also confirms why `GetCompressedFileSizeW` was the wrong
choice, which had been reasoned from the docs rather than observed: on the
*uncompressed* file it returns 4 197 600 — the logical size, not the
4 198 400 the file actually occupies. It erases the cluster slack.
`AllocationSize` reports it. Once the file is compressed the two agree.

Unlike btrfs, NTFS compression is a **per-file** attribute
(`FILE_ATTRIBUTE_COMPRESSED`, present in the listing for free), not a mount
option — so `path_on_compressed_mount` has no honest mount-level answer on
Windows and returns `false`.

## 3. Hardlink identity: fold, never truncate

MS-FSCC Appendix B fn.11, on NTFS: the low 48 bits are the MFT record index,
the next 16 a sequence number, and the high 64 bits **MUST be zero**. So the
low half *is* the classic file reference number — what `fsutil file
queryfileid` prints.

On ReFS the same footnote says the low 64 bits identify the file's **parent
directory** and the high 64 bits identify the file within it. Truncating
there gives every sibling in a directory the same inode and the hardlink
pass dedups the whole directory away. That is why the rule is fold, not
truncate:

```rust
fn fold_file_id(id: [u8; 16], folded: &AtomicBool) -> u64 {
    let lo = u64::from_le_bytes(id[0..8].try_into().unwrap());
    let hi = u64::from_le_bytes(id[8..16].try_into().unwrap());
    if hi == 0 { return lo; }          // NTFS: exact, and user-checkable
    folded.store(true, Ordering::Relaxed);
    lo.rotate_left(31) ^ hi.wrapping_mul(0x517c_c1b7_2722_0a95)
}
```

At 10 M hardlinked inodes the fold's collision probability is ~2.7e-6, and
its failure mode is the same as truncation's — two inodes fused — just 2^64
times rarer. `id_folded` surfaces through `ScanOutcome` so the UI can say
identity was folded rather than imply NTFS-grade precision.

Two sentinels mean *unknown*, not an inode: all-`0xFF` is "no unique ID
available", all-zero means the filesystem issues no IDs (FAT/exFAT/UDF).
Both force `nlink = 1`. Reading them as inodes would turn every file on a
scanned USB stick into one hardlink group.

`dwVolumeSerialNumber` is the `st_dev` analogue. It is assigned at format
time and survives sector-level clones, so two mounted volumes can present
the same serial — the same heuristic-not-sound status `st_dev` has.

## 4. Enumeration, and why not `std::fs::read_dir`

Not because of handles — because `WIN32_FIND_DATAW` has no allocation-size
field at all, so `read_dir` can never serve `Size::real`. (std's
`DirEntry::metadata()` on Windows *is* free, no syscall; it just does not
contain what we need, and hard-codes `number_of_links: None`.)

Instead, on the directory handle we have to open anyway:
`GetFileInformationByHandleEx(h, FileIdExtdDirectoryInfo, buf, 64 KiB)`,
walked by `NextEntryOffset` until `ERROR_NO_MORE_FILES`. Per entry that
yields name, `EndOfFile`, `AllocationSize`, `FileAttributes`,
`ReparsePointTag` and the 128-bit `FileId` — everything except the link
count, for zero per-entry syscalls.

The link count comes from `NtQueryInformationByName(FileStatInformation)`,
which its own Remarks describe as working "without opening the actual file".
Skip it for directories (the owner never reads a directory's nlink) and for
reparse points (never descended).

Per-directory syscall shape, Linux vs Windows:

| | open | list | stat |
|---|---|---|---|
| Linux | 1 `openat` | ⌈n/…⌉ `getdents64` | n `statx` |
| Windows | 1 `CreateFileW` | ⌈n/~1000⌉ `GetFileInformationByHandleEx` | n_files `NtQueryInformationByName` |

Same order. The rejected alternative — `CreateFileW` +
`GetFileInformationByHandle` + `CloseHandle` per entry — is 20-200× more
expensive per published measurements (wholetomato's benchmark; ripgrep#3293
measured 51 % end-to-end on Unreal Engine 5 from removing exactly this
pattern).

`CreateFileW` flags: `dwDesiredAccess = FILE_READ_ATTRIBUTES` reads metadata
"even if `GENERIC_READ` access would have been denied";
`FILE_FLAG_BACKUP_SEMANTICS` is mandatory to open a directory at all;
`FILE_FLAG_OPEN_REPARSE_POINT` is ignored on non-reparse-points so it is
safe to set always; all three `FILE_SHARE_*` bits, or a scanner collides
with anything holding a file open.

## 5. `MAX_PATH`

Canonicalising the root is sufficient, with three residual limits worth
writing down because they are not the ones people expect:

1. The 255-character **component** limit still applies; `\\?\` lifts only
   the total (32 767).
2. `\\?\` **disables normalisation**, so a joined name containing `/`, `.`
   or `..` becomes a literal broken component. Safe only because we build
   paths exclusively from listing names — never from user fragments.
3. UNC roots need `\\?\UNC\server\share`, not `\\?\\\server\share`.

`LongPathsEnabled` is not an escape hatch: it needs both the registry key
and a `longPathAware` manifest, and rustc embeds no such manifest by default
(rust-lang/rust#10512 open).

std's own rule is length-and-shape based — `LEGACY_MAX_PATH` is **248**, not
260, in `library/std/src/sys/path/windows.rs`.

## 6. Reparse points: classify by tag, never blanket-skip

| Tag | Value | Treatment |
|---|---|---|
| `IO_REPARSE_TAG_SYMLINK` | `0xA000000C` | `Kind::Symlink`, not followed — lstat parity |
| `IO_REPARSE_TAG_MOUNT_POINT` | `0xA0000003` | junction *and* volume mount point; `ExcludedReason::OtherFs`, never descended |
| `IO_REPARSE_TAG_APPEXECLINK` | `0x8000001B` | store-app alias; record, never descend |
| `IO_REPARSE_TAG_WOF` | `0x80000017` | **ordinary file** — Compact-OS compression |
| `IO_REPARSE_TAG_CLOUD*` | `0x9000?01A` | OneDrive placeholder — real file, real logical size |
| `IO_REPARSE_TAG_DEDUP` | `0x80000013` | server dedup stub — real file |
| `IO_REPARSE_TAG_LX_SYMLINK` | `0xA000001D` | WSL symlink; the `LX_*` family also gives `Kind::{Fifo,Char,Block,Socket}` free |
| anything else | | record, do not descend, `OtherFs` |

Blanket-skipping `FILE_ATTRIBUTE_REPARSE_POINT` would silently drop a large
slice of `C:\Windows` (WOF) and every OneDrive file.

**OneDrive hydration is not a hazard here**, but only by luck: placeholders
carry `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, and hydration is keyed to
*data* access. A scan never reads file contents. Any future feature that
does (hashing, preview) breaks this invariant — hence the comment in the
code.

There is no Windows `KernFs`; leave that `ExcludedReason` variant
Linux-only rather than inventing a meaning.

`--one-filesystem` maps onto `dwVolumeSerialNumber`
(`GetVolumeInformationByHandleW`, which returns the filesystem name in the
same call — needed anyway for §3's NTFS test). The boundary *signal*
differs: Linux gets it free from each entry's `st_dev`, Windows has to read
it from `ReparsePointTag == IO_REPARSE_TAG_MOUNT_POINT`.

**T1 refuses every mount point regardless of the flag**, including
same-volume junctions `--one-filesystem` would legitimately descend. A
junction can point at its own ancestor and camembert has no cycle detection;
refusing is the only setting that cannot loop. A junction-heavy tree
under-counts, and that is a documented T1 divergence.

## 7. Assumptions, all three measured

Verified 2026-07-26 by PowerShell P/Invoke on a Windows Server 2022 box
(build 20348, NTFS), not by reading. All three came back favourable; the
scripts are in the session scratchpad and the results are reproduced here
because they are the load-bearing facts of this design.

1. **`AllocationSize` reflects NTFS compression** — see §2's table.
2. **`OBJECT_ATTRIBUTES.RootDirectory` does give a directory-relative
   lookup for `NtQueryInformationByName`.** Microsoft documents the
   mechanism nowhere on that page; it works. Against an open directory
   handle with a bare leaf name:

   ```
   target.bin  : OK eof=1048576 alloc=1048576 nlink=2 attrs=0x20  tag=0x00000000
   hard.bin    : OK eof=1048576 alloc=1048576 nlink=2 attrs=0x20  tag=0x00000000
   realdir     : OK eof=0       alloc=0       nlink=1 attrs=0x10  tag=0x00000000
   missing.bin : ERR 0xC0000034 (STATUS_OBJECT_NAME_NOT_FOUND) -> win32=2
   ```

   So route 2 stands and the per-directory shape stays `openat`-like. Keep
   `nt_stat_supported` anyway: this proves the call works on *this* build
   and filesystem, not on Windows 10 1607 or on an SMB share.
3. **`OBJ_DONT_REPARSE` is not load-bearing — the call is already
   `lstat`.** With *and* without the flag, a symlink and a junction both
   report themselves rather than their target:

   ```
   sym.bin   with/without OBJ_DONT_REPARSE : attrs=0x420 tag=0xA000000C
   junction  with/without OBJ_DONT_REPARSE : attrs=0x410 tag=0xA0000003
   ```

   Set it anyway for explicitness, but do not build a safety argument on
   it; the belt-and-braces guard (never issue the call for entries whose
   listing already showed `FILE_ATTRIBUTE_REPARSE_POINT`) is the real one.

Two incidental findings worth writing down, because both are divergences
from Linux that will otherwise surface as "bugs":

- **Directories and reparse points report `EndOfFile = AllocationSize = 0`
  *in a listing*.** On Linux a directory has a genuine `st_size`/`st_blocks`
  and a symlink's `st_size` is its target's path length.
  `FILE_ID_EXTD_DIR_INFO` gives neither. For a symlink that is probably
  correct (the link text lives in the reparse buffer, not in file data — gap
  3 in HANDOFF, still unconfirmed); for a **directory it is simply not what
  the filesystem says**, and the same handle answers properly:
  `FileStandardInfo` on a directory returns its real index allocation, which
  is why the scan root — the one directory `open_root` sized by handle — was
  right while every subdirectory read 0.

  **Corrected 2026-07-27** (`Batch::dir_own_size`): the worker already opens
  every directory to enumerate it, so it asks that handle for the size and
  the owner applies it as a delta on the node its parent created. Measured
  on a `sub/` of 400 files with 38-character names: 0 B as a child, 196 608 B
  (48 INDX blocks) as a root; now 196 608 B either way. Verified byte-exact
  against an independent oracle — opening the directory's own
  `:$I30:$INDEX_ALLOCATION` stream by name and calling `GetFileSizeEx`,
  a different object through a different call — at 0/1/10/50/100/200/400/800
  entries: 0, 0, 4 096, 24 576, 49 152, 98 304, 196 608, 524 288 bytes, and
  camembert reports each of them exactly. The zeros are real: NTFS keeps a
  small index resident in the MFT record.

  Directories that are never opened — junctions, volume mount points,
  unknown reparse tags, and anything whose open failed — keep the listing's
  0, which is the honest answer when there is no handle to ask.
- **A 1-byte file reports `AllocationSize = 8`, not 0 and not one cluster.**
  MFT-resident files are accounted in bytes, not clusters. The earlier
  guess of "likely 0" was wrong. Report what the API says; do not invent an
  MFT-overhead correction (Explorer's own heuristic here is, per Raymond
  Chen, naive).

Fallback cost if (2) ever fails on some other build: no `nlink` means the
dump's `l` field is absent, freeable-2 D4's "the scan saw every link" test
fails closed, and the hardlink registry must key every non-directory entry
instead of only `nlink > 1` ones — roughly +24 bytes/file, ~240 MB at 10 M
files.

## 8. Errors

`ERROR_SHARING_VIOLATION` (32) and `ERROR_LOCK_VIOLATION` (33) — antivirus,
Office, backup agents — are the most common Windows scan errors after
access-denied, and std maps neither: both land in `Uncategorized`. A
`ErrorKind`-only mapping would therefore emit no `er` field for the errors
users see most.

So the mapping consults an explicit Win32 table **first**, falling back to
`ErrorKind`. This does not reintroduce the hazard the rule was written
against — reinterpreting a Win32 number *as* a Linux errno
(`ERROR_ACCESS_DENIED` is 5, and 5 is `EIO`: "your disk is dying" for a
permission denial). A table is a lookup, not a cast.

**Decided 2026-07-26: sharing violations get their own taxonomy entry**, not
a reuse of `EBUSY`. "Resource busy" is not what "an antivirus is holding
this file" means, and this is the error Windows users will see most. It is
the first Windows-only row in a Linux-numbered table; dump readers already
degrade an unrecognised `er` name to `None`, so it is additive on the wire.

Also recovered by the table: `ERROR_FILE_CORRUPT` (1392), the Windows `EIO`
and precisely what `Severity::Alert` exists for, and
`ERROR_CANT_RESOLVE_FILENAME` (1921) → `ELOOP`, which is otherwise
unreachable because `io::ErrorKind::FilesystemLoop` is **still nightly-only
at 1.88**.

`ERROR_NO_MORE_FILES` (18) is not an error — it is the enumeration
terminator.

## 9. Seam types

Mirrors `scan/linux.rs`: same six hooks, same two opaque types, `scan.rs`
untouched.

```rust
pub(crate) type Root = std::os::windows::io::OwnedHandle;

/// Canonicalized, `\\?\`-prefixed, NUL-terminated UTF-16 directory path.
pub(crate) struct WidePath(Vec<u16>);

#[derive(Clone, Copy)]
pub(crate) struct VolumeFacts {
    pub serial: u64,            // dwVolumeSerialNumber — the st_dev analogue
    pub native_64bit_ids: bool, // NTFS: high half of the file ID is zero
}

pub(crate) enum JobDir {
    Opened(Root),
    /// `parent \ name`. The Arc is the `JobFd::At(Arc<OwnedFd>, …)` analogue
    /// with the opposite resource profile: no descriptor is held, so the
    /// Linux worker's RLIMIT_NOFILE pathology does not exist — memory grows
    /// with queued-but-unopened directories instead.
    At(Arc<WidePath>, Vec<u16>),
}
```

`WorkerShared` differs from Linux by exactly four fields: `statx_supported`
becomes `nt_stat_supported`, `use_uring` disappears, `dev: u64` becomes
`vol: VolumeFacts`, and `id_folded` is added. `pending_jobs`, `next_token`,
`injector`/`stealers` and `abort` are identical, and the
`run`/`find_job`/`process_job`/`send_batch` skeleton should be a near-verbatim
port — `owner.rs` needs zero changes because `BatchEntry`'s field semantics
are unchanged.

`effective_threads` returns the existing `Media::Unknown` tier, honestly
labelled. The real analogue of `/sys/block/*/queue/rotational` is
`DeviceIoControl(IOCTL_STORAGE_QUERY_PROPERTY,
StorageDeviceSeekPenaltyProperty)` → `IncursSeekPenalty`; named here so
nobody re-researches it, but it is T2.

## 10. What this still gets wrong

1. NTFS-compression accounting is unresolved (§7.1) — the one that could
   make the headline number wrong.
2. Junctions are refused, not resolved (§6); junction-heavy trees
   under-count.
3. `ino` is probabilistic off NTFS (§3) — flagged at runtime, but a fold is
   a fold.
4. Volume serials collide on cloned volumes, so `--one-filesystem` can be
   fooled.
5. **No cross-check partner.** `tests/statx_engine.rs` proves the two Linux
   engines agree; `tests/scan.rs` cross-checks against `MetadataExt`. Neither
   has a Windows equivalent, because the very APIs that would serve as the
   oracle are the nightly-only ones. `fsutil file queryfileid` / `fsutil file
   layout` shelled out from a test is the only oracle available, which is
   exactly why it is worth doing.
6. `scripts/bench-compare.sh` is bash + Linux tools, so CLAUDE.md's
   before/after mandate is unenforceable on Windows. The enumeration choice
   in §4 rests on *published* measurements of a different workload, not on
   camembert's own tree — say so in the commit rather than implying
   otherwise.
7. **Alternate data streams are invisible.** Every size here is the unnamed
   `$DATA` stream; Explorer has counted ADS since 8.1. A file with a 2 GB ADS
   reports as small. `FileStreamInfo` would fix it at one more call per file.
8. Deduplicated volumes (Windows Server) report the stub, not the shared
   extent — the same class of problem as btrfs reflinks, and out of T1 scope.
9. `ui.rs`'s disk gauge is `statvfs`-shaped and needs `GetDiskFreeSpaceExW`;
   `ui/oracle.rs` uses `std::os::unix::fs::MetadataExt` directly. Both sit
   outside the scan seam but inside the port.
