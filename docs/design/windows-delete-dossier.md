# Deleting on Windows — dossier (2026-07-28)

Decision-ready dossier for making camembert *act* on Windows: today
deletion, the marking basket, the review modal and the freeable panel are
all `cfg(unix)` and absent there, so the tool names the fat directory and
then strands the user. **Not settled** — this is the displacement question,
what the Unix executor actually guarantees, measurement, options, an
adversarial attack on those options, and one recommendation, for a
co-design session. Nothing here is binding until it lands in a decisions
doc.

Every number below was measured on the user's own machine on 2026-07-27/28:
Ryzen 9 5950X (16C/32T), NVMe SSD, NTFS, Windows 11 Pro 10.0.26200,
**Microsoft Defender real-time protection ON and unmodified** throughout
(`AMServiceEnabled/RealTimeProtectionEnabled/BehaviorMonitorEnabled/
OnAccessProtectionEnabled` all True, engine 1.1.26060.3008). No security
setting was changed, no elevated shell was used, and the process ran at
medium integrity. Every destructive probe ran inside one scratch directory
under `%TEMP%`, behind a guard that panics on any path outside it; the
Recycle Bin was returned to its exact prior contents by the same runs that
filled it (verified by enumerating it and matching `System.Recycle.
DeletedFrom`). This is a **measurement document, not an implementation**:
no product code was written.

---

## Displacement

*What does this work displace, and does the thesis agree with that trade?*

On the table instead: **freeable phase 2 slice 3** (HANDOFF step 1),
**traversal-dedup Option D** (step 9), and the Windows port's own gap list
— above all gap 1, reparse points never entering the hardlink registry,
which is now a hash insert and which *moves totals on a Compact-OS system*.
And, landing concurrently, **reveal-in-Explorer** (`o`/`y`).

The honest answer has three parts, and the first two cut against this work.

**1. Reveal-in-Explorer covers most of the value at zero risk, and it is
better at the job than camembert would be.** Once the user is standing in
the right directory in Explorer, they get: a Recycle Bin, per-item conflict
resolution, a progress dialog that can be cancelled, an "in use" dialog
wired to the Restart Manager, and Ctrl-Z. camembert cannot beat any of
that. It can only reimplement it, worse, on the one code path in the
project that destroys data. Anyone arguing for a Windows executor has to
say what it adds over `o`, and the answer is narrower than "deletion".

**2. Two of the three surfaces that make the Linux executor *honest* cannot
exist on Windows.** The confirm modal there is not just a yes/no: it
carries the phase-1 open-file advisory (`/proc` sweep), the phase-2 reclaim
oracle (FIEMAP), and the confidence verdict built from both. On Windows
there is no `/proc` and no FIEMAP, so `freeable.rs`, `fiemap.rs` and
`confidence.rs` are gated out. A Windows delete confirm modal would be the
Linux one with the epistemics removed — which is precisely the shape the
thesis exists to refuse.

**3. What it genuinely adds is not deletion. It is the basket.** Explorer
cannot express "delete these thirty things scattered across nine
directories, as one reviewed batch, having just been shown what each one
costs". camembert's mark → `v` review → confirm flow can, and that gesture
is the thing `o` does not cover. That — plus the confirm-time `(dev, ino)`
re-check, which Explorer has no concept of — is the whole differentiated
surface.

So: the thesis agrees with the trade **only if the work restores the
epistemics rather than skipping them**, and §4 below is the load-bearing
finding on that point. The Windows Restart Manager gives a *better*
open-file advisory than Linux's `/proc` sweep does (it sees SYSTEM services
from an unelevated process; the `/proc` sweep sees 28 % of processes on a
desktop), and `SHQueryRecycleBinW` gives Windows its own honest answer to
"space you think you freed and did not". Neither of those destroys any
data, both are cheap, and **neither has been built**. That ordering — the
honest answers first, the destructive act second — is what makes this pass
the displacement test. Shipping the executor first would not.

One thing this must **not** displace: gap 1. It moves totals, which is the
kind of debt that gets worse the more surfaces read them.

---

## 1. What the Unix executor guarantees, and the Windows equivalent

`camembert-core/src/delete.rs` is the bar. Read precisely, it promises six
things and documents three residual holes.

### 1.1 The guarantees

| # | Unix guarantee | mechanism | Windows equivalent | verified |
|---|---|---|---|---|
| G1 | The walk is descriptor-relative, never a rebuilt path | scan-root fd held for the batch; `openat(.., O_DIRECTORY\|O_NOFOLLOW)` per component; `unlinkat` per entry | `NtOpenFile` with `OBJECT_ATTRIBUTES.RootDirectory` = a held directory handle and a bare leaf name, chained per component | **yes**, §3.1 |
| G2 | Nothing below the root is followed | `O_NOFOLLOW` on every `openat` below the root | `FILE_OPEN_REPARSE_POINT` (NT `CreateOptions 0x00200000`) — the same flag `scan/windows/worker.rs` already uses instead of `O_NOFOLLOW` | **yes**, §3.3 |
| G3 | An intermediate component swapped mid-walk cannot redirect it | a handle names an inode, not a name | identical: a Windows handle names a file object, and a rename of the directory it was opened through does not move it | **yes**, §3.1 |
| G4 | The live `(dev, ino)` must equal the identity the UI recorded at confirm time, or the entry is refused | `fstatat(AT_SYMLINK_NOFOLLOW)` before touching anything | `GetFileInformationByHandleEx(FileIdInfo)` → `FILE_ID_INFO { VolumeSerialNumber: u64, FileId: [u8;16] }`, on the handle we are about to delete through | **yes**, §3.4 |
| G5 | The stat-to-open window is closed: the freshly opened directory fd is `fstat`'d and compared before recursion | `fstat` on the new fd | free on Windows: the identity query *is* on the handle, so there is no second lookup to race | **yes**, §3.4 |
| G6 | A directory is emptied through descriptors, never a path, then removed | recursive `openat`+`unlinkat`, then `unlinkat(AT_REMOVEDIR)` | same shape; `FileDispositionInfo(Ex)` on a directory fails `ERROR_DIR_NOT_EMPTY` exactly as `rmdir` fails `ENOTEMPTY`, so the depth-first order is forced identically | **yes**, §3.6 |

### 1.2 The residuals it documents and accepts

- **The root's own path above the anchor is trusted.** The root is opened
  by path, following a symlink *as the root argument*, because that is the
  path the user named. Windows: identical, and camembert's Windows scan
  already opens the root by path.
