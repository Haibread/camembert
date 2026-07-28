# Changelog

All notable user-facing changes to camembert are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the major version is `0`, a minor bump may carry a breaking change; each
one is called out under **Breaking** with the migration to apply.

## [Unreleased]

### Changed

- **Release archive names drop the target triple's vendor field.**
  `camembert-<version>-x86_64-linux-musl.tar.gz` instead of
  `…-x86_64-unknown-linux-musl.tar.gz`, and
  `…-x86_64-windows-msvc.zip` instead of `…-x86_64-pc-windows-msvc.zip`.
  `unknown` is the one field of `<arch>-<vendor>-<os>-<abi>` that carries
  no information, and it was the least readable thing in a release's
  asset list. Architecture, OS and ABI all stay — dropping `musl` would
  have hidden that these binaries are static, which is why they install
  on any glibc. The `.deb` and `.rpm` names are unchanged: those follow
  their own ecosystems' conventions, which is why the same CPU appears
  as `amd64` there and `x86_64` here. Scripts pinned to the old asset
  names need updating; the `cargo --target` triples are untouched, and
  releases up to and including v0.4.0 keep the names they were
  published with.

## [0.4.0] - 2026-07-28

### Breaking

- **`--links`/`LINKS` no longer exists off Windows.** It was accepted
  everywhere and did nothing off Windows — the value was only ever read
  inside a `#[cfg(windows)]` branch, because `st_nlink` arrives inside
  the `statx` result a Unix scan already asks for. It is now compiled
  out there, so a Linux or macOS `camembert --links` fails with an
  unknown-argument error instead of silently ignoring it. Migration:
  drop the flag; nothing about the scan changes, since nothing about it
  ever depended on the flag on those platforms. This matters more than
  it did before because the man page and shell completions are generated
  from the same `clap` definitions and are now installed by the
  `.deb`/`.rpm`/`PKGBUILD` packages — a no-op flag documented in terms
  of Windows would have shipped to every Linux user.
- **`camembert-core` no longer exposes `rustix::io::Errno` in its error
  taxonomy**, replaced by `camembert_core::errno::ScanErrno` — a newtype
  over the canonical POSIX errno number in Linux/x86-64 numbering.
  `Tree::error_reason`, `Tree::error_reason_counts`, `Row::error_reason`,
  `errno::{name, from_name, label, severity, breakdown}` and the dump
  reader's `error_reason` fields all change type. Migration: swap
  `Errno::ACCESS` for `ScanErrno::ACCESS` (the taxonomy's names are
  mirrored as associated constants), and convert a host errno at the
  boundary with `ScanErrno::from(errno)` on Unix. Dumps are unaffected —
  the `er` field's bytes are unchanged, in both directions.

### Added

- **Native `.deb` and `.rpm` packages**, built for `x86_64` and `aarch64`
  and attached to every release. They install the binary, the three man
  pages, and bash/zsh/fish completions. The packaged binary is the same
  static musl build as the tarballs, so the packages declare no
  dependencies and install on any glibc vintage — at the price of not
  being distro-archive-policy packages. There is no APT/DNF repository
  yet: upgrading means downloading the next release.
- **Shell completions** for bash, zsh and fish, generated from the live
  `clap` definitions by a new `camembert-completions` binary (the same
  pattern `camembert-mangen` already used for the man pages). Packagers
  and from-source installers get them with
  `cargo run --release --package camembert --bin camembert-completions -- <OUT_DIR>`.
- The Arch `PKGBUILD` ([`packaging/aur/`](packaging/aur/)) installs the
  shell completions too, and now carries `options=('!lto')`: makepkg's
  default LTO makes `zstd-sys` emit GCC LTO bitcode that rustc's linker
  cannot resolve, so the package did not build at all on a stock Arch
  configuration.
- `scripts/build-packages.sh`, which builds the release binary,
  regenerates the man pages and completions, and produces both packages.
  It is what the release workflow runs, so a package can be reproduced
  locally exactly as CI built it.
- **A Windows scan backend** (`x86_64-pc-windows-msvc`). `camembert-core`
  builds and its scan engine runs there: real allocated sizes from
  `AllocationSize` (measured compression-aware on NTFS), real hardlink
  dedup from `NtQueryInformationByName`, reparse points classified by tag,
  and paths beyond `MAX_PATH`. The `camembert` binary builds and runs
  there too, with a reduced interface: deletion, the freeable oracle and
  the open-file confidence verdict are compiled out rather than disabled,
  so the keys do not exist, `?` does not list them and the palette does
  not offer them. See the README's "Platform support" for the full split.
  Prebuilt `x86_64-pc-windows-msvc` binaries are attached to releases as
  a `.zip`.
