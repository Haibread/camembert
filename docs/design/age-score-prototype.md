# "Big AND cold" scoring — prototype on real trees

Status: **prototype / evidence**, not a design decision and not an
implementation. No production code was written for this document; every
number below comes from Python run over camembert dumps of real trees on
one developer machine. The question it answers is narrow: *if we built the
"size × age" score the README promises, what would it actually show?*

Binding context this prototype does **not** revisit:
[query-decisions.md](query-decisions.md) settles that camembert stores
**mtime only** and never atime (`relatime` makes atime a daily-granularity
lie, `noatime` removes it entirely). Every "age" below is mtime-age:
*not modified since*, never *not read since*.

## 1. The promise and the gap

`README.md` line 31 advertises:

> **What is big *and* cold?** — size × age, visible at a glance.

The original ideation document
([handoff-original.md](handoff-original.md) §5) says the same:
« Dimension âge — le bon candidat à suppression est gros **et** froid.
Tri/score (taille × ancienneté). »

What exists today is `SortKey::Mtime` (sort by date alone) and the
`older:` / `newer:` query predicates. There is no score and no "big and
cold" view. This document asks whether one is worth building.

## 2. Method

Everything is reproducible from the two scratch scripts and the commands
below. The scripts live outside the repo on purpose (analysis, not
product); they are ~250 lines of Python whose only dependency is
`zstdcat`.

```bash
cargo build --release

# one dump per tree; --one-filesystem because /home is btrfs and
# crossing into .snapshots would multiply-count snapshotted data
./target/release/camembert "$HOME"         --no-ui --one-filesystem -o home.cmbt
./target/release/camembert "$HOME/.cache"  --no-ui --one-filesystem -o cache.cmbt
./target/release/camembert "$HOME/git"     --no-ui --one-filesystem -o git.cmbt
./target/release/camembert "$HOME/Downloads" --no-ui --one-filesystem -o downloads.cmbt
./target/release/camembert /usr            --no-ui --one-filesystem -o usr.cmbt

# analysis (scratchpad scripts, see appendix for what they do)
python3 score.py home.cmbt "\$HOME"   # top-20 per formula + overlap matrix + dir variants
python3 dist.py  home.cmbt "\$HOME"   # joint size x age grid + mtime anomaly probes
```

Dump parsing follows [dump-v1.md](../format/dump-v1.md): `%XX`-decoded
names (§4), u64 fields accepted as number *or* string (§5), `t:"d"` blocks
with `ta`/`td`/`tn` subtree totals (§6.2), entry lines keyed on `m`/`a`/`d`
(§6.4). The reader skips `ex`/`err` entries and non-regular files (`k`),
and dedupes hardlinks by `(dev, ino)` keeping the first occurrence, which
in an ordered dump is the canonical owner (§8). Sizes below are **apparent**
(`a`); the machine's btrfs is zstd-compressed, so `d` (st_blocks) is not a
physical-truth column here and switching to it does not change any ranking
in this document.

## 3. Trees scanned

| tree | entries | dirs | apparent | real (disk) | scan | notes |
|---|---|---|---|---|---|---|
| `$HOME` | 998 663 | 98 261 | 69.3 GiB | 71.0 GiB | 559 ms | 2 mounts excluded; 7 689 hardlinked inodes |
| `~/.cache` | 207 647 | 20 452 | 29.7 GiB | 30.1 GiB | 890 ms | 43 % of home, the "obvious" cleanup target |
| `~/git` | 243 052 | 27 976 | 9.4 GiB | 10.0 GiB | 1 162 ms | dev tree: `.venv`, `node_modules`, `.next` |
| `~/Downloads` | 290 477 | 16 203 | 13.2 GiB | 13.4 GiB | 123 ms | includes the camembert checkout itself |
| `/usr` | 634 355 | 27 608 | 17.4 GiB | 18.9 GiB | 750 ms | second mtime population: package install dates |

Scanning a 1 M-entry, 71 GiB home in **0.56 s** is itself a datapoint: at
these speeds nothing about the age question is bottlenecked by scanning.
(Cache warmth differs between rows — `~/git` was cold, `~/Downloads` warm
from the preceding `$HOME` pass — so these are not benchmark numbers.)

