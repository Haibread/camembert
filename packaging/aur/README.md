# AUR packaging

Source of truth for the [`camembert`](https://aur.archlinux.org/packages/camembert)
AUR package. The AUR repository holds only `PKGBUILD` and `.SRCINFO`; this
directory is where they are edited, reviewed, and kept in history alongside
the code they build.

Arch's official repositories are not a target: getting into `extra` requires
an Arch package maintainer to adopt the project, which is their call, not
ours. The AUR is what makes `yay -S camembert` work today.

## Requirements this package makes of the source tree

- **`camembert >= 0.3.0`** — `package()` installs man pages produced by the
  `camembert-mangen` helper binary, which does not exist in v0.2.0, and shell
  completions from `camembert-completions`, which landed later still.
- **`CAMEMBERT_GIT_SHA`** — `build.rs` uses a pre-set value over its own git
  lookup, so `--version` reports the packaged commit instead of `unknown`
  when building from a `.git`-less tarball.
- **A `check()`-able test suite** — extent-dependent tests guard-skip on
  filesystems without extents, and the statx engine falls back to synchronous
  `statx` when io_uring is unavailable, so the suite passes in a clean chroot.
  The directory-index fixtures added with the Windows landings assert their
  own potency only on Windows, so btrfs — which allocates no blocks to a
  directory — does not fail `check()` either.

## Per-release update

```bash
cd packaging/aur
# 1. bump pkgver to the new tag (without the leading "v")
# 2. set _commit to the commit that tag points at:
git rev-list -n 1 --abbrev-commit v0.3.0
# 3. refresh the checksum from the published tarball:
updpkgsums
# 4. regenerate the metadata AUR reads:
makepkg --printsrcinfo > .SRCINFO
```

`sha256sums=('SKIP')` is a placeholder, never a shipping value: it disables
verification of a tarball downloaded over the network. `updpkgsums` must have
replaced it before anything is pushed.

## Testing before publishing

```bash
cd packaging/aur
makepkg -si          # build, run the test suite, install locally
```

A clean-chroot build is the stronger check, since it catches dependencies
that only work because they happen to be installed on the build machine:

```bash
extra-x86_64-build   # requires the devtools package
namcap camembert-*.pkg.tar.zst
```

## Publishing

The AUR account and its SSH key belong to a person, not to this repository:
create the account at [aur.archlinux.org](https://aur.archlinux.org) and
upload a public key there first.

```bash
git clone ssh://aur@aur.archlinux.org/camembert.git aur-camembert
cp packaging/aur/{PKGBUILD,.SRCINFO} aur-camembert/
cd aur-camembert && git add PKGBUILD .SRCINFO && git commit && git push
```

The AUR rejects a push whose `.SRCINFO` disagrees with its `PKGBUILD`, so
regenerate it whenever the `PKGBUILD` changes — including for a `pkgrel`
bump.

## Automation, deliberately not done yet

Pushing to the AUR from the release workflow needs an SSH deploy key held as
a repository secret. At the current release cadence that trades a two-minute
manual step for a credential in CI and a job that can fail silently after the
release has already gone out. Revisit when releases are frequent enough that
the manual step actually gets forgotten.
