#!/usr/bin/env bash
# Build the native .deb and .rpm packages from an already-configured
# release build (see camembert/Cargo.toml's [package.metadata.deb] and
# [package.metadata.generate-rpm]).
#
# Usage:
#   scripts/build-packages.sh [--target TRIPLE] [--deb-only|--rpm-only]
#
#   --target TRIPLE  cross/static target to build and package for
#                    (default: the host's default target)
#   --deb-only       skip the .rpm
#   --rpm-only       skip the .deb
#
# The packagers are picked up from PATH and from target/packaging-tools/bin
# (populate the latter with:
#   cargo install --locked --root target/packaging-tools \
#     cargo-deb cargo-generate-rpm
# ). Both are plain Rust binaries — no dpkg, rpmbuild, or Docker needed,
# which is why these packages can be built on any host.
#
# The man pages and completion scripts are regenerated from the live clap
# definitions on every run (into target/packaging/) rather than committed,
# so a flag added to src/cli.rs cannot ship a package documenting the old
# surface.
#
# Output: the built packages are printed at the end, under
# target/[<triple>/]debian/ and target/[<triple>/]generate-rpm/.

set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="$PWD/target/packaging-tools/bin:$PATH"

TARGET=""
WANT_DEB=1
WANT_RPM=1
while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --deb-only) WANT_RPM=0; shift ;;
        --rpm-only) WANT_DEB=0; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

missing=""
[ "$WANT_DEB" = 1 ] && ! command -v cargo-deb >/dev/null && missing="$missing cargo-deb"
[ "$WANT_RPM" = 1 ] && ! command -v cargo-generate-rpm >/dev/null && missing="$missing cargo-generate-rpm"
if [ -n "$missing" ]; then
    echo "missing packaging tool(s):$missing" >&2
    echo "install with: cargo install --locked --root target/packaging-tools$missing" >&2
    exit 1
fi

# --- release binary -------------------------------------------------
# `--locked` because a package built from a drifted lockfile is not the
# thing that was tested.
target_args=()
bin_dir="target/release"
if [ -n "$TARGET" ]; then
    target_args=(--target "$TARGET")
    bin_dir="target/$TARGET/release"
fi

echo "==> building the release binary${TARGET:+ for $TARGET}"
cargo build --release --locked --package camembert "${target_args[@]}"

# Strip here rather than letting cargo-deb do it, so the .deb and the .rpm
# carry the byte-identical binary (cargo-generate-rpm never strips).
if command -v strip >/dev/null; then
    strip "$bin_dir/camembert"
fi

# --- generated assets -----------------------------------------------
# The generators are build-time tools: whatever they are built for has to run
# on *this* machine. When the target's architecture matches the host's (which
# is the case for every release build — each is on its own native runner), the
# --target build above already produced runnable generators, so reuse them
# rather than compiling the whole workspace a second time for the host triple.
# A genuine cross-build falls back to the host toolchain and pays for it.
gen_args=()
if [ -n "$TARGET" ] && [ "${TARGET%%-*}" = "$(uname -m)" ]; then
    gen_args=(--target "$TARGET")
fi

echo "==> generating man pages and completions"
rm -rf target/packaging
cargo run --release --locked --package camembert --bin camembert-mangen \
    "${gen_args[@]}" -- target/packaging/man
cargo run --release --locked --package camembert --bin camembert-completions \
    "${gen_args[@]}" -- target/packaging/completions

# --- packages -------------------------------------------------------
built=()
if [ "$WANT_DEB" = 1 ]; then
    echo "==> building the .deb"
    # --no-build: reuse the binary built above. --no-strip: already stripped.
    cargo deb --no-build --no-strip --package camembert "${target_args[@]}"
    built+=("$(dirname "$bin_dir")/debian"/*.deb)
fi

if [ "$WANT_RPM" = 1 ]; then
    echo "==> building the .rpm"
    rpm_args=()
    [ -n "$TARGET" ] && rpm_args=(--target "$TARGET")
    cargo generate-rpm --package camembert "${rpm_args[@]}"
    built+=("$(dirname "$bin_dir")/generate-rpm"/*.rpm)
fi

echo
echo "==> built:"
for artifact in "${built[@]}"; do
    printf '    %s (%s)\n' "$artifact" "$(du -h "$artifact" | cut -f1)"
done