`/usr` was added after the four home-side trees turned out to have almost
no age spread at all (§6). It is the only tree here where the age axis has
anything to say.

## 4. The candidate formulas

Let `s` = apparent size in bytes, `a` = age in days = `now − mtime`
clamped at 0.

| id | definition | intent |
|---|---|---|
| **F0** | `s` | control: sort by size, ignore age entirely |
| **F1** | `s · a` | the naive reading of "size × age" |
| **F2** | `s · ln(1 + a)` | damp the age axis so an ancient tiny file cannot outrank a fresh huge one |
| **F3** | `s · max(0, a − 180)` | nothing counts as cold before a 6-month grace period |
| **F7** | `s · min(a, 1096)` | saturating age: past 3 years, older stops mattering |
| **F4** | `pct(s) · pct(a)` | percentile-cross: scale-free, no unit mixing |
| **F5** | `s · a` in byte-years | "wasted byte-years" — an intuitive unit |
| **F6** | `s` **if** `s ≥ 100 MiB` **and** `a ≥ 365`, else excluded | quadrant: both floors, then sort by size |

**F5 is not a distinct ranking.** Byte-days, byte-years and byte-centuries
are the same order as F1 up to a positive constant. It is a *presentation*
choice for F1, not a competitor, and it is the only presentation of F1 that
is honest about what the number means. Treated as F1 throughout.

## 5. Results

### 5.1 `$HOME` — 894 446 files, 69.3 GiB

**F1 `s · a`** (top 20)

| # | size | age | path |
|---|---|---|---|
| 1 | 971.4 MiB | 0.1y | `~/.stack/pantry/hackage/00-index.tar` |
| 2 | 929.1 MiB | 0.1y | `...e/uv/archive-v0/mY41Jlvk4a0h438y/torch/lib/libtorch_cuda.so` |
| 3 | 844.6 MiB | 0.1y | `~/.stack/pantry/pantry.sqlite3` |
| 4 | 522.0 MiB | 0.1y | `...uAZMT0dw/nvidia/cudnn/lib/libcudnn_engines_precompiled.so.9` |
| 5 | 516.5 MiB | 0.1y | `...chive-v0/nS-HJtKCi4fwmRWK/nvidia/cu13/lib/libcublasLt.so.13` |
| 6 | 516.5 MiB | 0.1y | `.../python3.12/site-packages/nvidia/cu13/lib/libcublasLt.so.13` |
| 7 | 2.5 MiB | 20.0y | `....io-1949cf8c6b5b557f/csv-1.4.0/examples/data/bench/game.csv` |
| 8 | 2.5 MiB | 20.0y | `...x.crates.io-1949cf8c6b5b557f/encoding_rs-0.8.35/src/data.rs` |
| 9 | 468.4 MiB | 0.1y | `...ive-v0/LfXmDONqkxqTObuL/nvidia/cublas/lib/libcublasLt.so.12` |
| 10 | 447.7 MiB | 0.1y | `~/.cache/uv/archive-v0/xN2e5Vc5NPARwxs3/triton/_C/libtriton.so` |
| 11 | 440.9 MiB | 0.1y | `...e/uv/archive-v0/IPwPDQ-QmmUxer4j/torch/lib/libtorch_cuda.so` |
| 12 | 448.1 MiB | 0.1y | `~/.cache/uv/archive-v0/GEzjrsGKh7wsM6Xu/triton/_C/libtriton.so` |
| 13 | 448.1 MiB | 0.1y | `...n/.venv/lib/python3.12/site-packages/triton/_C/libtriton.so` |
| 14 | 431.0 MiB | 0.1y | `...0/FdXtJ-K20831zOQk/nvidia/cusparselt/lib/libcusparseLt.so.0` |
| 15 | 440.9 MiB | 0.1y | `...e/uv/archive-v0/ALnR5CtM9UQDixGy/torch/lib/libtorch_cuda.so` |
| 16 | 440.9 MiB | 0.1y | `...env/lib/python3.12/site-packages/torch/lib/libtorch_cuda.so` |
| 17 | 426.1 MiB | 0.1y | `...he/uv/archive-v0/IPwPDQ-QmmUxer4j/torch/lib/libtorch_cpu.so` |
| 18 | 426.1 MiB | 0.1y | `...he/uv/archive-v0/ALnR5CtM9UQDixGy/torch/lib/libtorch_cpu.so` |
| 19 | 426.1 MiB | 0.1y | `...venv/lib/python3.12/site-packages/torch/lib/libtorch_cpu.so` |
| 20 | 411.0 MiB | 0.1y | `...he/uv/archive-v0/mY41Jlvk4a0h438y/torch/lib/libtorch_cpu.so` |

