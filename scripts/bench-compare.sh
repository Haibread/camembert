#!/usr/bin/env bash
# Compare camembert's scan speed against known disk-usage tools on a
# deterministic synthetic tree, to catch performance regressions and
# keep an external reference point (see CLAUDE.md "Benchmarks").
#
# Usage:
#   scripts/bench-compare.sh [--files N] [--cold] [--tree DIR]
#
#   --files N   size of the synthetic tree (default 200000 files)
#   --cold      drop the page cache before each run (needs sudo)
#   --tree DIR  use an existing directory instead of the synthetic tree
#               (results are then machine- and content-specific)
#
# Competitors are picked up from PATH and from target/bench-tools/bin
# (populate the latter with:
#   cargo install --locked --root target/bench-tools \
#     hyperfine du-dust dua-cli parallel-disk-usage diskus
# ncdu and gdu are C/Go tools — install them system-wide if you want
# them in the comparison; the script includes whatever it finds.)
#
# Results: printed table, plus markdown + JSON exports under
# target/bench-results/ (kept out of git) when hyperfine is available.

set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="$PWD/target/bench-tools/bin:$PATH"

FILES=200000
COLD=0
TREE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --files) FILES="$2"; shift 2 ;;
        --cold) COLD=1; shift ;;
        --tree) TREE="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# --- synthetic tree -------------------------------------------------
# Deterministic layout: WIDTH top dirs, each with a src/ fan-out and a
# node_modules/ subtree (so the pattern machinery has something real),
# FILES files total, empty (scans are metadata-bound; content does not
# change what du/dust/camembert have to stat).
if [ -z "$TREE" ]; then
    TREE="$PWD/target/bench-tree-$FILES"
    STAMP="$TREE/.complete"
    if [ ! -f "$STAMP" ]; then
        echo "generating synthetic tree ($FILES files) in $TREE ..." >&2
        rm -rf "$TREE"
        python3 - "$TREE" "$FILES" <<'EOF'
import os, sys
root, files = sys.argv[1], int(sys.argv[2])
width = 100
per_dir = 50
made = 0
d = 0
while made < files:
    top = os.path.join(root, f"proj{d % width:03d}")
    for sub in ("src", os.path.join("node_modules", f"dep{d:04d}", "lib")):
        path = os.path.join(top, sub, f"batch{d:05d}")
        os.makedirs(path, exist_ok=True)
        for i in range(per_dir):
            if made >= files:
                break
            open(os.path.join(path, f"f{i:03d}.js" if i % 3 else f"f{i:03d}.log"), "w").close()
            made += 1
    d += 1
open(os.path.join(root, ".complete"), "w").close()
EOF
    fi
    echo "tree: $TREE ($FILES files, cached)" >&2
fi

# --- build ----------------------------------------------------------
echo "building camembert --release ..." >&2
cargo build --release --quiet
CAMEMBERT="$PWD/target/release/camembert"

# --- competitors ----------------------------------------------------
declare -a NAMES CMDS
add() { NAMES+=("$1"); CMDS+=("$2"); }

add "camembert" "$CAMEMBERT --no-ui '$TREE' > /dev/null"
add "du"        "du -sb '$TREE' > /dev/null"
command -v diskus > /dev/null && add "diskus" "diskus '$TREE' > /dev/null"
command -v dust   > /dev/null && add "dust"   "dust -d 0 '$TREE' > /dev/null"
command -v dua    > /dev/null && add "dua"    "dua -x aggregate '$TREE' > /dev/null"
command -v pdu    > /dev/null && add "pdu"    "pdu --max-depth 1 '$TREE' > /dev/null"
command -v ncdu   > /dev/null && add "ncdu"   "ncdu -0 -o /dev/null '$TREE'"
command -v gdu    > /dev/null && add "gdu"    "gdu -npc '$TREE' > /dev/null"

echo "competitors: ${NAMES[*]}" >&2

PREPARE=()
if [ "$COLD" = 1 ]; then
    echo "cold-cache mode: dropping the page cache before each run (sudo)" >&2
    PREPARE=(--prepare 'sync; sudo sh -c "echo 3 > /proc/sys/vm/drop_caches"')
fi

# --- run ------------------------------------------------------------
if command -v hyperfine > /dev/null; then
    OUT="target/bench-results"
    mkdir -p "$OUT"
    ts="$(date +%Y%m%d-%H%M%S)"
    ARGS=(--warmup 2 --min-runs 5 "${PREPARE[@]}"
          --export-markdown "$OUT/$ts.md" --export-json "$OUT/$ts.json")
    for i in "${!NAMES[@]}"; do
        ARGS+=(--command-name "${NAMES[$i]}" "${CMDS[$i]}")
    done
    hyperfine "${ARGS[@]}"
    cp "$OUT/$ts.md" "$OUT/latest.md"
    cp "$OUT/$ts.json" "$OUT/latest.json"
    echo "exports: $OUT/$ts.{md,json} (+ latest.*)" >&2
else
    # Fallback: crude best-of-5 wall clock via bash's time builtin.
    echo "hyperfine not found — falling back to best-of-5 wall clock" >&2
    for i in "${!NAMES[@]}"; do
        best=""
        for _ in 1 2 3 4 5; do
            [ "$COLD" = 1 ] && { sync; sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'; }
            start=$(date +%s.%N)
            bash -c "${CMDS[$i]}"
            end=$(date +%s.%N)
            t=$(echo "$end $start" | awk '{printf "%.3f", $1-$2}')
            if [ -z "$best" ] || awk "BEGIN{exit !($t < $best)}"; then best="$t"; fi
        done
        printf '%-12s %ss (best of 5)\n' "${NAMES[$i]}" "$best"
    done
fi
