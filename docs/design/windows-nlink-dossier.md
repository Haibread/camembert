# Windows link counts — dossier (2026-07-27)

Decision-ready dossier for the per-file link-count lookup in
`camembert-core/src/scan/windows/worker.rs`. **Not settled** — this is
measurement, options, an adversarial attack on those options, and one
recommendation, for a co-design session. Nothing here is binding until it
lands in a decisions doc.

> **Status, 2026-07-27: decided and shipped, in part.** The user approved
> §6's recommendation in its tuned form. Landed: steps 1-5 and 7 —
> registry on the listing's file id, tuned storage, redefined summary line,
> gated dump (`i` yes, `l` never without a count), `--links`/`LINKS`, and
> the `mklink /H` integration test. Measured after landing on the same box:
> 2044 → **107.0 ms** on the 200k tree (1.29× faster than gdu), 5845 →
> 2643 ms on `C:\Windows`, peak RSS 16.99 → 21.46 MB and 84.95 → 87.43 MB,
> totals byte-identical per entry via `camembert diff` on four trees.
> **Step 6 — the lazy lookup at the point of consumption — is not done**,
> and is the piece that buys back the at-a-glance "this file has links you
> cannot see" answer that §5's Attack C.2 is right to call a real loss.
> See `HANDOFF.md`'s Windows section. The §4/§6 numbers below are the
> pre-tuning ones and are corrected by the review note that follows.

Every number below was measured on the user's own machine on 2026-07-27:
Ryzen 9 5950X (16C/32T), NVMe SSD, NTFS, Windows 11 Pro 10.0.26200,
**Microsoft Defender real-time protection ON and unmodified**. The scan
tree is the deterministic 200k-file synthetic tree at
`target\bench-tree-200000` (8 301 directories, 200 001 files). Warm cache
throughout, repeated, variance reported. No security setting was changed
and no elevated shell was used; the process ran at medium integrity
(`S-1-16-8192`), which is itself one of the findings.