Eighteen of twenty rows are 0.1-year-old CUDA shared objects. Ranks 7 and
8 are 2.5 MiB files that claim to be **20 years old** — hold that thought
(§6). The age factor achieved: reordering ranks 7–8 into the middle of a
size sort.

**F2 `s · ln(1+a)`** (top 20) — the same list, minus the two 20-year
intruders, plus three fresh large files (a downloaded `.webm`, an
in-progress `.mp4.part`, a docker layer tarball) that F1 had pushed out:

| # | size | age | path |
|---|---|---|---|
| 1 | 971.4 MiB | 0.1y | `~/.stack/pantry/hackage/00-index.tar` |
| 2 | 929.1 MiB | 0.1y | `...e/uv/archive-v0/mY41Jlvk4a0h438y/torch/lib/libtorch_cuda.so` |
| 3 | 844.6 MiB | 0.1y | `~/.stack/pantry/pantry.sqlite3` |
| 4 | 522.0 MiB | 0.1y | `...uAZMT0dw/nvidia/cudnn/lib/libcudnn_engines_precompiled.so.9` |
| 5 | 967.0 MiB | 0.0y | `~/Videos/...Apple Xserve [DJ6dWOVZh2k].webm` |
| 6 | 516.5 MiB | 0.1y | `...chive-v0/nS-HJtKCi4fwmRWK/nvidia/cu13/lib/libcublasLt.so.13` |
| 7 | 516.5 MiB | 0.1y | `.../python3.12/site-packages/nvidia/cu13/lib/libcublasLt.so.13` |
| 8 | 905.4 MiB | 0.0y | `~/Videos/...ENQUÊTE D'ACTION - M6+ MAG [fV3JMPRqnko].f401.mp4.part` |
| 9 | 468.4 MiB | 0.1y | `...ive-v0/LfXmDONqkxqTObuL/nvidia/cublas/lib/libcublasLt.so.12` |
| 10 | 519.6 MiB | 0.1y | `...8ad70adf8e6436f195cc429825ffb85f95afcdb5d8d9deb576f3e93.tar` |
| 11–20 | 336–448 MiB | 0.1y | ten more `torch`/`triton`/`nvidia` `.so` files |

**F3 `s · max(0, a−180)`** (top 20) — every single row is
`~/.cargo/registry/src/…`, all stamped 20.0y or 8.5y, none larger than
2.5 MiB:

| # | size | age | path |
|---|---|---|---|
| 1 | 2.5 MiB | 20.0y | `...index.crates.io-…/csv-1.4.0/examples/data/bench/game.csv` |
| 2 | 2.5 MiB | 20.0y | `...index.crates.io-…/encoding_rs-0.8.35/src/data.rs` |
| 3 | 1.9 MiB | 20.0y | `...index.crates.io-…/petgraph-0.8.3/tests/res/graph_1000n_1000e.txt` |
| 4 | 1.9 MiB | 20.0y | `...index.crates.io-…/petgraph-0.8.3/tests/res/graph_1000n_1000e_iso.txt` |
| 5 | 1.5 MiB | 20.0y | `...index.crates.io-…/termwiz-0.23.3/src/emoji_variation.rs` |
| 6 | 1.5 MiB | 20.0y | `...index.crates.io-…/unicode-width-0.2.2/src/tables.rs` |
| 7 | 1.4 MiB | 20.0y | `...index.crates.io-…/unicode-width-0.1.14/src/tables.rs` |
| 8 | 1.3 MiB | 20.0y | `...index.crates.io-…/csv-1.4.0/examples/data/bench/nfl.csv` |
| 9 | 933.3 KiB | 20.0y | `...index.crates.io-…/csv-1.4.0/examples/data/bench/worldcitiespop.csv` |
| 10 | 888.9 KiB | 20.0y | `...index.crates.io-…/windows-sys-0.61.2/src/Windows/Wdk/…/mod.rs` |
| 11–20 | 0.6–1.6 MiB | 8.5–20.0y | ten more crate sources, same registry |