- **Renames strictly within the marked subtree mid-walk** stay bounded to
  that subtree, because every descent is a fresh no-follow open from the
  parent descriptor. Windows: identical, and §3.1 measures it.
- **The identity anchor is captured at confirm time, not scan time.** A
  swap between the scan and the moment the UI records `(dev, ino)` is
  anchored to the swapped-in inode. Windows: identical, and *worse-behaved
  in one specific case* — see §3.4 on folded ReFS ids.
- **Freed space for surviving hardlinks is optimistic** and warned about in
  the dialog. Windows: the same, and the badge means something narrower
  there (windows-nlink-dossier §6.3), so the wording cannot be reused
  verbatim.

### 1.3 The one guarantee that has no Windows equivalent at all

There is a seventh property `delete.rs` never has to state because POSIX
gives it away: **`unlinkat` cannot fail because someone has the file open.**
On Windows it can, and §3.2 shows the failure is the *common* case, not the
tail. This is the single largest semantic difference and it drives most of
§5.

---

## 2. What was measured, in one table

Every row is something that was run. Probe sources are described in §9.

| # | question | measured answer |
|---|---|---|
| 2.1 | Does a handle-relative open + delete work? | **Yes.** `NtOpenFile(RootDirectory=dir handle, "victim.txt", DELETE\|SYNCHRONIZE)` + `SetFileInformationByHandle(FileDispositionInfo)` deletes it. Chaining (`root → "a" → "deep.txt"`, all `FILE_DIRECTORY_FILE`/`FILE_NON_DIRECTORY_FILE`) works. |
| 2.1b | Can a relative name escape upward? | **No.** `..\p1b\plain.txt` → `STATUS_OBJECT_NAME_INVALID` (0xC0000033, win32 123). `.\plain.txt` → 0xC000003A. `\??\C:\Windows\notepad.exe` → `STATUS_INVALID_PARAMETER` (0xC000000D). `sub\t.txt` **is** accepted (a backslash sub-path is legal), `sub/t.txt` is not. |
| 2.1c | Does a held directory handle survive a swap of its name? | **Yes.** `a` held → `a` renamed to `moved`, a fresh decoy `a` created → opening `target.txt` through the held handle returns the ORIGINAL file id, not the DECOY's. |
| 2.2 | Does `FILE_DISPOSITION_POSIX_SEMANTICS` unlink a file another process holds open? | **Only if that process granted `FILE_SHARE_DELETE`.** Holder shares R/W/D: classic → name stays visible until the last close, a re-open gets `ERROR_ACCESS_DENIED` (delete-pending); POSIX → **name gone immediately**, re-open gets `ERROR_FILE_NOT_FOUND`. Holder shares READ only, or nothing: **the open-for-DELETE itself is refused, `STATUS_SHARING_VIOLATION` (0xC0000043 / win32 32), for both flavours.** |
| 2.2b | A running executable? | Open-for-DELETE succeeds; the disposition fails `ERROR_ACCESS_DENIED` (5) with **and** without POSIX semantics. The image section wins. |
| 2.3 | Does `FILE_OPEN_REPARSE_POINT` delete the junction and not its target? | **Yes.** With the flag the handle's `FileId` is the junction's own (≠ target's); the disposition removes the junction, and `real_target\CANARY.txt` survives. **Without** the flag the handle's `FileId` **is the target's** — that is the whole hole. `RemoveDirectoryW` on a junction also removes the link, not the target. |
| 2.3b | Symlinks? | `mklink /D` → *"You do not have sufficient privilege"*. Developer Mode is off on this box, so directory/file symlinks could not be created and the symlink case is **untested** (HANDOFF Windows gap 3 is the same blocker). |
| 2.4 | Does `FILE_ID_INFO` match what the scan recorded? | **Exactly.** 4/4 entries (including a `mklink /H` pair) have listing `FileId` == by-handle `FILE_ID_INFO.FileId`, and both fold identically through `fold_file_id`. `VolumeSerialNumber` = `0x80123d27123d2396`, whose low 32 bits are `GetVolumeInformationW`'s `0x123d2396`. |
| 2.4b | A volume issuing no unique ids? | On the FAT32 volume present on this box, `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` — the scan's *only* listing call — fails with `ERROR_INVALID_PARAMETER` (87). `FILE_ID_INFO` by handle still answers, returning `1` for the root. So the scan produces no entries there at all, and the delete question does not arise; where the `is_sentinel_id` path *does* fire, the scan stores `ino = 0`, and an executor must refuse on that value. |
| 2.5 | Does `IFileOperation` work from a non-elevated console app with no window? | **Yes.** `CoInitializeEx(APARTMENTTHREADED)` + `CoCreateInstance(FileOperation)` + `DeleteItem` + `PerformOperations` returns `S_OK`, no UI, no elevation. |
| 2.5b | Recycle vs permanent, 1000 × 4 KiB files | recycle **4888–5556 ms** (4.9–5.6 ms/file); permanent through the *same* API **1609–1613 ms** (1.61 ms/file) → **3.0–3.4×**; permanent through the handle route **195–261 µs/file** → recycling is **~25× the cost of the delete camembert would otherwise do**. Control: creating those files costs 441 µs each on this box, so the filter stack, not the disk, sets the floor. |
| 2.5c | Does it report per-item failures usefully? | **Only through a progress sink.** A file held with `FILE_SHARE_READ` only: `PerformOperations` returned **`S_OK`** while `GetAnyOperationsAborted()` returned **true** and the sink's `PostDeleteItem` carried `hr = 0x80270027` (`COPYENGINE_E_SHARING_VIOLATION_SRC`). A caller that only checks the `PerformOperations` HRESULT sees success on a batch that deleted nothing. |
| 2.5d | Was an item actually recycled, or permanently deleted? | `PostDeleteItem`'s `psiNewlyCreated` is the discriminator: non-null (the `C:\$Recycle.Bin\<SID>\$R…` item) on every recycled item, null on every permanently deleted one. This is the only in-band signal. |
| 2.5e | `\\?\` paths — what the Windows backend carries everywhere | **Rejected.** `SHCreateItemFromParsingName("\\?\C:\…")` → `E_INVALIDARG` (0x80070057), and the operation then fails `E_UNEXPECTED`. The same file by plain path recycles fine. A **339-character** plain path is accepted and recycles. |
| 2.5f | A directory | Recycled as **one** item: 200 files + their directory in 348–524 ms, one `PostDeleteItem` callback. Per file that is ~2 µs — recycling a tree is a rename, recycling its files one by one is not. |
| 2.5g | Read-only file | Recycles cleanly (`hr = S_OK`), no flag needed. |
| 2.5h | Is there a Recycle Bin on this volume? | `SHQueryRecycleBinW` succeeds **read-only** on every fixed volume here, including the FAT32 one, and returns items + bytes. On this machine `C:\` holds **66 items / 6 264 307 348 bytes = 5.83 GiB** of already-"deleted" data. It returns `S_OK` even where the bin is empty, so it is a *size* oracle, not an *availability* oracle. |
| 2.6 | `RemoveDirectoryW` on a non-empty directory | `ERROR_DIR_NOT_EMPTY` (145). `FileDispositionInfo` **and** `FileDispositionInfoEx(POSIX)` on the same directory also return 145 — POSIX semantics does not buy recursive removal. Depth-first is forced. |
| 2.6b | `FILE_ATTRIBUTE_READONLY` on a file | `DeleteFileW` → `ERROR_ACCESS_DENIED` (5). Opening for DELETE **succeeds**; `FileDispositionInfo` → 5; `FileDispositionInfoEx(DELETE\|IGNORE_READONLY_ATTRIBUTE)` → **OK**. |
| 2.6c | `FILE_ATTRIBUTE_READONLY` on a directory | `RemoveDirectoryW` → 5; classic disposition → 5; `Ex(DELETE\|IGNORE_READONLY_ATTRIBUTE)` → **OK**. |
| 2.7 | Hardlinks | Deleting one name leaves the sibling present at full size, as on Unix. |
| 2.8 | The name round-trip the README calls the blocker | UTF-16 → `OsString::from_wide` → `as_encoded_bytes` (what the tree interns) → a real WTF-8 decoder → UTF-16 is **byte-exact for lone surrogates**, and deleting by the recovered name works. Today's lossy decoder produces a **different** name for exactly those cases. Non-WTF-8 input (`0xFF`, a truncated 2-byte lead, an overlong `C0 AF`, a bare continuation) is **refused** by the decoder rather than mangled. |

### 2.9 The one number that should change the design

**`FILE_DISPOSITION_POSIX_SEMANTICS` is not `unlink`.** It changes *when
the name disappears*, not *whether you are allowed to delete*. The
gatekeeper is the share mode every existing holder chose, and the flag has
no influence over that. On Windows "the file is in use" therefore remains
a hard failure, and the brief's worry — "a delete that fails half-way
through a tree is worse than one that does not start" — is real and is not
solved by the 1709 flag. What POSIX semantics *does* buy is worth having
anyway: with it, a successful delete of a file that a well-behaved holder
shares is immediately invisible, so the tree the executor is walking does
not sprout delete-pending zombies that make the parent's `RemoveDirectory`
fail. Use it; do not sell it as `unlink`.

---

## 3. The mechanics, in detail

### 3.1 The `unlinkat` analogue exists and is exactly the shape the scan already uses

`scan/windows/worker.rs::query_nlink` → `winlink::links_at` already proves
`OBJECT_ATTRIBUTES.RootDirectory` works for a *query*. It works identically
for an open with `DELETE` access, and `SetFileInformationByHandle` accepts
the resulting NT handle without ceremony (a `HANDLE` is a `HANDLE`).

Three properties matter and all three were measured:

1. **Chaining works.** `root → openat("a", FILE_DIRECTORY_FILE) →
   openat("deep.txt", FILE_NON_DIRECTORY_FILE)` deletes `deep.txt` without
   ever naming `C:\...\a\deep.txt`.
2. **The object manager refuses upward escape.** `..\` is
   `STATUS_OBJECT_NAME_INVALID`, not a traversal. It does accept a
   backslash-separated *sub*-path, which is harmless — a Windows filename
   cannot contain a backslash, so a name coming out of the tree can never
   be one — but an executor should still pass one component at a time,
   because that is what makes property 3 true at every level.
3. **A handle survives a rename of its name.** This is G3, and it is the
   reason the descriptor-relative shape is worth the trouble. Measured:
   after `a` is renamed away and a decoy `a` created, the held handle still
   resolves `target.txt` to the original file id.

Notably, the nlink dossier §2.2 measured that `RootDirectory`-relative
lookup buys **nothing** in performance (48.31 µs vs 47.91 µs absolute). It
is not a performance feature here either. It is the *only* way to get G1,
G3 and G5, and that is the entire argument for it.

### 3.2 Sharing violations are the Windows failure mode

Three holder share modes, two disposition flavours, six runs:

| holder's share mode | classic `FileDispositionInfo` | `FileDispositionInfoEx(POSIX)` |
|---|---|---|
| `READ\|WRITE\|DELETE` | set OK; name visible until holder closes; a re-open gets win32 5 | set OK; **name gone at once**; a re-open gets win32 2 |
| `READ` only | **open refused, 0xC0000043 (win32 32)** | **open refused, 0xC0000043 (win32 32)** |
| nothing (exclusive) | **open refused, 0xC0000043 (win32 32)** | **open refused, 0xC0000043 (win32 32)** |

Plus: a running executable's image opens for DELETE but refuses the
disposition with win32 5, both flavours.

Design consequences:

- The executor's per-entry error taxonomy needs
  `WIN_SHARING_VIOLATION` — which `errno.rs` already has, for exactly this
  reason (HANDOFF, "the taxonomy grows non-POSIX rows").
- A partially failed directory removal is normal, not exceptional. The
  Unix executor already handles this ("Failures never abort the batch:
  each entry gets its own `EntryOutcome`") and the accounting is already
  documented as overestimating in the safe direction when a recursive
  drain half-succeeds. That contract carries over unchanged; what changes
  is how *often* it fires.
- Because it fires often, the **pre-flight** advisory (§4) stops being a
  nicety and becomes the difference between "camembert told me this would
  fail" and "camembert half-deleted my build directory".

### 3.3 Reparse points

The `O_NOFOLLOW` analogue is real and it is the same flag the scan already
passes. The measured contrast is stark enough to be the test that guards
it forever:

```
OPEN_REPARSE_POINT: id=[b9,45,06,..] (target id=[fd,3a,06,..]) same=false
  disposition -> Ok(())
  junction exists=false | target dir exists=true | CANARY exists=true
NO reparse flag:    id=[fd,3a,06,..] == target id? true   <-- the hole
```

`RemoveDirectoryW` on a junction also removes only the link, so both routes
are safe; the by-handle route is the one that composes with G4.

**Symlinks were not testable** (no Developer Mode, no elevation). The
reparse-tag handling in `classify` treats `IO_REPARSE_TAG_SYMLINK` as
`Kind::Symlink` and never descends it, and the same
`FILE_OPEN_REPARSE_POINT` flag is what makes an open of one refer to the
link — but that is inference, not measurement, and any implementation must
land with a test that runs where symlink creation is permitted (the
`windows-2025` CI runner is a candidate: GitHub-hosted Windows runners run
elevated).

### 3.4 Identity

`FILE_ID_INFO` gives `(VolumeSerialNumber: u64, FileId: [u8;16])` and it
matches the directory listing's `FileId` byte for byte on NTFS — measured
on four entries including a hardlink pair. That makes G4 mechanically
available: the executor opens the target handle-relative, asks
`GetFileInformationByHandleEx(FileIdInfo)` on that very handle, and
compares.

Three cautions the implementation owes:

1. **Compare the full 128 bits, not the fold.** `fold_file_id` collapses a
   ReFS-shaped id to 64 bits with a documented ~2.7e-6 collision
   probability at 10 M inodes. For *grouping* hardlinks that is fine; for
   *deciding what to destroy* it is not. Since `DeleteTarget::expected` is
   captured at confirm time anyway (not from the packed 32-byte node), it
   can carry the full `[u8;16]` and the `u64` volume serial. Do that.
2. **`ino == 0` must refuse, not delete.** The scan writes `0` for
   sentinel ids (`is_sentinel_id`), which means "this volume issues no
   identity". `SkipReason::IdentityMismatch` is the wrong word for it; it
   needs its own reason — call it `IdentityUnavailable` — because "we
   could not confirm" and "it changed" are different facts and a user
   reading a refusal deserves the right one. The dossier's rule stands:
   **a target whose identity cannot be confirmed is refused, never
   deleted.**
3. **The volume serial is 64-bit here and 32-bit from
   `GetVolumeInformationW`.** Measured: `0x80123d27123d2396` vs
   `0x123d2396`. Compare like with like; the scan's `WorkerShared::vol`
   should be the one the executor re-reads, or the comparison is theatre.

### 3.5 The Recycle Bin

`IFileOperation` works unelevated with no window. What it costs, and what
it hides:

| operation, 1000 × 4 KiB files | total | per file |
|---|---|---|
| `NtOpenFile(rel)` + `FileDispositionInfoEx(POSIX)` | 195–261 ms | **0.20–0.26 ms** |
| `NtOpenFile(rel)` + the `FILE_ID_INFO` identity check + disposition | 207–245 ms | **0.21–0.25 ms** |
| `DeleteFileW` by full path (what `std::fs::remove_file` does) | 345–417 ms | 0.35–0.42 ms |
| `IFileOperation`, permanent (no `FOF_ALLOWUNDO`) | 1609–1613 ms | 1.61 ms |
| `IFileOperation`, recycle (`FOF_ALLOWUNDO\|FOFX_RECYCLEONDELETE`) | 4888–5556 ms | **4.9–5.6 ms** |

Two readings, both design inputs. **The identity re-check is free** — it is
inside the noise of the open it rides on, which is what makes G4 affordable
at any batch size. And **recycling costs 25× a permanent delete**: a 10 000
-file selection is 2.6 s permanent and ~55 s recycled, on an NVMe box with
nothing else running. Recycling a *directory* is cheap (one rename), so the
cost is specifically per top-level item, which is exactly the shape a
marking basket produces.

Four behaviours matter more than the timings:

- **`PerformOperations` returning `S_OK` does not mean anything was
  deleted.** Measured against a locked file: `S_OK`, with the failure only
  visible in `GetAnyOperationsAborted()` and in the sink's per-item
  `hr = 0x80270027`. Any implementation must implement
  `IFileOperationProgressSink` — 16 methods — or it is reporting fiction.
- **`\\?\` paths are rejected.** camembert's Windows backend carries
  `\\?\`-prefixed `WidePath` everywhere. The prefix has to be stripped for
  the shell, and the fact that a 339-character plain path *does* work
  means the prefix is not load-bearing for length here — but that is one
  machine's long-path policy, not a guarantee.
- **`psiNewlyCreated` is the only proof the item went to the bin.** Which
  means the "was this reversible?" question is answerable — but only
  after the fact, per item, and only if you wrote the sink.
- **The bin is not free space.** `SHQueryRecycleBinW` says `C:\` on this
  box holds **5.83 GiB in 66 items**. Every byte of that is space a user
  believes they released.

### 3.6 Directories and the read-only attribute

Depth-first is forced: `FileDispositionInfo`, `FileDispositionInfoEx` and
`RemoveDirectoryW` all return `ERROR_DIR_NOT_EMPTY` on a non-empty
directory. So the Unix recursion shape ports unchanged — snapshot the
names, recurse into real directories through fresh no-follow opens, delete
leaves, then the directory.

`FILE_ATTRIBUTE_READONLY` is the genuinely new obstacle, and it applies to
**directories too**, which surprises people coming from `chmod`:

| target | `RemoveDirectoryW` / `DeleteFileW` | classic disposition | `Ex(DELETE\|IGNORE_READONLY_ATTRIBUTE)` |
|---|---|---|---|
| read-only file | win32 5 | win32 5 | **OK** |
| read-only empty directory | win32 5 | win32 5 | **OK** |

**Recommendation: use the flag, never clear the attribute.**
`SetFileAttributesW` to strip `READONLY` and then delete is the classic
workaround and it is wrong twice: it is two operations with a window
between them, and if the delete then fails the file is left *changed* —
camembert would have mutated a file it refused to delete. `FILE_DISPOSITION
_FLAG_IGNORE_READONLY_ATTRIBUTE` does it in one call with no residue, and
it is available on the same Windows 10 1709+ floor as POSIX semantics.
What the UI owes the user is a word, not a prompt: the confirm modal's
per-entry list should mark read-only entries, because on Windows that
attribute is often the only thing standing between a user and a mistake
they meant to make hard.

Where `FileDispositionInfoEx` is unavailable (pre-1709, or a filesystem
that does not implement it — the scan already has an `is_unsupported_
status` notion for exactly this class of answer), the executor should fall
back to the classic disposition and **refuse read-only entries with a named
reason** rather than clearing anything.

---

## 4. The open-file advisory has no `/proc` — and the substitute is better

Phase 1's pre-deletion warning ("these files are open, deleting frees
nothing yet") is a `/proc/[pid]/fd` sweep whose measured coverage on a
desktop is **28 % of processes** (freeable-decisions, research §4). Two
Windows analogues were tested.

### 4.1 Restart Manager — usable, unprivileged, and it sees SYSTEM

`RmStartSession` / `RmRegisterResources` / `RmGetList` from a medium-
integrity, non-elevated process:

| what | result |
|---|---|
| a file held by a **different** process (a spawned child, `FILE_SHARE_READ`) | found, with pid, `strAppName`, `ApplicationType`, `bRestartable` |
| a file nobody holds | `needed = 0` — a clean negative, not a silent one |
| `C:\Windows\explorer.exe` | 1 app, *"Windows Explorer"* |
| `C:\Windows\System32\svchost.exe` | **104 distinct services**, running as SYSTEM / LOCAL SERVICE / NETWORK SERVICE, enumerated from an unelevated process |
| `C:\Windows\System32\drivers\ntfs.sys` | 0 — kernel-loaded files are invisible to it |
| `C:\Windows\System32\ntdll.dll` | `RmGetList` → win32 6 (`ERROR_INVALID_HANDLE`); registration succeeded, the query did not |

Cost, and it is the number that decides where it can run:

| registered files | `RmStartSession` | `RmRegisterResources` | `RmGetList` |
|---|---|---|---|
| 1 | 0.3 ms | 0.2 ms | **50.5–90 ms** |
| 10 | 0.3 ms | 0.2 ms | 50.5 ms |
| 100 | 0.2 ms | 0.3 ms | 143.8 ms |
| 1000 | 0.3 ms | 1.2 ms | 285.4 ms |

So: a fixed ~50 ms floor, sublinear growth, and a whole basket costs under
300 ms. One caveat for UI budgeting: the **first** `RmGetList` in a fresh
process measured **434.6 ms** (the `RmSvc` service warming up); every later
one in the same process was in the table above. That is off-thread work with the same "advisory, may not have
landed yet" contract freeable D6 already writes down (and freeable
attack-a finding 4 already anticipated the race). **Coverage is
categorically better than Linux's**: the SYSTEM-service case is precisely
the multi-user blind spot the `/proc` sweep's coverage line exists to
apologise for.

Two honest caveats it must carry: kernel-held files (`ntfs.sys`) are
invisible, and some system files refuse the query outright (`ntdll.dll`,
win32 6) — so an *absent* warning still is not a promise, and the panel
wording must say "the Restart Manager saw N holders" rather than "nothing
holds these".

### 4.2 The system handle table — enumerable, and not usable

`NtQuerySystemInformation(SystemExtendedHandleInformation)` **succeeds
unelevated**: 191 065 handles across 375–385 processes, ~8 MiB of buffer.
That is where the good news stops.

- `OpenProcess(PROCESS_DUP_HANDLE)` is granted on 212–222 of 374–384
  processes — **57 %**, better than `/proc`'s 28 %, but the rest are
  exactly the processes an admin cares about.
- Turning handles into *paths* is the part that does not work. Over the
  first 60 000 entries: 2 941 duplicated, 70 of them disk files, 69
  resolved to a path, in 8 ms. Over the full table it **did not
  complete**: a run was killed at 120 s having processed ~88 000 of
  191 065 entries, and an earlier attempt blocked outright — the classic
  `DuplicateHandle` + name-query hang, which a `GetFileType == FILE_TYPE_
  DISK` pre-filter did not prevent.

A sweep that takes minutes, can block indefinitely, and still cannot see
43 % of processes is not a foundation for a UI advisory. **Use the Restart
Manager; do not build this.** Recording it here so nobody spends a day
rediscovering it.

### 4.3 The Windows freeable panel writes itself

Linux's `f` panel answers "space that is deleted but not yet free". The
Windows question with the same shape is **the Recycle Bin**, and
`SHQueryRecycleBinW` answers it read-only, per volume, unelevated, in one
call: on this machine, 5.83 GiB. That is a real, checkable, honest
answer no other Windows disk tool surfaces, it costs nothing, and it
destroys nothing.

It is also a **precondition**: any option that recycles instead of deleting
manufactures exactly this kind of invisible unfreed space, so the meter has
to exist before the tap is opened.

---

## 5. Options

Common ground across A/B/C: the executor is handle-relative
(§3.1), never follows a reparse point below the root (§3.3), re-checks
`(volume serial, 128-bit file id)` at confirm time and refuses on mismatch
or on an unavailable identity (§3.4), removes directories depth-first
(§3.6), uses `IGNORE_READONLY_ATTRIBUTE` rather than mutating attributes,
and reports per entry with `WIN_SHARING_VIOLATION` in the taxonomy. The
axis is **what "delete" means to the user, and what camembert may then
claim about freed space.**

### Option A — Recycle Bin only

`IFileOperation` with `FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE`, a progress
sink for per-item results, `\\?\` stripped from every path.

- **Cost.** 4.9–5.6 ms/file measured; 25× the handle route. A
  1000-file basket is 5.5 s, a 10 000-file one ~55 s, on a fast box with
  Defender in the stack. Directories are cheap (one rename), loose files
  are not.
- **Reversibility.** Real, and it is the strongest argument for it. Every
  bug in the new code — a wrong node, a mis-ordered batch, a botched name
  decode — becomes recoverable instead of terminal.
- **What it gives up: G4.** `IFileOperation` takes a shell item, i.e. a
  path. There is no way to say "recycle this only if it is still file id
  X". The confirm-time identity re-check, which is the Unix executor's
  headline guard, **cannot be enforced through this API**. A post-hoc check
  is possible (a same-volume recycle preserves the file id, and
  `psiNewlyCreated` hands you the bin item to interrogate) but it verifies
  after the fact, which is a different and weaker promise.
- **What it gives up: honesty about freed space.** This is the thesis
  problem the brief names, and it is worse than "the gauge is briefly
  stale": the space is *never* freed until the user empties the bin, which
  many users never do — 5.83 GiB of evidence on this machine.
  Mitigable — do not touch the freed figure, say "moved to the Recycle
  Bin", and show the bin's size in the panel (§4.3) — but only mitigable if
  §4.3 exists first.
- **Availability.** Untestable here in the negative: no network share, no
  removable media, no elevation to create either. Documented behaviour is
  that a volume without a bin permanently deletes instead, and with
  `FOF_NOCONFIRMATION` it does so silently. `SHQueryRecycleBinW` returns
  `S_OK` even on an empty bin, so it is not an availability test;
  `psiNewlyCreated == null` is the only reliable signal, and it arrives
  after the file is gone. **A design that promises reversibility while
  being unable to check it in advance is the worst failure mode in this
  dossier.**
- **Dependency.** `windows-sys` (the T1 dependency) exposes no COM
  interfaces. Either add the `windows` crate — a much larger dependency —
  or hand-roll `IFileOperation`, `IShellItem` and a 16-method
  `IFileOperationProgressSink` vtable. Neither is free, and the second is
  a lot of `unsafe` in the module that destroys data.

### Option B — Permanent only, mirroring Unix

Handle-relative walk, `FileDispositionInfoEx(DELETE | POSIX_SEMANTICS
[| IGNORE_READONLY_ATTRIBUTE])`, classic `FileDispositionInfo` fallback.

- **Cost.** 0.20–0.26 ms/file, identity check included. A 1000-file basket
  is 0.2 s.
- **Guarantees.** All six of §1.1, measured. It is the only option that
  keeps G4.
- **Honesty.** The freed figure is true the moment it is printed, which is
  what the disk gauge and every downstream number assume.
- **Dependency.** None beyond `windows-sys`, which is already T1. One new
  `unsafe` surface (`NtOpenFile` + `SetFileInformationByHandle`), sitting
  next to `winlink.rs`'s existing one.
- **The cost, stated plainly.** On the platform where the Recycle Bin is a
  cultural default, camembert would delete permanently. A user who has
  internalised "Windows delete is undoable" and skims the confirm modal
  loses data. That is not a hypothetical; it is how the platform's users
  are trained.
- **Second cost.** Sharing violations mean a batch can half-succeed, and
  there is no undo for the half that succeeded. §4.1's advisory reduces
  this to a warned-about outcome rather than a surprise, but does not
  remove it.

### Option C — Both, the destructive one behind an explicit gate

Ship B as the mechanism; add the bin as a mode. Two sub-shapes, and the
difference matters more than the option:

- **C1, recycle by default, permanent behind a flag.** Matches platform
  expectation; makes camembert's freed figure a lie by default unless the
  bin panel is there to redeem it; gives up G4 on the default path.
- **C2, permanent by default, recycle behind `--trash` / a `T` toggle in
  the confirm modal.** Keeps every guarantee on the default path; offers
  reversibility to those who ask; the mode is visible in the modal's own
  wording ("delete permanently" vs "move to the Recycle Bin — nothing is
  freed until you empty it").
- **Cost of either.** Two executors, two error taxonomies, two sets of
  tests, and a COM dependency that only one path uses. That is a real
  maintenance burden on the module that must never be subtly wrong.

### Option D — Keep Windows read-only; lean on reveal-in-Explorer

No executor. `o`/`y` opens Explorer at the selected row. Optionally add
Option 6 from HANDOFF's next-steps list — composable stdout output of the
marked selection — so a user can pipe camembert's basket into
`Remove-Item` themselves.

- **Risk of destroying data: zero.** No other option can say that.
- **What the user loses:** the batch gesture. Explorer cannot delete thirty
  things scattered over nine directories as one reviewed operation. That
  is the whole of what §Displacement identified as differentiated.
- **What it costs to keep:** the README already documents deletion as
  absent-and-compiled-out, so this is free today and stays free.
- **What it fails at:** it is an honest answer to "where did my disk go"
  and a shrug at "so now what". For a tool whose pitch is *honest answers
  to real questions*, "go use another program" is a defensible answer
  exactly once — and reveal-in-Explorer is that once.

This is **not** a strawman. If the co-design session concludes that the
Restart Manager advisory and the Recycle-Bin panel (§4) are the whole win
here, D plus those two is a coherent, cheap, zero-risk product, and this
dossier would have been worth writing to reach it.

---

## 6. Attack

### Attack A (Recycle Bin only)

1. **FATAL — it cannot honour the confirm-time identity check.** G4 is the
   guard `delete.rs` was rewritten to add (HANDOFF: "the intermediate-
   symlink TOCTOU and a real-directory swap of the top-level target are
   both closed"). A shell-item API takes a path and re-resolves it, which
   is the exact shape the Unix executor abandoned as unsafe. Shipping a
   Windows executor that re-introduces it, on the platform whose scan
   *already* walks by path (worker.rs: "That loses the Linux backend's
   immunity to hostile symlink swaps mid-walk"), doubles down on the port's
   one known weakness.
2. **SERIOUS — it cannot verify in advance that it is reversible.** The
   whole case for A is undo. Measured: the only signal that an item was
   recycled rather than nuked is `psiNewlyCreated`, delivered per item,
   after the fact. On a volume without a bin, `FOF_ALLOWUNDO |
   FOF_NOCONFIRMATION` silently permanently deletes — untestable on this
   box, and precisely the configuration a user reaches for when clearing a
   USB stick or a network scratch area.
3. **SERIOUS — `PerformOperations` lies.** `S_OK` with nothing deleted,
   measured. An implementation that omits the sink reports success on a
   failed batch. That is a 16-method COM interface of ceremony standing
   between camembert and a correct failure count.
4. **SERIOUS — the freed figure becomes false by construction.** The disk
   gauge, the `freed` tally in `DeleteReport`, the toast, and every
   downstream reading of `apply_removal` would all describe space that is
   still allocated. This is not a rounding error; it is the class of bug
   the project's whole pitch is built against.
5. **ANNOYING — 25× slower, and the slowness is per top-level item**, which
   is exactly the axis a marking basket scales on.
6. **ANNOYING — the dependency.** `windows-sys` has no COM; `windows` is a
   much larger tree. The nlink decision made `windows-sys` a deliberate T1
   choice; A quietly reopens that.

### Attack B (permanent only)

1. **SERIOUS — it is culturally wrong on this platform, and the cost of
   being wrong is unrecoverable.** Windows users are trained that delete
   means "recycle". Every mitigation available (a confirm modal, an
   explicit word, a basket review) is a mitigation camembert *already has*
   on Linux and which did not stop this dossier from being commissioned as
   a design-before-code exercise. Being right about semantics does not make
   a lost file come back.
2. **SERIOUS — a half-completed batch has no undo.** Sharing violations
   are common (§3.2), and unlike Unix there is no way to finish the job.
   Deleting `node_modules` while the editor's file watcher holds a handle
   leaves a partly-gutted tree and an error list. Recoverable in the sense
   that nothing valuable was lost; corrosive in the sense that the user's
   next move is `rmdir /s` in a shell, having learned camembert cannot
   finish what it starts.
3. **SERIOUS — the epistemics are thinner than Linux's and the modal must
   say so.** No FIEMAP means no reclaim oracle, no exclusive floor, no
   confidence verdict. A Windows confirm modal that looks like the Linux
   one but silently omits three of its four evidence lines is *worse* than
   one that visibly has less to say. The Restart Manager advisory (§4.1)
   restores one of them and should be treated as a hard prerequisite, not
   a follow-up.
4. **ANNOYING — `IGNORE_READONLY_ATTRIBUTE` needs Windows 10 1709+.** The
   fallback path (classic disposition, refuse read-only entries) is a
   second code path that almost nobody exercises, which is the exact shape
   of the `known_names_round_trip` class of bug called out in HANDOFF. It
   needs a test that forces it, not a `cfg`.
5. **ANNOYING — POSIX semantics is sold on its name.** Anyone reading
   `FILE_DISPOSITION_FLAG_POSIX_SEMANTICS` in the source will assume it
   means `unlink`. §2.9 says it does not. If that is not written on the
   call site, someone will later "simplify" the sharing-violation handling
   away.

### Attack C (both)

1. **SERIOUS — two executors on the one module that destroys data.** The
   Unix executor is ~540 lines with five named guards and a residual-window
   section. C means that, plus a COM path with a different error model
   (HRESULT + sink), a different identity story (post-hoc), and a different
   failure taxonomy — and the UI must present both truthfully. The cost is
   not the code; it is that two paths means the less-used one rots.
2. **SERIOUS — a mode toggle on a destructive action is a footgun in its
   own right.** If the modal can say either "delete permanently" or "move
   to the Recycle Bin", then the single most important sentence in the
   product changes depending on state the user set earlier, possibly in a
   config file. C2 mitigates this by making the default the safe-to-
   describe one and putting the mode in the modal's own text; C1 does not.
3. **ANNOYING — it defers the hard question rather than answering it.**
   "Both, behind a flag" is what a design says when it does not want to
   choose. The choice still has to be made, because the *default* is what
   99 % of users get.

### Attack D (stay read-only)

1. **SERIOUS — it declines the one thing the user asked for.** The brief
   opens with "the user wants to be able to act on Windows, not just look".
   A dossier that recommends not acting had better be very sure, and its
   evidence is one honest sentence — Explorer is better at deleting — plus
   a lot of risk-aversion.
2. **SERIOUS — it leaves the marking basket dead code on Windows
   forever.** Mark/review/confirm is a designed, tested, shipped surface.
   Keeping it compiled out on a supported platform is a permanent seam in
   the UI, the keymap, the palette and the help, and every future TUI
   change pays for it.
3. **ANNOYING — reveal-in-Explorer's value is not measured.** Nobody has
   watched a user try to act on a camembert finding through `o`. This
   dossier asserts it covers most of the value; that assertion is the
   weakest link in its own displacement argument.
4. **COSMETIC — it is not actually zero-cost.** The README already carries
   a paragraph explaining why deletion is absent, and that paragraph gets
   less true as this dossier's measurements accumulate (§2.8 in
   particular: the WTF-8 decoder it says nobody has written now exists as
   sixty lines of probe code with a passing round-trip).

### Cross-cutting

- **The name round-trip is a hard prerequisite for A, B and C.**
  `tree.rs::os_name_from_bytes` is deliberately lossy on Windows and its
  doc comment says why: interned bytes may have come from a dump written
  on another platform, so `from_encoded_bytes_unchecked` cannot be
  justified. §2.8 measures the fix: a real WTF-8 → UTF-16 decoder that
  round-trips lone surrogates exactly and **refuses** non-WTF-8 input.
  That is safe code, it needs no `unsafe`, and it turns "we cannot delete
  on Windows" into "we refuse to delete entries whose names did not come
  from this platform" — which is a correct and checkable rule.
- **Provenance.** Even with the decoder, an executor must refuse to run
  against a tree that did not come from a live scan on this machine. No
  interactive dump viewer exists yet (freeable attack-a finding 8 flagged
  the same latent trap), so this is a guard to write down now and enforce
  when one arrives.
- **The `windows-2025` CI job is the only thing that keeps any of this
  honest.** It runs `cargo test --workspace --locked` as of 2026-07-27.
  Every guarantee in §1.1 needs a test there, and the junction test in
  particular — a `mklink /J` fixture asserting the canary survives — is
  the one whose absence would be discovered by a user, once.
- **`DeleteReport` is public API consumed by the TUI.** Whichever option
  wins, the types (`DeleteTarget`, `InodeId`, `SkipReason`, `EntryOutcome`,
  `DeleteReport`) should move to a platform-neutral `delete/mod.rs` with
  today's body relocated verbatim to `delete/unix.rs`, so the UI has one
  shape to render. That refactor touches Linux mechanically and must be
  pinned by the existing tests before anything else lands.

---

## 7. Recommendation

**Option C2 — permanent, handle-relative, identity-checked deletion as the
mechanism camembert owns, with the Recycle Bin as an explicit, clearly
worded opt-in — and neither shipping until the two zero-destruction
surfaces exist.**

Ordered, each slice landable and reviewable on its own:

1. **The Recycle-Bin meter** (`cfg(windows)`, destroys nothing).
   `SHQueryRecycleBinW` on the scan root's volume → the disk gauge's
   Windows suffix and an `f` panel that says *"5.83 GiB in the Recycle Bin
   — not free until you empty it"*. This is Windows' answer to freeable
   phase 1, it is a thesis-grade honest answer no competitor gives, and it
   is a precondition for slice 5. One flag if any (`--no-recycle-scan`,
   matching `--no-proc-sweep`'s presence semantics), documented in `--help`
   and the README.
2. **The WTF-8 decoder** (`camembert-core`, no `unsafe`, portable tests).
   `wtf8_to_utf16(&[u8]) -> Option<Vec<u16>>`, exact for lone surrogates,
   refusing anything else. Pinned by the §2.8 cases, plus a property test
   over random UTF-16. Nothing else can land without it, and it retires the
   README's stated reason for the whole absence.
3. **The Restart Manager advisory** (`cfg(windows)`, destroys nothing).
   `RmStartSession`/`RmRegisterResources`/`RmGetList`, off-thread, with
   freeable D6's advisory contract verbatim and its two measured caveats
   (kernel-held files invisible; some system files refuse the query). At
   50–285 ms it can also power a standing "N of these are open" line in the
   `v` review modal, not only the confirm. Shipping this *before* the
   executor means the first destructive release already has its epistemics.
4. **The executor** (`delete/windows.rs`). Types moved to `delete/mod.rs`
   first, Unix body relocated verbatim. Then: root opened by path;
   `NtOpenFile(RootDirectory=…)` per component with
   `FILE_OPEN_REPARSE_POINT`; `FILE_ID_INFO` on the target handle compared
   against a confirm-time-captured `(u64 volume serial, [u8;16] file id)`;
   `FileDispositionInfoEx(DELETE | POSIX_SEMANTICS)`, plus
   `IGNORE_READONLY_ATTRIBUTE` when the entry carries the attribute;
   depth-first directory drain; per-entry `EntryOutcome` with
   `WIN_SHARING_VIOLATION` and a new `SkipReason::IdentityUnavailable` for
   `ino == 0`. Tests in the `windows-2025` job: the junction canary, the
   held-handle-survives-rename swap, a read-only file, a sharing violation,
   a hardlink sibling, and a name with a lone surrogate.
5. **`--trash` / a `T` toggle in the confirm modal**, only if wanted after
   4 has run for a while. `IFileOperation` + a real progress sink, `\\?\`
   stripped, `psiNewlyCreated` checked per item, and the modal's headline
   changing to *"move to the Recycle Bin — nothing is freed until you empty
   it"* with the freed figure suppressed. If the dependency question
   (`windows` crate vs hand-rolled vtables) cannot be answered cheaply,
   this slice does not happen and Windows users get the meter from slice 1
   plus Explorer.

**Why C2 over A.** A gives up G4, which is the guard the Unix executor was
specifically rewritten to add, and it cannot verify its own central promise
(that the operation is reversible) before performing it. Reversibility that
you cannot confirm in advance is not a safety property; it is a hope. And A
makes camembert's freed figure false by construction on the one platform
where the tool has the least other evidence to offer.

**Why C2 over D.** D is the option with zero chance of destroying data and
it should be taken seriously right up to the point where slices 1 and 3
have shipped — because those two are the parts of this work the thesis
actually endorses, and they are D-compatible. If the session stops after
slice 3, that is a good outcome. What tips it past D is the basket: the
one gesture Explorer cannot make, backed by the one guard Explorer does not
have.

**The reservation that would reopen this.** Nobody has measured how often
a *real* Windows selection hits `ERROR_SHARING_VIOLATION`. All §3.2's
sharing data comes from synthetic holders. If, on real trees — a
`node_modules` under a running editor, a browser profile, a `%LOCALAPPDATA%`
cache — a large fraction of a typical basket is refused, then a permanent
delete that half-completes with no undo is the wrong default, and the
Recycle Bin's reversibility stops being a nicety and becomes the deciding
property. That measurement is cheap (slice 3 produces it as a side effect:
run the advisory over real baskets and count) and it should be taken
*before* slice 4's default is fixed. The second, narrower reservation:
if slice 5's dependency answer turns out to be "add the `windows` crate",
that reopens the T1 dependency decision made for `windows-sys`, and it
should go back to the user rather than be assumed.

---

## 8. What this does to Linux

**Nothing, with one mechanical exception that must be pinned by tests.**

Slices 1, 3 and 5 are `cfg(windows)` end to end and touch no Unix file.
Slice 2's decoder is Windows-only in use (`OsStr::from_bytes` is exact on
Unix and `os_name_from_bytes` already says so) though it can live as a
portable pure function with portable tests.

The exception is slice 4's prerequisite: moving `DeleteTarget`, `InodeId`,
`SkipReason`, `EntryOutcome`, `EntryResult` and `DeleteReport` from
`delete.rs` into a platform-neutral `delete/mod.rs`, with the current body
relocated verbatim into `delete/unix.rs`. That is a file move plus
`cfg` attributes and **no logic change**; the existing deletion tests are
what proves it. `SkipReason` gains one variant (`IdentityUnavailable`),
which is additive and never produced on Unix, where `st_ino` is always
meaningful.

What must **not** happen is the tempting generalisation: rewriting the Unix
executor to share a "portable" walk abstraction with the Windows one. The
two are the same *shape* and not the same *code* — `unlinkat` cannot fail
for sharing reasons, `FileDispositionInfoEx` has no `AT_REMOVEDIR`, the
read-only attribute has no Unix analogue, and POSIX semantics is a flag on
one and the definition of the other. A shared abstraction would have to
model the union of both, and the module that destroys data is the last
place to pay for that.

---

## 9. Reproducing this

Two throwaway crates, kept out of the repository per CLAUDE.md, both
confined by a `guard()` that panics on any path outside one scratch
directory under `%TEMP%`:

- **`delprobe`** (`windows-sys` only, plus three hand-declared `ntdll`
  imports) with subcommands `handlerel`, `relnames`, `swap`, `posix`,
  `junction`, `identity`, `dirs`, `names`, `bench`, `rm`, `handles`, `vol`.
  `vol` is strictly read-only and was the only probe pointed at a volume
  other than the scratch one. `bench` is min-of-5 with a file-creation
  control in the same loop.
- **`recycleprobe`** (the `windows` crate, for COM) implementing a real
  `IFileOperationProgressSink` so per-item HRESULTs and `psiNewlyCreated`
  are visible. It permanently removes from the bin exactly the items its
  own sink reported, and ships a `--listbin` / `--cleanbin` mode that
  enumerates the Recycle Bin, matches `System.Recycle.DeletedFrom` against
  the probe root, and purges only those — used to verify the bin was
  returned to its prior contents (checked: 0 probe items remaining).

Both were run non-elevated, at medium integrity, with Defender's real-time
protection on and unmodified. `Get-MpPreference` needs administrator
rights, so **whether the scratch directory sits under a Defender exclusion
is unknown** — a stated confound, as in the nlink dossier, not a resolved
one.