> **Orchestrator's review, 2026-07-27.** The load-bearing claim was
> re-measured independently and holds: totals are byte-identical between
> the shipped binary and Option C on `System32\drivers`, `System32`
> (8.4 GiB) and the 200k tree; A 2 100.8 ms → C 153.7 ms, floor 109.4 ms,
> peak RSS 17.0 → 39.4 MB.
>
> **Three corrections to what follows**, the first two settled by a
> follow-up measurement of three registry shapes (§4's Option C is the
> *naive* one; two tuned shapes were built and measured after this dossier
> was written).
>
> 1. **§4/§6's gdu comparison is wrong twice over, in both directions.**
>    Naive C measured 148–154 ms against gdu's 139–146 ms, so it reaches
>    parity at best — "1.13× faster" inverts it. But the **tuned** shape
>    (§6.2's, actually built) measured **107.5 ms**, i.e. 1.6 ms above the
>    no-dedup floor and **1.33× faster than gdu**. So the conclusion
>    "overtakes gdu" is right while the number supporting it is wrong, and
>    it only becomes right once the per-singleton allocation §4 identifies
>    is actually removed.
> 2. **§6.2's "≤ 24 B/entry" is unreachable in the shape it names, and the
>    tuning still succeeds.** `FxHashMap<u64, NodeId>` stores `(u64, u32)`,
>    which pads to 16 B, plus hashbrown's control byte: **285.2 MB** at
>    10 M measured directly, 28.5 B/entry — and because a scan cannot
>    pre-size the map, the final doubling holds the old and new tables at
>    once, so the honest **peak is 429 MB**. Measured on the 200k tree the
>    same shape costs 23.5 B/entry; the two figures differ because hash
>    capacity moves in steps, which is why 10 M was measured rather than
>    extrapolated.
> 3. **The dossier's framing of the memory reservation is backwards, and
>    this is the important one.** It presents C as breaching D4 while the
>    status quo fits. Measured at real Windows link densities, **the status
>    quo blows D4 harder**: its `(dev,ino)` set plus `hardlink_firsts` come
>    to ~369 MB at `System32`'s 92 % density versus the tuned map's 285 MB,
>    putting today's binary at 1382–2308 MB at 10 M — 1.5–2.6× past D4's
>    documented 900 MB hardlink-heavy ceiling. Nobody had noticed because
>    nobody had scanned a Windows system drive at that scale. Neither C
>    shape makes memory worse where it matters; they make it worse only on
>    link-free trees, where the registry is useless and today it is free.
>    D4 does not need defending against C — it needs a Windows row.

---

## Displacement

*What does this work displace, and does the thesis agree with that trade?*

On the table instead: **freeable phase 2 slice 3** (HANDOFF step 1),
**traversal-dedup Option D** (step 9), and the rest of the Windows port's
own gap list — reparse-point link counts, the integration-test gating that
would turn the `windows-2025` CI job from `cargo check` into `cargo test`,
junction resolution.

Honest answer: **this is not a feature competing for a slot; it is a
standing rule the Windows backend is currently in breach of.** CLAUDE.md's
Benchmarks section says a camembert scan "falling behind `gdu`/`dust`-class
scanners on the same tree ... is a regression: fix it or explain it in the
change that introduces it." On this box, on this tree:

| tool | mean | dedups hardlinks here? |
|---|---|---|
| camembert, link count skipped | 112.4 ms ± 2.4 | **no** |
| gdu 5.x | 146.9 ms ± 4.3 | **no** (measured, §3) |
| camembert, as shipped | **2 177 ms ± 54** | yes |

14.8× behind gdu. The Windows backend shipped with that unmeasured, for
the reason `windows-backend-design.md` §10.6 states plainly: the bench
harness was bash-only, so the mandate was unenforceable. It is enforceable
now, and the answer is that the backend is in breach. Fixing it does not
need to earn a slot against slice 3 — it needs to stop being a debt the
next Windows change inherits.

Does the thesis agree? The thesis is *differentiation through honest
answers to real questions*, and hardlink dedup is one of the honest-numbers
claims. So the trade has to be examined rather than assumed, and §3 does
exactly that. The short version, which reframes the whole dossier: the
2 065 ms this call costs **is not what buys correct totals**. Correct
totals are available for free. What the call buys is narrower, real, and
worth about 40 ms — not 2 s.

The one thing that should *not* be displaced by this work is the
`windows-2025` CI job's promotion to `cargo test`. Whatever lands here
changes hardlink accounting on a platform whose integration tests do not
compile, and that is a worse debt than the speed.

---

## 1. Problem

`handle_entry` calls `query_nlink` — `NtQueryInformationByName(FileStat
Information)` — once per non-directory entry, purely to obtain
`NumberOfLinks`. The directory listing
(`GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`) already carries
name, both sizes, attributes, reparse tag and the 128-bit `FileId` for zero
per-entry syscalls; the link count is the one field it does not carry.

Measured end to end (hyperfine, 3 warmups, 15 runs, `--no-ui --top 0`):

```
A  status quo (per-file nlink)      2.177 s ± 0.054 s   [2.133 … 2.355]
C  listing ino, no nlink call        166.0 ms ± 13.5 ms [157.8 … 212.9]
   floor: no hardlink dedup at all   112.4 ms ±  2.4 ms [109.6 … 118.1]
   gdu                               146.9 ms ±  4.3 ms [141.6 … 157.0]
```

**94.8 % of the scan is the link-count lookup.** The design document's own
syscall-shape table (`windows-backend-design.md` §4) predicted this cost
would be `statx`-shaped. It is not.

---

## 2. Part 1 — where the 10 µs goes

All figures single-threaded, on the 200k tree, minimum of 5 runs, warm.
Single-threaded deliberately: the question is per-call cost and its
composition, not throughput. (The 48 µs here versus the ~10 µs implied by
the 8-worker end-to-end number is exactly the parallel speedup, and §2.4
shows why that speedup is only 4.6× and not 8×.)

### 2.1 The call is an open. That is the whole finding.

| what one non-directory entry costs | per entry | marginal over listing |
|---|---|---|
| (c) listing only — no per-file call | **1.93 µs** | — |
| listing + `FxHashSet` insert of the listing's own `FileId` | 2.00 µs | **+0.07 µs** |
| (a) + `NtQueryInformationByName(FileStatInformation)`, directory-relative — **as shipped** | **48.31 µs** | +46.38 µs |
| + the same call with a full `\??\C:\…` absolute path | 47.91 µs | +45.98 µs |
| + two such calls per file | 78.33 µs | +76.40 µs |
| + `CreateFileW(FILE_READ_ATTRIBUTES)` + `CloseHandle` | **45.56 µs** | +43.63 µs |
| (b) + `CreateFileW` + `GetFileInformationByHandle` + `CloseHandle` | 51.73 µs | +49.80 µs |
| + `CreateFileW` + **16 ×** `NtQueryInformationFile` + `CloseHandle` | 63.57 µs | +61.64 µs |

Two derived numbers do the work:

- **A by-handle information query costs 1.13 µs.**
  `(63.57 − 45.56) / 16`.
- **A bare open-and-close costs 43.6 µs**, and
  `NtQueryInformationByName` costs 46.4 µs.

So **97.6 % of the by-name call is the create path** — object-manager
parse, file-object instantiation, `IRP_MJ_CREATE` down the filter stack,
cleanup, close — and 2.4 % is retrieving the field.

That retires the premise the backend was built on.
`NtQueryInformationByName`'s Remarks say it works *"without opening the
actual file"*, and `windows-backend-design.md` §4 took that to mean it is
cheap, while rejecting `CreateFileW`-per-entry as "20-200× more expensive".
Measured, the two are within 7 % of each other. "Without opening the file"
means **without returning a handle to you**; the file object is still
created and every minifilter still sees the create. The design's 20-200×
figure was published measurement of `CreateFileW`-per-entry versus
*listing* — which is the 1.93 µs column, and which is indeed 24× cheaper.
The comparison was right; it was pointed at the wrong pair.

### 2.2 It does not scale with path depth

2 000 files at each depth, minimum of 5 runs:

| depth | `NtQueryInformationByName`, dir-relative bare leaf | same, absolute `\??\` path |
|---|---|---|
| 1 | 27.09 µs | 27.01 µs |
| 4 | 27.44 µs | 27.11 µs |
| 8 | 27.93 µs | 27.34 µs |
| 16 | **28.56 µs** | **28.45 µs** |

**+5.4 % over sixteen levels.** Object-manager path parsing is not the
cost. Two consequences:

- The `OBJECT_ATTRIBUTES.RootDirectory` directory-relative trick that
  `windows-backend-design.md` §7.2 went to some trouble to verify buys
  **nothing measurable** (48.31 vs 47.91 µs — the absolute path is, if
  anything, marginally faster). It is still the right shape for
  `openat`-parity reasons; it is not a performance feature and should stop
  being described as one.
- Any option that keeps the call cannot be rescued by shortening paths.

(The 27 µs here versus 48 µs on the bench tree is a working-set effect —
2 000 freshly written files stay entirely hot; 200 001 across 8 301
directories do not. The *shape* is what this table is for.)

### 2.3 The filter stack, attributed read-only

Defender's settings were **not modified** — that is out of bounds and was
not authorised. State at measurement time, read-only via
`Get-MpComputerStatus`:

```
AMServiceEnabled          : True
RealTimeProtectionEnabled : True
BehaviorMonitorEnabled    : True
OnAccessProtectionEnabled : True
IoavProtectionEnabled     : True
AMEngineVersion           : 1.1.26060.3008
AntivirusSignatureVersion : 1.455.371.0
```

`Get-MpPreference` returns *"N/A: Must be an administrator to view
exclusions"*, so **whether this tree sits under an exclusion is unknown** —
a stated confound, not a resolved one.

`fltmc filters` needs elevation (`0x80070005`). The read-only substitute is
the registry: a minifilter registers under
`HKLM:\SYSTEM\CurrentControlSet\Services\<name>\Instances` with an
`Altitude`. **24 registered, 14 running**, highest altitude first:

```
bindflt 409800 · UCPD 385250.5 · wtd 385110 · WdFilter 328010 (Defender)
storqosflt 244000 · gameflt 189850 · CldFlt 180451 · bfs 150000
FileCrypt 141100 · luafv 135000 · UnionFS 130850 · npsvctrig 46000
Wof 40700 · FileInfo 40500
```

Fourteen drivers get a callback pair on every `IRP_MJ_CREATE`. The listing
call, by contrast, is one `IRP_MJ_DIRECTORY_CONTROL` per ~1 000 entries.
That is the mechanism; it is consistent with §2.1's split, and it is as far
as a read-only attribution can honestly go. **It is not a defence.** gdu
runs through the same fourteen filters on the same tree in 146.9 ms,
because it never issues a create. There is no platform floor here — only a
choice about how many file objects to instantiate.

### 2.4 More threads make it worse

`--threads N`, status-quo mode, best of 3:

| threads | 8 | 16 | 32 | 64 | 128 |
|---|---|---|---|---|---|
| wall | **2 090 ms** | 3 119 ms | 2 985 ms | 2 876 ms | 2 983 ms |

**Negative scaling past the default.** On a 16C/32T box with an NVMe SSD,
throwing threads at a 46 µs I/O-shaped call makes it 40 % slower. Whatever
serialises inside the create path (per-file-object FCB work, the
scan-cache lock inside `WdFilter`, or both) is not something concurrency
hides. This kills the cheapest-looking option outright — see Option F.

### 2.5 The bulk MFT APIs need elevation. Confirmed, not assumed.

Both candidates enumerate the MFT and both want a volume handle. Measured
directly:

```
\\.\C:  GENERIC_READ           -> FAILED win32=5 (ERROR_ACCESS_DENIED)
\\.\C:  FILE_READ_ATTRIBUTES   -> handle OK
          FSCTL_QUERY_FILE_LAYOUT      -> FAILED win32=1
          FSCTL_ENUM_USN_DATA          -> FAILED win32=1
          FSCTL_GET_NTFS_VOLUME_DATA   -> FAILED win32=1
\\.\C:  access 0 (query-only)  -> handle OK, all three FSCTLs -> win32=1
```

A volume handle *does* open unprivileged with `FILE_READ_ATTRIBUTES` —
which is the one genuinely surprising result here — but it is too weak to
dispatch a filesystem-control IRP. Cross-checked through Microsoft's own
wrappers, which ask for `GENERIC_READ`:

```
fsutil fsinfo ntfsinfo C:                          Error 5: Access denied
fsutil usn enumdata 1 0 1 C:                       Error 5: Access denied
fsutil file layout C:\Windows\System32\drivers\ntfs.sys   Error 5: Access denied
```

So: **elevation-only, on both paths, confirmed two ways.** Worth
documenting, per the brief, but it cannot be the default and camembert has
no elevation story. It also would not help as much as it looks: a
whole-volume MFT enumeration answers link counts for `C:` in bulk, but the
scan root is usually a subtree, and paying a full-volume MFT pass to scan
`C:\Users\me\projects` is a worse trade than the one being replaced.

There is no non-elevated bulk API. Every directory information class
(`FileIdExtdDirectoryInfo`, `FileIdBothDirectoryInfo`, `FileFullDirectory
Info`, …) omits `NumberOfLinks`; the only classes that carry it
(`FileStandardInformation`, `FileStatInformation`, `FileAllInformation`,
`FILE_STAT_LX_INFORMATION`) are per-file, and §2.1 shows the per-file
cost is the create, not the class.

---

## 3. What the link count actually buys

This is the part that decides the dossier, and it is the part that was
assumed rather than measured.

### 3.1 On real Windows trees, the link count deduplicates nothing

An audit harness recorded, for every non-directory entry, both the
listing's `FileId` and the stat-reported `nlink`, then computed three
totals: the naive sum, **today's rule** (an `nlink > 1` entry whose
`(dev, ino)` was already seen contributes 0), and **option C's rule** (any
repeat sighting of a file id contributes 0, `nlink` never consulted).

| tree | files | `nlink > 1` | repeat sightings | "lonely links" | Σ naive | Σ today | Σ option C |
|---|---|---|---|---|---|---|---|
| synthetic fixture (64 files + 64 `mklink /H`) | 128 | 128 | 64 | 0 | 134 217 728 | **67 108 864** | **67 108 864** |
| `C:\Windows\System32\drivers` | 756 | **728** | **0** | 728 | 169 237 128 | 169 237 128 | 169 237 128 |
| `C:\Windows\System32` | 24 688 | **22 685** | 292 | 22 108 | 9 640 352 904 | **9 066 829 841** | **9 066 829 841** |
| `C:\Program Files\Common Files` | 261 | 230 | 0 | 230 | 46 964 157 | 46 964 157 | 46 964 157 |

("Lonely link" = an inode with `nlink > 1` of which this scan saw exactly
one path.)

Read the three rightmost columns: **today's total and option C's total are
byte-identical on every tree, including the one built specifically to
contain hardlinks.** Confirmed independently by running the real binary in
all three modes:

```
hltree fixture   query 64.0 MiB   all 64.0 MiB   skip 128.0 MiB
drivers          query 162.6 MiB  all 162.6 MiB  skip 162.6 MiB
```

The reason is in `owner.rs:274`:

```rust
let is_hardlink = entry.kind != Kind::Dir && entry.nlink > 1;
let is_extra_link = is_hardlink && self.hardlinks.contains(&(entry.dev, entry.ino));
```

The thing that deduplicates is the `(dev, ino)` set. `nlink > 1` is only a
**gate** deciding whether an entry is worth inserting into it. On Linux
that gate is free — `nlink` rides inside the `statx` result — so it is a
pure memory optimisation. On Windows it is a 46 µs syscall.

And on Windows it is not even selective. On `C:\Windows\System32`, **91.9 %
of files have `nlink > 1`** (WinSxS links essentially everything), so the
gate admits nearly everything anyway. Measured peak RSS on that tree: 9.85
MB with no registry, 11.85 MB with today's gate, 12.15 MB with option C.
**The gate costs 22 685 syscalls to save 0.3 MB.**

Meanwhile `drivers` is the case that should be framed on a poster: 728 of
756 files have `nlink > 1`, **zero** of them repeat inside the scan (their
siblings live in WinSxS, outside the root), the total is byte-identical to
counting every link once, and the link-count lookup consumed 95 % of the
runtime to move the answer by nothing.

### 3.2 What it *does* buy — measured, and it is not nothing

Three consumers, and only three:

1. **The `⛓` badge and the "hardlinked inodes: N" summary line.** On
   Windows this is the answer to *"deleting this file here frees nothing,
   because it has links you cannot see"* — and §3.1 shows that on
   `C:\Windows` this is true of 92 % of files. That is a genuinely
   thesis-aligned answer to a real question, and it is the one thing on
   this list that no competitor offers. **It must be priced, not
   dismissed.**
2. **The dump's `l` field** (`dump.rs:303`, spec §6.4/§7), and with it `i`
   and `dev`, all emitted together for registry members.
3. **The freeable-2 D4 rule** ("the scan saw every link",
   `fiemap/floor.rs:271`, `ui/oracle.rs:325`). Both files are `cfg(unix)`;
   `lib.rs` gates `fiemap` and `freeable` behind `cfg(target_os = "linux")`
   and `cfg(unix)` respectively. **D4 has no Windows consumer today, and
   cannot acquire one without a FIEMAP equivalent that does not exist.**
   Establishing that was one of the brief's questions; the answer is that
   the Windows `nlink` value is currently consumed by exactly two things,
   both cosmetic-to-informational, neither arithmetic.

### 3.3 No other tool on Windows offers this at all

Verified by experiment, not by reading READMEs. Fixture: 64 files of 1 MiB
each in `unique\`, plus 64 `mklink /H` hardlinks to them in `links\`.
Ground truth: 67 108 864 bytes of distinct content; 134 217 728 if every
link is counted.

| tool | reported | dedups? |
|---|---|---|
| `gdu --non-interactive` | 64.0 MiB + 64.0 MiB = **128 MiB** | no |
| `dust` | **128M** | no |
| `diskus` | **134 242 304** | no |
| `robocopy /L /S` | **134 217 728** | no |
| PowerShell `Get-ChildItem -Recurse` | 134 217 728 | no |
| **camembert** | **64.0 MiB**, *"hardlinked inodes: 64 (each counted once)"* | **yes** |

camembert is the only one that gets it right, by a factor of two, on a
tree where being wrong is very obvious. That is the property under
discussion, and §3.1 has already shown it survives Option C intact.

What each tool *claims*, checked against the measurement above, sharpens
this considerably:

- **`diskus` is the only one that tells the truth.** Its README says
  Windows tools do not respect hardlinks and *"diskus does the same and
  counts such entries multiple times"*, with the Unix behaviour named as
  the exception. Documented divergence, measured divergence, no gap.
- **`dust` claims *"Dust will not count hard links multiple times"*** with
  no platform qualifier. Measured on Windows: it counts them twice. The
  claim is Unix-only in fact and universal in wording.
- **`gdu` claims *"Hard links are counted only once."*** with no platform
  qualifier either, and ships Windows binaries. Measured on Windows: it
  counts them twice. Go's `os.FileInfo` exposes no link count on Windows,
  so there is no mechanism by which it could.
- **`robocopy`** is not a usage analyser and promises nothing here;
  **`ncdu`** has no native Windows build at all.

So the bar is not "everyone else already does this cheaply". The bar is
that two of the four shipping Windows binaries make a correctness claim
their Windows builds do not deliver. camembert delivers it, and the
question this dossier answers is only what it should cost.

---

## 4. Options

### Option A — status quo

Query `nlink` per non-directory entry.

- **Cost.** 2 177 ms ± 54 on the bench tree; 14.8× behind gdu; in breach
  of CLAUDE.md's benchmark rule.
- **Buys.** §3.2's three consumers, one of which does not exist on
  Windows.
- **Totals.** Correct — and identical to Option C's on every tree measured.
- **Memory.** Peak RSS 15.3 MB @ 200k (synthetic, no hardlinks); 11.85 MB
  on `System32`.
- **Honesty cost.** None. It is the baseline everything else is priced
  against.

### Option B — probabilistic pre-filter over `(dev, ino)`

Bloom/cuckoo filter sized for the scan; query `nlink` only when the filter
says "possibly seen before". No false negatives, so no in-scan hardlink
pair is missed; false positives cost one wasted 46 µs syscall each.

**This option is dead, and the measurements are what killed it.** The hole
the brief asked to be quantified is not an edge case — it is the dominant
case:

- On `C:\Windows\System32\drivers`, **728 of 728** `nlink > 1` inodes are
  seen exactly once. The filter never fires. Every one of them would be
  recorded as `nlink = 1`.
- On `C:\Windows\System32`, **22 108 of 22 685** (97.5 %) are lonely.

So B would record a false `l:1` on 97.5 % of the entries that carry link
information, which is precisely the wire field it was supposed to preserve.
What that breaks:

- **The dump's `l` field** (spec §6.4). A Windows-written dump would assert
  `nlink = 1` for files that have twelve links. `l` is *absent* when
  `nlink == 1`, so the lie is by omission, and a Linux reader cannot
  distinguish "single link" from "we did not look" — the dump format has no
  vocabulary for the third state.
- **Cross-platform dump reads.** `dump/read.rs` types `nlink` as
  `Option<u64>`; absence already means "not emitted". Silently
  overloading it with "not determined" makes every downstream consumer's
  `None` branch wrong in a new way. `diff` merge-joins across dumps, so a
  Linux dump and a Windows dump of the same NTFS volume mounted twice would
  disagree about which files are hardlinked.
- **Freeable-2 D4** — no Windows consumer today (§3.2), so nothing breaks
  *now*; the exposure is that B bakes an unreliable `l` into stored dumps
  that outlive the decision.

And it does all that while still paying syscalls. Option C achieves B's
speed goal *and* B's stated correctness property (no missed in-scan pair)
with **zero** syscalls and no false negatives at all. B is strictly
dominated. It stays in this dossier only so nobody proposes it again.

### Option C — exact in-scan dedup from the listing's file id, no `nlink` at all

Register every non-directory entry's `(volume serial, FileId)` — already in
the listing, free — and dedup on repeat sighting. Never call
`NtQueryInformationByName`.

- **Speed.** **166.0 ms ± 13.5** (13.1× faster than A; 1.13× faster than
  gdu, which does not dedup).
- **Totals.** Byte-identical to A on all four trees measured, including
  the hardlink fixture (§3.1). Prototyped in the real binary and verified,
  not argued.
- **Exactness.** Exact for everything inside the scan. Blind to links
  outside it — which is the *only* thing A knows that C does not.
- **Memory — the real cost, and it is workload-shaped:**

  | tree | skip | A (status quo) | C | C's delta |
  |---|---|---|---|---|
  | 200k synthetic, zero hardlinks | 14.8 MB | 15.3 MB | **36.4 MB** | +21.1 MB = **105 B/entry** |
  | `C:\Windows\System32`, 91.9 % linked | 9.85 MB | 11.85 MB | 12.15 MB | **+0.3 MB** |

  The delta is largest exactly where the registry is useless, and
  negligible where hardlinks are dense. But 105 B/entry is the number that
  matters against `scan-tree-decisions.md` D4's **~450 MB @ 10 M entries**:
  naively, **+1.05 GB at 10 M. That does not fit and the naive prototype
  must not ship.**

  Most of it is avoidable and identifiable. The prototype pays, per entry:
  a `FxHashSet<(u64,u64)>` slot (~22 B), a `HardlinkLink` record in the
  owner's `Vec` (32 B with padding), and — the expensive one — a **`Vec<NodeId>`
  heap allocation per group** inside `hardlink::group_links` /
  `reattribute`, which at 200 k singleton groups means 200 k allocations
  that exist only to hold one element. A tuned shape (registry as
  `FxHashMap<ino, first NodeId>`, link records pushed only on a *repeat*
  sighting, no per-singleton `Vec`) lands near **24 B/entry**, i.e. ~240 MB
  at 10 M. Against a 320 MB tree inside a 450 MB envelope, **that still
  breaches D4** — see the Attack, and see the Recommendation's reservation.

- **Honesty cost, precisely.** Three things, all real:

  1. **The `⛓` badge and the summary counter change meaning.** Run
     against `drivers`, the prototype prints *"hardlinked inodes: 756"*
     where A prints *"728"* — and 756 is simply the file count. Under C
     the registry admits everything, so "hardlinked" would badge every
     file on the system. **The counter must be redefined to "inodes
     reached by more than one path in this scan"** (which would print 0 on
     `drivers`, 64 on the fixture, 292 on `System32`). That is a
     *different but still honest* statement — and it is strictly less
     informative than A's, because the answer to "will deleting this free
     anything" flips from *known* to *unknown* for 22 108 files on
     `System32`.
  2. **The dump.** `entry_line` emits `i` + `l` for every registry member,
     so naive C writes `l:2` on every entry — a falsehood on the wire, and
     measurably fat: the 200k tree's dump goes **121 758 → 694 478 bytes,
     5.7× larger, after zstd**. Any C implementation must emit `i`/`l`
     only for inodes actually seen more than once, and must **omit `l`
     entirely on Windows** rather than fabricate one. Spec §8's canonical
     rule is phrased "for each `(dev, ino)` with `nlink > 1`"; under C the
     Windows rule becomes "seen more than once", which produces the same
     canonical owner for every genuine group and needs a platform note in
     the spec.
  3. **Freeable-2 D4** is unaffected today (no Windows consumer) and would
     have to fail closed if one ever arrives — which is what it already
     does when `nlink` is absent.

### Option D — deferred second pass

Skip `nlink` during the walk; post-scan, query only the inodes that need
it.

**As stated, it degenerates.** The brief's own test — "establish whether
the candidate set can be identified without already knowing the answer" —
comes back negative for the `⛓` badge: the set of files that might have
links outside the scan is *every file*, because the only signal that
distinguishes them is the link count itself. Post-scan you would pay
200 001 × 46 µs, exactly A's bill, just later. Worse: A pays it inside an
8-way parallel walk with warm directory handles; D pays it single-file
from cold paths, and §2.4 says you cannot buy the parallelism back.

**But one variant survives and is good.** Defer not to *end of scan* but to
*point of consumption*, where the candidate set is small and known:

- the **selection card** for the currently highlighted row — 1 file, ~30-48 µs,
  invisible;
- the **visible viewport** for the `⛓` column — ~50 rows, 1.4-2.4 ms,
  cacheable per inode, off the 33 ms cadence;
- the **delete-confirm modal** for marked files — this is where "deleting
  this frees nothing" actually matters, and a 1 000-file selection costs
  30-48 ms off-thread;
- **dump writing**, if `l` is wanted, as an explicit opt-in that says what
  it costs.

Absolute-path lookups work identically to directory-relative ones (§2.2:
47.91 vs 48.31 µs), so the UI does not need a live directory handle to do
this. This variant is not an alternative to C — it is C's missing half.

### Option E — a cheaper information class, or a bulk API

Exhausted in §2.1 and §2.5. No directory information class carries
`NumberOfLinks`; every class that does is per-file and the per-file cost is
the create, not the class. `FSCTL_QUERY_FILE_LAYOUT` and
`FSCTL_ENUM_USN_DATA` are elevation-only, measured two ways. **There is no
option E of this shape.** Recording it so the next person does not spend a
day rediscovering it.

### Option F — keep `nlink`, buy the time back with threads

The obvious cheap answer, and the one the box's 16C/32T might seem to
invite. §2.4 measured it: 8 → 2 090 ms, 16 → 3 119 ms, 32 → 2 985 ms,
64 → 2 876 ms, 128 → 2 983 ms. **Negative scaling.** Dead on the numbers.

Worth stating why it looked plausible: `effective_threads` on Windows
returns the `Media::Unknown` tier (`min(2 × cores, 8)`) purely because
there is no seek-penalty probe yet (`windows-backend-design.md` §9), so it
is easy to assume the default is leaving headroom on an NVMe box. It is
not. The headroom is imaginary; the create path serialises.

---

## 5. Attack

### Attack A (status quo)

1. **FATAL for the benchmark rule.** 14.8× behind gdu, and CLAUDE.md says
   that is a regression to be fixed or explained. The explanation on offer
   — "we deduplicate hardlinks and they do not" — is true (§3.3) and is
   *not sufficient*, because Option C shows the property costs 54 ms, not
   2 065 ms.
2. **SERIOUS — it pays per-file for a per-inode answer.** The 22 685
   `nlink > 1` files on `System32` resolve to 22 396 distinct inodes; A
   asks the filesystem once per *link*, never caching by inode. Even
   keeping A, an inode-keyed memo would cut the call count — though only
   by 1.2 % on that tree, because links are lonely. The point stands
   architecturally and is worthless practically, which is itself the
   argument against A.
3. **SERIOUS — the cost is invisible in the codebase.** `query_nlink`'s
   doc comment describes a cheap call ("without opening the actual file").
   Anyone reading `worker.rs` would not guess it is 95 % of the scan. If A
   is kept, that comment is a documentation bug.
4. **ANNOYING — it is already breaching D4 on Windows and nobody noticed.**
   At 91.9 % hardlink density the "gate saves memory" premise is false on
   the exact platform this dossier is about. D4's 450 MB was sized for
   Linux link densities.

### Attack B (probabilistic pre-filter)

1. **FATAL — the hole is the common case, not the tail.** 97.5 % of
   link-bearing entries on `System32` are lonely; 100 % on `drivers`. B is
   wrong about almost every file it exists to be right about.
2. **FATAL — dominated.** C is faster, exact for the same set, and needs
   no syscalls. There is no residual argument for B.
3. **SERIOUS — it puts an unfalsifiable claim on the wire.** An absent `l`
   means "single link" in the spec. B would make it mean "single link, or
   we guessed". Dumps outlive decisions.

### Attack C (my own recommendation, attacked hardest)

1. **SERIOUS — the memory objection is real and the prototype fails it.**
   105 B/entry measured → ~1.05 GB at D4's 10 M target. Even the tuned
   24 B/entry shape lands at ~240 MB on top of a 320 MB tree, which
   breaches 450 MB. **This is the single strongest argument against C**,
   and it must be resolved before implementation, not after. Mitigations
   that need pricing: keying on the 64-bit `FileId` low half only (volume
   is scan-constant on Windows T1, so the `dev` half is pure waste — halves
   the key), and a coverage cap that degrades *visibly* in the
   `Coverage::Exceeds` register rather than silently.
2. **SERIOUS — C removes an answer the thesis is built on giving.** "This
   file has links you cannot see, so deleting it frees nothing" is exactly
   the kind of honest answer camembert exists for, and on `C:\Windows` it
   is true of 92 % of files. C's redefined counter answers a *different*
   question. Option D's consumption-point variant restores it where a user
   asks; it does not restore it as an at-a-glance column over a whole
   listing, and the recommendation must not pretend otherwise.
3. **SERIOUS — C's dedup silently stops working where the file id is
   unreliable, and it has no fallback.** `is_sentinel_id` already forces
   `nlink = 1` for all-`0xFF` and all-zero ids (FAT/exFAT/UDF, some SMB
   servers). Under A those files simply do not dedup, which is correct and
   quiet. Under C they *also* do not dedup, but now that is the only
   mechanism there is — and on a ReFS volume the 128-bit fold makes
   identity probabilistic (`id_folded`), so C inherits the fold's collision
   risk as a *totals* risk rather than a registry-membership risk. A
   collision under A costs one wrongly-grouped hardlink; under C it costs
   one file's bytes vanishing from the total. `id_folded` is already
   surfaced; it now needs to be surfaced *louder*.
4. **SERIOUS — the dump change is a wire-visible behaviour change on a
   format whose major version is near-taboo.** Omitting `l` on Windows is
   spec-legal (readers already treat it as optional) but it means a
   Windows dump and a Linux dump of the same tree differ in a field
   `diff` can see. The `i` emission gate has to move from "registry
   member" to "seen twice" or dumps grow 5.7×, measured.
5. **ANNOYING — 166 ms is 54 ms above the 112 ms floor, and half of that
   is waste.** The per-singleton `Vec` allocation in `reattribute` is pure
   overhead for 200 k groups of size one. Tuning is not optional for
   memory reasons anyway, and it should recover most of the 54 ms.
6. **ANNOYING — C makes `nt_stat_supported` dead code** on the default
   path, along with `is_unsupported_status` and its test. If `--links`
   keeps them alive, they are now on a path almost nobody exercises, which
   is how the `known_names_round_trip` class of bug got in
   (`HANDOFF.md`, "Why `errno.rs` was the blocker").
7. **COSMETIC — "hardlinked inodes: N" changes meaning across a version
   boundary on one platform only.** A user comparing a Windows summary
   before and after upgrade sees the number drop from 728 to 0 on
   `drivers` and will read it as a regression. The line needs to say what
   it counts, not just count.

### Cross-cutting

- **Reproducibility.** A and C are both order-independent for totals: the
  canonical re-attribution (`hardlink.rs`, dump D2) re-anchors each group
  on the smallest raw-byte path after the scan, so work-stealing order
  does not leak into the numbers. C does not change this; it changes which
  inodes enter the pass. The `HARDLINK_FIRST` flag *is* order-dependent
  before re-attribution, and C multiplies the number of nodes carrying it
  by ~50× — worth a test that pins post-re-attribution determinism at
  scale.
- **`--one-filesystem`.** Unaffected: T1 refuses every mount point, so the
  volume serial is scan-constant and the `dev` half of the key is
  redundant on Windows regardless of which option wins.
- **The `windows-2025` CI job cannot see any of this.** It is `cargo
  check`. Every claim in this dossier about totals was verified by running
  the binary by hand, which is not a regression guard. Whichever option
  wins, the fixture in §3.3 (64 files + 64 `mklink /H`, assert the total is
  half the naive sum) is the test that should have existed already.

---

## 6. Recommendation

**Ship Option C as the Windows default, tuned for memory before it lands,
and restore the link-count answer at the point of consumption (Option D's
surviving variant) rather than at every entry.**

Concretely:

1. **`handle_entry` stops calling `query_nlink`.** Every plain file with a
   non-sentinel id enters the registry; repeat sightings deduplicate.
   Totals are byte-identical to today on every tree measured — this is the
   load-bearing fact, and it is measured, not argued.
2. **Tune the registry before merging, not after.** `FxHashMap<u64 ino,
   NodeId first>` keyed on the file id alone (the volume is scan-constant
   under T1), link records pushed only on a repeat sighting, and no
   per-singleton `Vec` in `group_links`/`reattribute`. Target ≤ 24 B/entry
   and ≤ 130 ms on the bench tree. **If it cannot get there, this
   recommendation does not stand — see the reservation.**
3. **Redefine the Windows summary line and the `⛓` badge** to "inodes
   reached by more than one path in this scan", in those words, in the
   summary, the README and `--help`. The Linux wording does not change.
4. **Gate the dump.** Emit `i` only for inodes seen more than once; **never
   emit `l` on Windows** (absent is honest; `l:2` is not). Add a platform
   note to `docs/format/dump-v1.md` §8 saying the Windows canonical rule
   keys on "seen more than once". Verify the 200k dump stays at ~122 KB,
   not 694 KB.
5. **Add `--links` / `LINKS`** (documented in `--help` and the README as
   experimental, per CLAUDE.md): restores the per-file call, restores `l`
   in the dump and the "links outside this scan" meaning of the badge, and
   states its own cost — *"~13× slower; every file is opened once"*. This
   is how a user who genuinely wants the WinSxS answer gets it, with the
   price on the label.
6. **Query lazily where it is consumed**, off the scan path: the selection
   card (1 file, ~40 µs), the delete-confirm modal for marked files
   (~40 µs each, off-thread, memoised per inode). This is where "deleting
   this frees nothing" actually changes a decision, and it is affordable
   exactly there.
7. **Ship the hardlink fixture as an integration test** — 64 files + 64
   `mklink /H`, assert the total is half the naive sum — and gate it so the
   `windows-2025` job can run it. Without this, nothing above is
   regression-guarded.

**Why C over A.** A costs 2 065 ms to buy an answer that changes no total
on any tree measured, on a platform where every competitor is 14× faster
and none of them is even trying to be correct. C keeps the correctness
property that differentiates camembert (§3.3 — it is the *only* tool that
halves the fixture), overtakes gdu while doing it, and pays for it in
memory rather than in a syscall that Defender's fourteen minifilters get to
inspect. The honesty that is actually sold is narrower than "hardlink
dedup": it is one badge and one optional dump field, and §6.5/§6.6 buy both
back where they matter.

**The reservation that would reopen this.** If step 2's tuning cannot get
under ~24 B/entry, Option C at `scan-tree-decisions.md` D4's 10 M target
needs ~240 MB on top of a 320 MB tree and **breaches the 450 MB budget**.
That is not a reason to keep A — A breaches it too on any Windows system
drive, at 92 % hardlink density, and nobody had noticed — it is a reason to
reopen **D4 itself** as a budget decision with Windows link densities in
evidence. The second reservation is narrower: if the listing's `FileId`
turns out to be unreliable on a filesystem camembert must support (ReFS
folding, SMB shares issuing sentinels), C's dedup degrades silently where
A's degraded loudly, and the `id_folded` signal has to be promoted from a
log line to a scan-outcome statement before C ships.

---

## 7. What this does to Linux

**Nothing, and it must stay that way.**

The Linux backend gets `nlink` free inside `statx` — one field in a result
it already requests — so the `nlink > 1` gate there costs zero syscalls and
genuinely saves memory (Linux trees are not 92 % hardlinked; that is a
WinSxS property, not a filesystem one). Every part of the recommendation is
confined to:

- `camembert-core/src/scan/windows/worker.rs` — the call site;
- a `cfg`-gated registry-admission policy on the owner (the owner's own
  logic is unchanged; only *which* entries arrive with `nlink > 1` differs,
  which is already a platform-supplied value);
- platform-conditional **wording** for the summary line and the `⛓` badge;
- a platform-conditional dump field gate.

`scan/linux/` is not touched. Freeable-2 D4, the FIEMAP floor, the oracle
and the confidence verdict are all `cfg(unix)`/`cfg(target_os = "linux")`
and never see a Windows `nlink`. The one thing that must **not** happen is
the tempting generalisation — "register every inode everywhere, it is
simpler" — because on Linux that would trade a free gate for real memory
and would change the meaning of `⛓` on the platform where it is currently
exactly right. If a future reader wants one code path, the honest one is
the *Linux* semantics, and Windows is the platform that cannot afford them.

---

## 8. Reproducing this

The measurement harness is a scratch crate (`nlinkbench`) kept out of the
repository per CLAUDE.md; it walks a tree with
`GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` and does exactly
one extra thing per non-directory entry, selected by mode: nothing,
a hash insert, `NtQueryInformationByName` (relative or absolute, once or
twice), `CreateFileW`+`CloseHandle`, `+GetFileInformationByHandle`,
`+16 × NtQueryInformationFile`, or the §3.1 audit. `volprobe` issues the
two MFT FSCTLs against `\\.\C:` at three access levels.

The end-to-end numbers come from a prototype switch
(`CAMEMBERT_PROTO_NLINK` = `query` | `all` | `skip`) patched into
`handle_entry` on the dossier's worktree branch. **It is a measurement
device and must not ship**: it is an undocumented environment variable,
which the project's documentation rule forbids outright. It is marked as
such in the code.

Competitor comparison used `target\bench-tools\bin\hyperfine.exe` and the
`mklink /H` fixture described in §3.3.