**F4 `pct(s) · pct(a)`** (top 20) — 18 of the 20 rows are the same
`~/.cargo/registry` files as F3, in nearly the same order, with the
largest entry being 2.5 MiB.

**F6 quadrant (≥100 MiB and ≥1 y)** — *empty*. Zero rows.

Top-20 overlap between formulas on `$HOME`:

| | F0 (size) | F1 | F2 | F3 | F7 | F4 | F6 |
|---|---|---|---|---|---|---|---|
| F0 | 20 | 11 | 14 | 0 | 11 | 0 | 0 |
| F1 | 11 | 20 | 17 | 2 | 18 | 2 | 0 |
| F2 | 14 | 17 | 20 | 0 | 17 | 0 | 0 |
| F3 | 0 | 2 | 0 | 20 | 0 | 18 | 0 |
| F7 | 11 | 18 | 17 | 0 | 20 | 0 | 0 |
| F4 | 0 | 2 | 0 | 18 | 0 | 20 | 0 |
| F6 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |

Two clusters and nothing in between: {F0, F1, F2, F7} are size sorts with
decoration (11–18 shared rows out of 20), {F3, F4} are age sorts with
decoration (18 shared rows). Nothing is doing "big **and** cold" — each
formula collapses onto one axis or the other.

### 5.2 `/usr` — 499 690 files, 17.4 GiB (the only tree with real age spread)

**F1 `s · a`**: rank 1 is `libOpenImageDenoise_device_hip.so` (190.8 MiB,
3 months); rank 2 is a **KDE wallpaper PNG** (10.4 MiB, 4.4 years); ranks
5–17 are the Noto CJK font collection (1.5 years). Fourteen of the twenty
rows are `/usr/share/fonts` and `/usr/share/wallpapers` — package-owned
files you must not `rm` (pacman would flag the package as broken, and the
fonts are why CJK text renders).

**F5 in its own unit**, on the same data, shows why F1 does that:

| # | size | age (y) | byte-years (MiB·y) | path |
|---|---|---|---|---|
| 1 | 190.8 MiB | 0.24 | 47 | `/usr/lib/libOpenImageDenoise_device_hip.so.2.4.1` |
| 2 | 10.4 MiB | 4.41 | 46 | `/usr/share/wallpapers/Lines/contents/images/3840x2160.png` |
| 3 | 95.4 MiB | 0.42 | 40 | `/usr/bin/minikube` |
| 4 | 8.9 MiB | 4.41 | 39 | `/usr/share/wallpapers/Liquid/contents/images/3840x2160.png` |
| 5 | 26.1 MiB | 1.47 | 38 | `/usr/share/fonts/noto-cjk/NotoSerifCJK-Bold.ttc` |
| 6 | 25.4 MiB | 1.47 | 37 | `/usr/share/fonts/noto-cjk/NotoSerifCJK-Medium.ttc` |

A 191 MiB library installed three months ago and a 10 MiB wallpaper from
2022 are, to F1, *the same problem*. That is the whole objection to
multiplying a byte by a day in one sentence: the product is a real
quantity, but no user has ever wanted to free "47 MiB·years".

**F2 `s · ln(1+a)`** on `/usr` degenerates the other way: 16 of 20 rows
match the pure size sort. The list is `chromium`, `libxul.so`,
`slack`, `code`, `godot`, `k9s`, `firefox` — every large binary on the
system, all 0.0–0.1 y old. The log damping is so aggressive that a
4-year-old file gets a factor of ln(1610) ≈ 7.4 against a 1-month-old
file's ln(31) ≈ 3.5: a 2.1× age advantage against size ratios of 20×.