- `errno::ScanErrno::SHARING_VIOLATION`, the taxonomy's first non-POSIX
  reason: Windows' `ERROR_SHARING_VIOLATION`/`ERROR_LOCK_VIOLATION`, i.e.
  another process holding the file open. Its wire name is
  `WIN_SHARING_VIOLATION`, deliberately without an `E` prefix so it cannot
  be read as an errno, and non-POSIX reasons are numbered from 2^24 so the
  dump's decimal fallback can never collide with a real errno. The entry is
  unconditional, not `cfg(windows)`: a dump written on Windows has to
  decode on Linux.

### Changed

- The `directory unreadable` debug log renders its errno as the canonical
  name (`EACCES`) instead of the host's prose description (`Permission
  denied (os error 13)`). Asking the host to describe a canonical number
  would reintroduce the very numbering assumption the newtype removes.

### Fixed

- **The test suite is green on btrfs again.** Two directory-index
  fixtures asserted their own potency ("this directory reports allocated
  index bytes, so the test can tell the corrected answer from the
  listing's zero") on every platform. That is a Windows property: the
  correction it guards reconciles a listing's resident-index zero with
  the by-handle figure, a gap only Windows has, and on Unix "a directory
  has allocated blocks" is not an invariant at all — btrfs keeps
  directory entries in metadata B-trees and reports zero for any
  directory. The guards are now Windows-only; the agreement assertions
  the tests exist for still run everywhere.
- **`camembert diff` no longer renders a Unix path with a backslash
  separator** when the dump's root is a *relative* directory whose own name
  contains one (`camembert 'back\slash' -o dump.cmbt`). The join picked the
  first `/`-or-`\` byte in the directory path, which for such a root is part
  of a filename, and printed `back\slash\plain.txt` — a path that exists
  nowhere and that the dump's own `d` lines (`back\slash/sub`) contradict.
  A legacy 0.3.0 Windows dump is now recognised by its root being a Windows
  *absolute* path (`C:\…`, `\\server\share`, `\\?\C:\…`), which is the only
  shape that writer could have produced, so those still render as the
  Windows paths a user recognises.

## [0.3.0] - 2026-07-25

### Changed

- **Cloud block storage no longer costs 40 % of the scan.** Scaleway SBS
  volumes (and virtualized block devices generally) report
  `queue/rotational = 1` while being network-attached flash; camembert
  believed them, dropped to the 2-worker rotational tier, and engaged
  io_uring on top. A `rotational` flag of `1` is now cross-checked against
  the device's active I/O scheduler and disbelieved when the kernel left
  `none` scheduling active — a combination the kernel never produces for a
  real spinning disk. Measured on a 2-vCPU cloud instance, 100k entries:
  ext4 warm 340 → 210 ms, cold 1840 → 1017 ms; XFS warm 379 → 178 ms, cold
  2122 → 968 ms. A real HDD, and any device whose `queue/scheduler` cannot
  be read, keeps the rotational tier.
- **`--statx-engine auto` now resolves to `sync` at every worker count.**
  io_uring batching measured 12-21 % faster at ≤ 2 workers on the
  development machine and 1.2-1.7× *slower* at every worker count from 1
  to 8 (warm and cold, ext4/XFS/btrfs/f2fs) on cloud block storage. With
  the evidence pointing both ways, the default takes the engine that is
  never the slow one; `--statx-engine io_uring` still forces the other.

### Fixed

- **`camembert … --no-ui | head` no longer panics.** The Rust runtime
  leaves `SIGPIPE` ignored, which turned a closed stdout into a write
  error and a panic (stack trace, exit 101) where every other tool in a
  pipeline exits quietly; the default disposition is now restored at
  startup. The dump is also written *before* any summary text, so a
  truncated pipeline costs the reader nothing but the text they stopped
  reading.

### Added

- **Man pages** — `camembert-mangen <OUT_DIR>` renders `camembert.1` (plus
  one page per subcommand) from the same clap definitions the binary parses
  with, so the manual cannot drift from `--help`.
- **`CAMEMBERT_GIT_SHA` is honored at build time** — set it and `--version`
  reports that commit instead of `unknown`, which is what a distro packager
  building from a `.git`-less tarball needs.
- **Declared MSRV: Rust 1.88**, the floor the dependency graph already
  imposed. A CI job builds against exactly that version, so raising it has
  to be deliberate.
- **AUR packaging** — `packaging/aur/` holds the `PKGBUILD` and the runbook
  for publishing and updating it. Not published yet; it needs a release
  containing the man-page generator.

## [0.2.0] - 2026-07-24

### Breaking

- Scans **cross filesystem boundaries by default**.
  `--cross-filesystems`/`CROSS_FILESYSTEMS` is gone; pass
  `--one-filesystem`/`ONE_FILESYSTEM` to restore the old
  stop-at-the-mount-point behavior. Scripts carrying the removed flag now
  fail to parse instead of silently changing meaning. Kernfs
  (`/proc`, `/sys`, …) stays excluded by filesystem magic either way.
  Known caveat, documented in the README and `--help`: btrfs snapshot
  subvolumes and bind mounts are descended and can multiply-count.

### Added

- **Reclaim oracle** — marking an entry now maps its extents off the UI
  thread (`FS_IOC_FIEMAP`, no root), so the `D` confirmation dialog answers
  what deleting actually frees, bucketed rather than optimistic: `frees ≥ X
  exclusive`, `+ up to Y shared only within the marked set`, `Z shared
  elsewhere will not be freed`, `W not estimated`. Figures are
  allocated-logical bytes. btrfs and XFS get the full extent-aware tier,
  reflink-less filesystems an exact hardlink-only figure, ZFS nothing at all
  (block cloning has no per-file API — no figure beats a guess).
- **Ambient exclusive floor** — a background pass after each scan and each
  in-app deletion proves how much of every directory nobody else references:
  a brighter segment inside each row's bar, plus an `excl ≥ X · mapped Y ago`
  line (or `fully shared`) on the selection card. Additive and counted once
  filesystem-wide, so directory totals never double-count their children.
  Requires kernel ≥ 6.1; below that it never runs rather than showing a
  figure it can't stand behind.
- **`--no-fiemap`/`NO_FIEMAP`** — disables the oracle and the floor
  outright: no job spawns, no `FS_IOC_FIEMAP` call is ever made. Flag and
  env only, no `camembert.toml` key, same shape as `--no-proc-sweep`.
- **Confidence verdict** — both places a freeable number drives a decision
  (top of the `f` panel, top of the `D` dialog) now open with one graded
  line: `measured`, `partial`, `fragmentary`, or `no figure`, plus what
  drove the grade. It headlines the caveats rather than replacing them, and
  carries its level in plain text so a monochrome terminal reads it as well
  as a truecolor one.
- **Per-error `errno`** — every failing directory read and stat keeps its
  reason end to end (scan → tree → dump → TUI), because `EACCES` ("rerun as
  root") and `EIO` ("your disk is dying") are not the same event.
- Multi-filesystem scans report a device count, and the disk gauge captions
  them as spanning N filesystems instead of a percentage of a single device.

### Changed

- `Esc` ascends to the parent directory from the tree view instead of
  quitting outright.
- The README and `--help` describe the age axis as the **filter** it is
  (`--filter '>10M older:1y'`, on mtime), not a score. A measured prototype
  of seven scoring formulas on five real trees found every continuous
  formula collapses onto the size or the age axis, and that mtime is widely
  fabricated — so no score view ships.

### Fixed

- The disk gauge no longer overstates coverage on compressed mounts.
- FIEMAP pagination guards against non-advancing batches instead of looping
  forever on a filesystem that returns an empty batch mid-file.
- The floor pass stays cancellable through hardlink-heavy work, so quitting
  mid-pass no longer waits on it.

### Security

- Deletion walks descriptor-relative (`openat`/`unlinkat`) with the mark
  identity threaded through to the executor, closing a TOCTOU window where a
  path swapped between marking and deleting could redirect the unlink.

## [0.1.0] - 2026-07-23

Initial release: the scan engine, tree view, flat view and pattern
breakdown, filter query language, `.cmbt` dump format, `camembert diff`,
`camembert import` for ncdu exports, the freeable (deleted-but-open) panel,
and musl builds for x86_64 and aarch64. See the
[commit history](https://github.com/Haibread/camembert/commits/v0.1.0) for
the full detail.

[Unreleased]: https://github.com/Haibread/camembert/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/Haibread/camembert/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Haibread/camembert/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Haibread/camembert/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Haibread/camembert/releases/tag/v0.1.0