**F4 `pct(s) · pct(a)`** on `/usr` is the clearest failure in this
document. All twenty rows are `/usr/share/wallpapers` and
`/usr/share/plasma/look-and-feel` PNG/JPG assets, **the largest of which
is 10.4 MiB and ten of which are under 1 MiB**. The mechanism: 99.67 % of
files in `/usr` are under 1 MiB, so a 420 KiB screenshot already sits in
the top size percentile and pairs it with a top age percentile, while a
296 MiB `chromium` binary installed last week pairs a top size percentile
with a bottom age percentile. Percentile-cross is scale-free in
exactly the wrong way — it discards the fact that the difference between
420 KiB and 296 MiB is 700×, which is the entire reason a disk-usage tool
exists.

**F6 quadrant** at the stated floors (≥100 MiB, ≥1 y): *empty again*. At
≥10 MiB, ≥1 y it returns 16 files totalling 323.5 MiB — the Noto CJK
fonts and one wallpaper. Which is to say: the honest answer for `/usr` is
"there is 323 MiB of big-and-old here out of 18.9 GiB, and it's your
fonts."

### 5.3 `~/.cache`, `~/git`, `~/Downloads` — condensed

Full tables were generated for all three; they are omitted here because
they say the same thing three more times, and the overlap matrices carry
the information more compactly.

| tree | F0∩F1 | F0∩F2 | F0∩F3 | F0∩F4 | F6 rows @100 MiB/1y | F6 rows @10 MiB/1y |
|---|---|---|---|---|---|---|
| `$HOME` | 11 | 14 | 0 | 0 | **0** | **0** |
| `~/.cache` | 14 | 17 | 0 | 0 | **0** | **0** |
| `~/git` | 17 | 19 | 0 | 0 | **0** | **0** |
| `~/Downloads` | 17 | 17 | 0 | 0 | **0** | **0** |
| `/usr` | 2 | 16 | 0 | 0 | **0** | 16 |

(`F0∩Fx` = how many of the size-only top 20 survive in formula *x*'s top
20.)

On `~/Downloads`, F3 returns **zero rows** — not one file in 13.2 GiB is
older than six months. On `~/.cache` and `~/Downloads`, F1 and F7 produce
*byte-identical* top-20s (20/20 overlap with each other) because no file
is anywhere near the 3-year cap, so the cap never binds and F7 is F1 under
a different name. The one genuinely
user-deletable item any formula surfaced across the home trees is
`~/Downloads/streamlit-streamlit_app-2026-07-01-16-44-02.webm` (53.4 MiB,
one month old) at F1 rank 10 on `~/Downloads` — and it got there by being
*recent*, not cold.

## 6. The finding that reframes the question

### 6.1 There is no big-and-cold data on this machine

The joint size × age grid for `$HOME` (counts / apparent bytes):

| size \ age | <1mo | 1–6mo | 6mo–1y | 1–2y | >2y | total |
|---|---|---|---|---|---|---|
| <1M | 641 233 / 6.6 GiB | 220 552 / 2.6 GiB | 1 368 / 3.6 MiB | — | 27 104 / 366.4 MiB | 890 257 / 9.6 GiB |
| 1–10M | 2 694 / 7.5 GiB | 678 / 1.9 GiB | — | — | 12 / 19.7 MiB | 3 384 / 9.4 GiB |
| 10–100M | 504 / 11.7 GiB | 176 / 5.4 GiB | — | — | — | 680 / 17.1 GiB |
| 100M–1G | 58 / 15.4 GiB | 67 / 17.8 GiB | — | — | — | 125 / 33.3 GiB |
| >1G | — | — | — | — | — | 0 |

Bytes past both floors, `$HOME`:

| size floor \ age floor | 6mo | 1y | 2y | 5y |
|---|---|---|---|---|
| 1 MiB | 12 / 19.7 MiB | 12 / 19.7 MiB | 12 / 19.7 MiB | 12 / 19.7 MiB |
| 10 MiB | 0 | 0 | 0 | 0 |
| 100 MiB | 0 | 0 | 0 | 0 |
| 1 GiB | 0 | 0 | 0 | 0 |

**In 69.3 GiB and 894 446 files there is not one file above 10 MiB older
than six months.** The entire "cold" mass of this home directory (>2 y) is
386.1 MiB — 0.54 % — and all of it, byte for byte, lives in
`~/.cargo/registry`.

### 6.2 …and the "old" data that does exist is fake

Enumerating the distinct mtimes among files older than 2 years in `$HOME`
gives 164 distinct values covering 27 116 files. The top of that list:

| mtime | date | files | what it is |
|---|---|---|---|
| `1153704088` | 2006-07-24 | **22 599** | cargo's fixed timestamp on extracted registry sources |
| `1516166490–3` | 2018-01-17 | 2 671 | one upstream tarball's build clock, preserved by extraction |
| `123456789` | 1973-11-29 | 523 | a literal joke constant in some package's test fixtures |
| `1` | 1970-01-01 | 308 | reproducible-build zero |

83 % of the "oldest" files on this machine share **one** mtime, to the
second, because cargo stamps a constant into the tarballs it unpacks. F3
and F4 rank that constant at the top of their lists with total confidence.

Two more anomalies worth recording, both of which any real implementation
must handle:

- **308 files with `mtime ≤ 1`**, i.e. an apparent age of 56.6 years.
- **5 directories with a negative mtime**, `m = -2785708037` →
  1881-08-…, under `~/Downloads/spike-tts-2/`. This is not a camembert
  bug (the dump faithfully records what `statx` returned) but it means
  `now − mtime` can be ~145 years, and a directory-level `s · a` score
  puts a 34 MiB directory at rank 1 of the whole home tree purely on the
  strength of a corrupt timestamp. Any age arithmetic needs a sanity
  clamp and, ideally, a visible badge rather than a silent clamp.

The mtime-honesty caveat already written into
[query-decisions.md](query-decisions.md) — "`cp -p`/`rsync -a`/`tar -x`
preserve source mtimes, so a file copied yesterday can read as ten years
old" — turns out to be not an edge case but *the dominant behaviour of the
old tail on a developer machine*. A predicate (`older:1y`) states that
caveat once and lets the user judge. A score silently multiplies it by the
file size and sorts by the result.

### 6.3 How representative is this?

One machine, honestly. The home directory here has genuinely young data:
the largest real user files (videos, downloads, `.venv`s) are all under six
months old, which is what a working dev box looks like a while after a
reinstall or a migration. A ten-year-old photo library or a fileserver's
`/srv` would populate the empty quadrant.

But that cuts both ways, and the direction it cuts is against the score:

- On a machine **with** cold data, a `>1G older:2y` query returns it
  directly and says how much there is. The score adds nothing.
- On a machine **without** cold data — this one — the score still returns
  twenty confident rows. F3 and F4 return `.cargo` sources; F1 and F2
  return whatever is biggest. **A score can never say "nothing".** The
  quadrant can, and did, five times out of five.

`/usr` is the counter-sample where age genuinely varies (package install
dates spread over 4+ years), and there the score's failure is not emptiness
but *wrongness*: it recommends deleting fonts and wallpapers.

## 7. Directory granularity

A directory has no mtime that means what a user thinks it means: the
inode's own mtime records only direct-child churn (create/unlink/rename in
that directory), not modification of anything below it. Four candidate
definitions, all scored as `subtree_apparent × age`:

- **D-own** — the directory inode's own `m`.
- **D-max** — newest file mtime anywhere in the subtree. "Cold" = nothing
  in here has been touched. This is the only definition that matches the
  intent.
- **D-med** — size-weighted median file mtime in the subtree.
- **D-min** — oldest file mtime in the subtree.

On `$HOME`, D-own's top 5 are the five directories with the corrupt
negative mtime (`~/Downloads/spike-tts-2/refs`, 34.7 MiB, "144.8 years
old"), ahead of `~/.cache/paru` at 4.8 GiB. D-min's rank 1 is `~` itself
at "56.6 years" because one `mtime=1` file somewhere below it drags the
whole home directory to the top — D-min is structurally broken, since the
score of every directory is set by its single oldest descendant. D-med
ranks `~/.cargo` first, at "20.0 years", for the reasons in §6.2.

D-max is the only variant that behaves, and on `~/git` — the dev tree the
brief asked about specifically — it produces this:

| # | subtree | age | path |
|---|---|---|---|
| 1 | 5.0 GiB | 0.1y | `~/git/work/pyannote-diarization/.venv` |
| 2 | 5.0 GiB | 0.1y | `~/git/work/pyannote-diarization/.venv/lib` |
| 3 | 5.0 GiB | 0.1y | `~/git/work/pyannote-diarization/.venv/lib/python3.12` |
| 4 | 5.0 GiB | 0.1y | `…/.venv/lib/python3.12/site-packages` |
| 5 | 2.7 GiB | 0.1y | `…/site-packages/nvidia` |
| 6 | 1.7 GiB | 0.1y | `…/site-packages/nvidia/cu13` |
| 7 | 1.7 GiB | 0.1y | `…/site-packages/nvidia/cu13/lib` |
| 8 | 6.5 GiB | 0.0y | `~/git/work` |
| 9 | 5.1 GiB | 0.0y | `~/git/work/pyannote-diarization` |
| 10–20 | … | 0.0–0.1y | eight more descendants of the same `.venv`, plus `poc-app-fitness` |

Nineteen of twenty rows are nested ancestors and descendants of **one**
`.venv`. This is the ancestor-chain problem, and it is not specific to the
age score — it is what a flat ranking over a tree always does — but it
means a directory-level "big and cold" view needs a suppression rule
("hide a directory whose parent is already listed and whose score is
within *x* % of it") before it shows anything. That rule is a design
question in its own right, with its own tuning constant, and none of it is
about age.

The cheap version of the same answer already exists: `camembert /usr
--filter '>10M older:1y'` re-aggregates the whole tree over the matching
files and prints matched subtree totals per directory, so the directory
rollup is free and already correct. The prototype found no reason to
define a directory *age* at all.

## 8. Judgment, formula by formula

| formula | verdict | why |
|---|---|---|
| **F1 `s·a`** | reject | Multiplies incommensurable units; ties a 191 MiB fresh library with a 10 MiB 4-year-old wallpaper. Shares 11–17 of 20 rows with a plain size sort on the four home-side trees, and on the one tree where it diverges (`/usr`, 2/20) it recommends deleting fonts. |
| **F2 `s·ln(1+a)`** | reject | Works exactly as designed — and the design is a size sort. 14–19 of 20 rows match F0 on every tree. If the answer is "sort by size", ship the sort, not a logarithm. |
| **F3 `s·(a−180)⁺`** | reject | The grace period is right in principle and it is the formula I expected to win. In practice it inverts into a pure age sort (0/20 overlap with size on every tree) and its entire top 20 on `$HOME` is `~/.cargo/registry` sources under 3 MiB, carrying tarball-preserved timestamps rather than real age. |
| **F4 `pct(s)·pct(a)`** | reject, hardest | Scale-free means size-blind. Its `/usr` top 20 tops out at 10.4 MiB with eleven rows under 1 MiB, in a tree containing 296 MiB binaries. A disk-usage tool that cannot tell 420 KiB from 296 MiB has lost the plot. |
| **F5 byte-years** | not a formula | Rank-identical to F1. Useful only as F1's honest label — and reading that label ("47 MiB·years") is what makes F1 obviously wrong. |
| **F7 `s·min(a,3y)`** | reject | Same cluster as F1/F2; the cap never binds on trees where nothing is over 3 years, and where it does bind (`/usr`) it is still F1. |
| **F6 quadrant** | **the one that works** | It is the only candidate that returned *nothing* when there was nothing, five times out of five. When it returns rows they are defensible ("16 files, 323.5 MiB, your CJK fonts"), and the two floors are exactly the two questions the user actually has ("how big do I care about?" / "how stale is stale?"), asked explicitly instead of encoded in a hidden exchange rate. |

The pattern across all of them: **a single scalar score has to pick an
exchange rate between bytes and days, and there is no defensible one.**
`s·a` sets it at "1 MiB is worth 1 day". `s·ln a` sets it near zero.
`pct·pct` sets it at "one rank position of size = one rank position of
age". Every choice is arbitrary, invisible to the user, and wrong on some
tree. F6 refuses to set one, and is therefore the only candidate whose
output a user can reason about.

## 9. Recommendation

**Build nothing. Delete the promise, or restate it as the query it already
is.**

The evidence:

1. F6 — "above a size floor AND above an age floor, sorted by size" — beat
   every scoring formula on every tree, and F6 **is already implemented**.
   `camembert /usr --no-ui --filter '>10M older:1y'` produces exactly the
   F6 top-20, plus a re-aggregated directory rollup, plus the honest
   headline `matched: 323.5 MiB, 16 entries — of 18.9 GiB real scanned`.
   The query language shipped the winning formula before this prototype
   started.
2. Every continuous score collapses onto one axis (§5.1 overlap matrix)
   and none of them can express "there is nothing here", which was the
   correct answer for four of the five trees.
3. mtime's known unreliability is not a footnote on this data — it is
   83 % of the old tail (§6.2). A predicate the user typed carries that
   caveat legibly. A score that silently multiplies a fabricated 2006
   timestamp by a file size does not.

Concretely, three things worth doing, none of which is a score:

- **Fix `README.md` line 31.** "size × age, visible at a glance" describes
  a feature that does not exist and, on this evidence, should not. Replace
  it with what the tool does: `>1G older:1y` — one filter, re-aggregated,
  with an honest "nothing matched" when nothing matches. Same for the
  `--help` text if it repeats the claim.
- **Consider a preset, not a score.** If "big and cold" deserves a
  one-keystroke affordance in the TUI, make it a *named filter preset*
  that expands to a visible query string the user can then edit — the
  config file already has a `stale = "older:1y"` key, which is the right
  shape. The user sees `>1G older:1y` in the filter bar, understands why
  each row is there, and can move either floor. A score gives them a
  number they cannot interpret and cannot adjust.
- **Clamp and badge absurd mtimes.** Negative mtimes, `mtime ≤ 1`, and
  future mtimes exist in the wild on this very machine (§6.2). Whatever
  consumes age should clamp the arithmetic and surface the anomaly rather
  than rank on it. This is a small honesty feature entirely in the spirit
  of the project thesis, and it is worth more than the score.

If a score is built anyway — because a treemap or a wheel needs a
continuous colour channel, which is a legitimate reason a threshold cannot
serve — then F3 with a *user-visible, user-editable* grace period is the
least-bad basis, and it must ship alongside the count of how many items
cleared the grace period at all. On four of the five trees here, that
count would have been zero, and printing "0 items older than 6 months"
would have been more useful than any ranking.

## 10. What this prototype could not answer

- **Whether a machine with genuine cold data changes the verdict.** No
  tree here had a populated big-and-cold quadrant. The argument that F6
  still wins there (§6.3) is reasoning, not measurement. A scan of a
  long-lived fileserver, a NAS, or a 10-year-old home directory would test
  it, and would be worth doing before the README wording is finalised.
- **The cost of the age axis at scale.** Nothing was measured about
  computing a score over 10 M nodes inside the owner-side accumulator, or
  about whether a score column changes the flat-view fold's cost. If a
  score is ever built, that measurement is required by the benchmark rule
  in `CLAUDE.md`; this document does not discharge it.
- **The ancestor-chain suppression rule** (§7). A directory-level view
  needs one, its constant needs tuning against real trees, and the right
  answer may be structural (only show a directory if its parent is not
  shown) rather than numeric. Out of scope here.
- **Whether users read a score as a recommendation.** F1's `/usr` output
  lists `/usr/share/fonts/noto-cjk` seven times in the top ten. Whether a
  user would actually delete them, or would recognise the list as junk, is
  a UX question this data cannot settle — but it is the reason a score
  that cannot say "nothing" is dangerous in a tool that also has a delete
  key.

## Appendix — scripts

Two throwaway Python files, kept in the session scratchpad and
deliberately not committed:

- `score.py` — dump reader (v1 spec: `%XX` names, string-or-number u64,
  hardlink dedup, `ex`/`err`/non-regular skipping) + the seven formulas +
  top-20 tables + top-20 overlap matrix + the four directory-age variants.
- `dist.py` — joint size × age grid, bytes-past-both-floors table, and the
  mtime-anomaly probes (future mtimes, `≤ 1` mtimes, most-frequent exact
  mtimes, out-of-range directory mtimes).

Reconstructing them is a couple of hours' work from this document; the
dumps themselves are the reproducible artifact, and the commands in §2
regenerate them.
