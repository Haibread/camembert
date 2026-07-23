//! The donut camembert: pure rasterization of the viewed directory's
//! children into a subpixel grid, then composition into terminal cells
//! (design §Direction, slice 2).
//!
//! Two subpixel geometries, chosen by the glyph ladder:
//! - **half-blocks** (`▀`): 1×2 subpixels per cell — with a typical 1:2
//!   cell aspect, subpixels are square;
//! - **sextants** (U+1FB00..): 2×3 subpixels per cell — subpixels are
//!   0.75:1, corrected by the rasterizer's `px_aspect`.
//!
//! Everything here is terminal-free and unit-tested; `ui.rs` only maps
//! the resulting cell grid onto the frame buffer.

/// Donut hole radius as a fraction of the outer radius.
pub const HOLE_FRACTION: f64 = 0.42;

/// Slices below this fraction of the total merge into the gray "rest".
pub const MIN_SLICE_FRACTION: f64 = 0.02;

/// Subpixel aspect (width/height) for half-blocks: 1 column × ½ row on a
/// 1:2 cell is square.
pub const HALF_BLOCK_ASPECT: f64 = 1.0;

/// Subpixel aspect for sextants: ½ column × ⅓ row on a 1:2 cell.
pub const SEXTANT_ASPECT: f64 = 0.75;

/// A rasterized subpixel grid: each subpixel holds the index of the slice
/// covering it, or `None` outside the ring (hole included).
#[derive(Debug)]
pub struct PixelGrid {
    width: usize,
    height: usize,
    px: Vec<Option<u16>>,
}

impl PixelGrid {
    pub fn get(&self, x: usize, y: usize) -> Option<u16> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.px[y * self.width + x]
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }
}

/// Rasterize a donut of `fracs` (slice fractions, in display order,
/// summing to ~1 — they are normalized internally) into a `width` ×
/// `height` subpixel grid. `px_aspect` is the physical width/height of
/// one subpixel; the donut compensates so it stays circular. Slices start
/// at 12 o'clock and run clockwise. An empty/zero input yields an
/// all-`None` grid.
pub fn rasterize(fracs: &[f64], width: usize, height: usize, px_aspect: f64) -> PixelGrid {
    let mut grid = PixelGrid {
        width,
        height,
        px: vec![None; width * height],
    };
    let total: f64 = fracs.iter().copied().filter(|f| *f > 0.0).sum();
    if total <= 0.0 || width == 0 || height == 0 {
        return grid;
    }
    // Cumulative clockwise boundaries in turns of the (normalized) total.
    let mut boundaries = Vec::with_capacity(fracs.len());
    let mut acc = 0.0;
    for frac in fracs {
        acc += frac.max(0.0) / total;
        boundaries.push(acc);
    }

    // Physical space: subpixel height = 1, width = px_aspect.
    let (physical_w, physical_h) = (width as f64 * px_aspect, height as f64);
    let (cx, cy) = (physical_w / 2.0, physical_h / 2.0);
    let outer = physical_w.min(physical_h) / 2.0;
    let hole = outer * HOLE_FRACTION;
    for y in 0..height {
        for x in 0..width {
            let dx = (x as f64 + 0.5) * px_aspect - cx;
            let dy = (y as f64 + 0.5) - cy;
            let r2 = dx * dx + dy * dy;
            if r2 < hole * hole || r2 > outer * outer {
                continue;
            }
            // Angle from 12 o'clock, clockwise, in turns [0, 1).
            let turns = (dx.atan2(-dy) / std::f64::consts::TAU).rem_euclid(1.0);
            let slice = boundaries
                .iter()
                .position(|&b| turns < b)
                .unwrap_or(fracs.len() - 1);
            grid.px[y * width + x] = Some(slice as u16);
        }
    }
    grid
}

/// One composed terminal cell: glyph + slice indices for fg/bg (`None` =
/// terminal default). The caller maps indices to actual colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WheelCell {
    pub ch: char,
    pub fg: Option<u16>,
    pub bg: Option<u16>,
}

const EMPTY_CELL: WheelCell = WheelCell {
    ch: ' ',
    fg: None,
    bg: None,
};

/// Compose a half-block cell grid: cell (col, row) covers subpixels
/// (col, 2·row) and (col, 2·row + 1). `▀` carries the top subpixel as fg
/// and the bottom as bg; edge cells with one empty subpixel keep the
/// terminal-default background.
pub fn compose_half_blocks(grid: &PixelGrid) -> Vec<Vec<WheelCell>> {
    let rows = grid.height().div_ceil(2);
    (0..rows)
        .map(|row| {
            (0..grid.width())
                .map(|col| {
                    let top = grid.get(col, 2 * row);
                    let bottom = grid.get(col, 2 * row + 1);
                    match (top, bottom) {
                        (None, None) => EMPTY_CELL,
                        (Some(t), Some(b)) => WheelCell {
                            ch: '▀',
                            fg: Some(t),
                            bg: Some(b),
                        },
                        (Some(t), None) => WheelCell {
                            ch: '▀',
                            fg: Some(t),
                            bg: None,
                        },
                        (None, Some(b)) => WheelCell {
                            ch: '▄',
                            fg: Some(b),
                            bg: None,
                        },
                    }
                })
                .collect()
        })
        .collect()
}

/// Compose a sextant cell grid: cell (col, row) covers the 2×3 subpixels
/// starting at (2·col, 3·row). A cell holds at most two colors (fg
/// pattern + bg): the two most frequent slice colors win; at the ring's
/// outer edge (empty subpixels present) all lit subpixels join the fg
/// pattern so the disc stays solid, at the cost of ≤1 subpixel of color
/// bleed along slice boundaries.
pub fn compose_sextants(grid: &PixelGrid) -> Vec<Vec<WheelCell>> {
    let rows = grid.height().div_ceil(3);
    let cols = grid.width().div_ceil(2);
    (0..rows)
        .map(|row| {
            (0..cols)
                .map(|col| compose_sextant_cell(grid, col, row))
                .collect()
        })
        .collect()
}

fn compose_sextant_cell(grid: &PixelGrid, col: usize, row: usize) -> WheelCell {
    // (bit, slice) for each lit subpixel; bit = sy*2 + sx.
    let mut lit: Vec<(u8, u16)> = Vec::with_capacity(6);
    for sy in 0..3 {
        for sx in 0..2 {
            if let Some(slice) = grid.get(2 * col + sx, 3 * row + sy) {
                lit.push(((sy * 2 + sx) as u8, slice));
            }
        }
    }
    if lit.is_empty() {
        return EMPTY_CELL;
    }
    // Frequency per slice color, ties broken by smaller slice index.
    let mut counts: Vec<(u16, usize)> = Vec::with_capacity(2);
    for &(_, slice) in &lit {
        match counts.iter_mut().find(|(s, _)| *s == slice) {
            Some((_, n)) => *n += 1,
            None => counts.push((slice, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let primary = counts[0].0;

    if lit.len() == 6 && counts.len() > 1 {
        // Interior cell with a slice boundary: primary as pattern,
        // everything else as background.
        let secondary = counts[1].0;
        let bits = lit
            .iter()
            .filter(|&&(_, slice)| slice == primary)
            .fold(0u8, |bits, &(bit, _)| bits | 1 << bit);
        WheelCell {
            ch: sextant_char(bits),
            fg: Some(primary),
            bg: Some(secondary),
        }
    } else {
        // Edge cell (or single color): all lit subpixels in the pattern,
        // terminal-default background outside the ring.
        let bits = lit.iter().fold(0u8, |bits, &(bit, _)| bits | 1 << bit);
        WheelCell {
            ch: sextant_char(bits),
            fg: Some(primary),
            bg: None,
        }
    }
}

/// The character for a 2×3 subpixel pattern. Bit `sy*2 + sx` set means
/// subpixel (sx, sy) lit (bit 0 = upper left … bit 5 = lower right). The
/// Unicode 13 sextant block (U+1FB00..U+1FB3B) skips four patterns that
/// already exist elsewhere: empty, the left/right half blocks, and the
/// full block.
pub fn sextant_char(bits: u8) -> char {
    match bits & 0x3f {
        0 => ' ',
        21 => '▌',
        42 => '▐',
        63 => '█',
        bits => {
            let skipped = u32::from(bits > 21) + u32::from(bits > 42);
            char::from_u32(0x1FB00 + u32::from(bits) - 1 - skipped)
                .expect("sextant block is contiguous")
        }
    }
}

/// Build the donut's slices from the display-ordered rows of the viewed
/// directory. `rows` is `(disk, identity_rank)` per row (the same ranks
/// as the table — [`super::theme::assign_identity`]); `total` is the
/// directory's own subtree total. Returns `(fractions, ranks)` in slice
/// order: identity-ranked rows at or above [`MIN_SLICE_FRACTION`] keep
/// their rank; everything else — small slices, unranked rows, and the
/// part of `total` not covered by any child (the directory's own size) —
/// merges into a final `None` (gray) rest slice.
pub fn build_slices(rows: &[(u64, Option<usize>)], total: u64) -> (Vec<f64>, Vec<Option<usize>>) {
    let (fracs, ranks, _) = build_slices_indexed(rows, total);
    (fracs, ranks)
}

/// Same slice merging as [`build_slices`], but returns each kept slice's
/// row position in `rows` instead of its color rank — `rows` is expected
/// to be in the same display order the table/cursor use (position `i` =
/// cursor position `i`), so this is directly usable to hit-test a donut
/// click back to a table row. `None` for the merged "rest" slice, which
/// does not correspond to any single row.
pub fn build_slice_targets(rows: &[(u64, Option<usize>)], total: u64) -> Vec<Option<usize>> {
    let (_, _, targets) = build_slices_indexed(rows, total);
    targets
}

/// Shared implementation of [`build_slices`] and [`build_slice_targets`]:
/// the same merge pass, carrying both the color rank and the originating
/// row position for each kept slice.
fn build_slices_indexed(
    rows: &[(u64, Option<usize>)],
    total: u64,
) -> (Vec<f64>, Vec<Option<usize>>, Vec<Option<usize>>) {
    if total == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let mut fracs = Vec::new();
    let mut ranks = Vec::new();
    let mut targets = Vec::new();
    let mut accounted = 0.0;
    let mut rest = 0.0;
    for (i, &(disk, rank)) in rows.iter().enumerate() {
        let frac = disk as f64 / total as f64;
        accounted += frac;
        match rank {
            Some(rank) if frac >= MIN_SLICE_FRACTION => {
                fracs.push(frac);
                ranks.push(Some(rank));
                targets.push(Some(i));
            }
            _ => rest += frac,
        }
    }
    // Children can sum below the total (the directory's own blocks) but
    // also above it transiently mid-scan; never emit a negative rest.
    rest += (1.0 - accounted).max(0.0);
    if rest > 1e-9 {
        fracs.push(rest);
        ranks.push(None);
        targets.push(None);
    }
    (fracs, ranks, targets)
}

/// A proportion bar of `width` cells for `frac` in `[0, 1]`, padded with
/// spaces. Unicode uses eighth-blocks for sub-cell precision; ASCII uses
/// `#`. A non-zero fraction always shows at least a sliver.
pub fn proportion_bar(frac: f64, width: usize, ascii: bool) -> String {
    const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let frac = frac.clamp(0.0, 1.0);
    let mut bar = String::with_capacity(width * 3);
    if ascii {
        let mut filled = (frac * width as f64).round() as usize;
        if frac > 0.0 && filled == 0 {
            filled = 1;
        }
        bar.extend(std::iter::repeat_n('#', filled));
        bar.extend(std::iter::repeat_n(' ', width - filled));
        return bar;
    }
    let mut eighths = (frac * (width * 8) as f64).round() as usize;
    if frac > 0.0 && eighths == 0 {
        eighths = 1;
    }
    bar.extend(std::iter::repeat_n('█', eighths / 8));
    if !eighths.is_multiple_of(8) {
        bar.push(EIGHTHS[eighths % 8 - 1]);
    }
    while bar.chars().count() < width {
        bar.push(' ');
    }
    bar
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3-slice donut (½, ¼, ¼) on a 20×20 square-pixel grid: slice 0
    /// covers 12→6 o'clock clockwise, slice 1 covers 6→9, slice 2
    /// covers 9→12.
    fn three_slice_grid() -> PixelGrid {
        rasterize(&[0.5, 0.25, 0.25], 20, 20, 1.0)
    }

    #[test]
    fn rasterize_places_slices_by_angle() {
        let grid = three_slice_grid();
        // Just under 12 o'clock, slightly right of center: slice 0 starts.
        assert_eq!(grid.get(10, 1), Some(0), "top → first slice");
        // 3 o'clock: still slice 0 (it spans half the turn).
        assert_eq!(grid.get(15, 10), Some(0), "right → first slice");
        // ~7-8 o'clock: slice 1.
        assert_eq!(grid.get(7, 15), Some(1), "lower left → second slice");
        // ~10-11 o'clock: slice 2.
        assert_eq!(grid.get(5, 7), Some(2), "upper left → third slice");
    }

    #[test]
    fn rasterize_leaves_hole_and_outside_empty() {
        let grid = three_slice_grid();
        assert_eq!(grid.get(10, 10), None, "hole (r < 42% of outer)");
        assert_eq!(grid.get(0, 0), None, "corner outside the disc");
        assert_eq!(grid.get(19, 19), None, "corner outside the disc");
        // Out-of-bounds coordinates never panic.
        assert_eq!(grid.get(99, 3), None);
    }

    #[test]
    fn rasterize_compensates_pixel_aspect() {
        // Sextant subpixels are 0.75:1 — on a 40×24 grid the physical
        // extent is 30×24, so the disc's vertical diameter spans the full
        // height while narrow pixels widen the horizontal pixel count.
        let grid = rasterize(&[1.0], 40, 24, SEXTANT_ASPECT);
        assert_eq!(grid.get(20, 1), Some(0), "top edge inside");
        // Horizontal extreme: physical x from 15-12=3 → pixel 3/0.75 = 4.
        assert_eq!(grid.get(5, 12), Some(0), "left edge inside");
        assert_eq!(grid.get(1, 12), None, "beyond the horizontal radius");
    }

    #[test]
    fn rasterize_degenerate_inputs() {
        let empty = rasterize(&[], 8, 8, 1.0);
        assert_eq!(empty.get(4, 1), None, "no slices → empty grid");
        let zeros = rasterize(&[0.0, 0.0], 8, 8, 1.0);
        assert_eq!(zeros.get(4, 1), None, "zero total → empty grid");
        let tiny = rasterize(&[1.0], 0, 0, 1.0);
        assert_eq!(tiny.get(0, 0), None, "zero size → no panic");
        // Fractions are normalized: [2, 2] behaves like [0.5, 0.5].
        let unnorm = rasterize(&[2.0, 2.0], 20, 20, 1.0);
        assert_eq!(unnorm.get(15, 10), Some(0), "right half → slice 0");
        assert_eq!(unnorm.get(4, 10), Some(1), "left half → slice 1");
    }

    #[test]
    fn half_block_composition() {
        let grid = three_slice_grid();
        let cells = compose_half_blocks(&grid);
        assert_eq!(cells.len(), 10, "20 subpixel rows → 10 cell rows");
        assert_eq!(cells[0].len(), 20);
        // Center cell row 5 covers subpixel rows 10-11: the hole.
        assert_eq!(cells[5][10], EMPTY_CELL);
        // Right of center, interior: both subpixels in slice 0.
        assert_eq!(
            cells[5][15],
            WheelCell {
                ch: '▀',
                fg: Some(0),
                bg: Some(0)
            }
        );
        // Corner stays empty.
        assert_eq!(cells[0][0], EMPTY_CELL);
    }

    #[test]
    fn half_block_edges_keep_default_background() {
        // A 1-column, 2-row grid with only the top subpixel lit.
        let grid = PixelGrid {
            width: 1,
            height: 2,
            px: vec![Some(3), None],
        };
        let cells = compose_half_blocks(&grid);
        assert_eq!(
            cells[0][0],
            WheelCell {
                ch: '▀',
                fg: Some(3),
                bg: None
            }
        );
        // Bottom-only: ▄ with default background.
        let grid = PixelGrid {
            width: 1,
            height: 2,
            px: vec![None, Some(7)],
        };
        assert_eq!(
            compose_half_blocks(&grid)[0][0],
            WheelCell {
                ch: '▄',
                fg: Some(7),
                bg: None
            }
        );
    }

    #[test]
    fn sextant_chars_map_the_legacy_block() {
        assert_eq!(sextant_char(0), ' ');
        assert_eq!(sextant_char(0b000001), '\u{1FB00}', "upper left only");
        assert_eq!(sextant_char(0b000010), '\u{1FB01}', "upper right only");
        assert_eq!(sextant_char(0b010100), '\u{1FB13}', "b=20, last before gap");
        assert_eq!(
            sextant_char(0b010101),
            '▌',
            "left half exists pre-Unicode 13"
        );
        assert_eq!(sextant_char(0b010110), '\u{1FB14}', "b=22, after first gap");
        assert_eq!(sextant_char(0b101010), '▐', "right half");
        assert_eq!(
            sextant_char(0b101011),
            '\u{1FB28}',
            "b=43, after second gap"
        );
        assert_eq!(sextant_char(0b111110), '\u{1FB3B}', "b=62, block end");
        assert_eq!(sextant_char(0b111111), '█', "full block");
    }

    #[test]
    fn sextant_composition_full_and_boundary_cells() {
        // 2×3 grid = exactly one cell, all lit, single color: full block.
        let grid = PixelGrid {
            width: 2,
            height: 3,
            px: vec![Some(1); 6],
        };
        assert_eq!(
            compose_sextants(&grid)[0][0],
            WheelCell {
                ch: '█',
                fg: Some(1),
                bg: None
            }
        );
        // Interior boundary: left column slice 0, right column slice 1 —
        // ties broken toward the smaller slice index as fg.
        let grid = PixelGrid {
            width: 2,
            height: 3,
            px: vec![Some(0), Some(1), Some(0), Some(1), Some(0), Some(1)],
        };
        assert_eq!(
            compose_sextants(&grid)[0][0],
            WheelCell {
                ch: '▌',
                fg: Some(0),
                bg: Some(1)
            }
        );
        // Edge cell: some subpixels empty → all lit join the fg pattern.
        let grid = PixelGrid {
            width: 2,
            height: 3,
            px: vec![Some(4), None, Some(4), None, Some(4), None],
        };
        assert_eq!(
            compose_sextants(&grid)[0][0],
            WheelCell {
                ch: '▌',
                fg: Some(4),
                bg: None
            }
        );
    }

    #[test]
    fn build_slices_merges_small_and_unranked_into_rest() {
        // total 1000: children 500 (rank 0), 300 (rank 1), 15 (rank 2,
        // under 2%), 100 (unranked); 85 unaccounted (the dir itself).
        let (fracs, ranks) = build_slices(
            &[(500, Some(0)), (300, Some(1)), (15, Some(2)), (100, None)],
            1000,
        );
        assert_eq!(ranks, vec![Some(0), Some(1), None]);
        assert!((fracs[0] - 0.5).abs() < 1e-9);
        assert!((fracs[1] - 0.3).abs() < 1e-9);
        // Rest = 0.015 + 0.1 + 0.085 unaccounted.
        assert!((fracs[2] - 0.2).abs() < 1e-9);
        assert!((fracs.iter().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn build_slice_targets_maps_kept_slices_back_to_rows() {
        // Same fixture as build_slices_merges_small_and_unranked_into_rest:
        // rows 0 and 1 survive as their own slices, rows 2 and 3 (plus the
        // unaccounted remainder) fold into the trailing rest slice.
        let targets = build_slice_targets(
            &[(500, Some(0)), (300, Some(1)), (15, Some(2)), (100, None)],
            1000,
        );
        assert_eq!(targets, vec![Some(0), Some(1), None]);
    }

    #[test]
    fn build_slice_targets_edge_cases() {
        assert_eq!(build_slice_targets(&[], 0), Vec::<Option<usize>>::new());
        // No children: the whole donut is the rest slice — not navigable.
        assert_eq!(build_slice_targets(&[], 100), vec![None]);
    }

    #[test]
    fn build_slices_edge_cases() {
        assert_eq!(build_slices(&[], 0), (Vec::new(), Vec::new()));
        // No children: the whole donut is the rest slice.
        let (fracs, ranks) = build_slices(&[], 100);
        assert_eq!(ranks, vec![None]);
        assert!((fracs[0] - 1.0).abs() < 1e-9);
        // Mid-scan transient: children exceed the published total — the
        // rest never goes negative.
        let (fracs, _) = build_slices(&[(150, Some(0))], 100);
        assert_eq!(fracs.len(), 1, "no rest slice");
        assert!(fracs[0] > 1.0);
    }

    #[test]
    fn proportion_bars() {
        assert_eq!(proportion_bar(0.5, 4, false), "██  ");
        assert_eq!(proportion_bar(0.5, 4, true), "##  ");
        assert_eq!(proportion_bar(0.0, 3, false), "   ");
        assert_eq!(proportion_bar(1.0, 3, false), "███");
        assert_eq!(proportion_bar(1.0, 3, true), "###");
        // Sub-cell precision via eighth blocks.
        assert_eq!(proportion_bar(0.6875, 2, false), "█▍");
        // A non-zero fraction never renders empty.
        assert_eq!(proportion_bar(0.001, 4, false), "▏   ");
        assert_eq!(proportion_bar(0.001, 4, true), "#   ");
        // Out-of-range input clamps instead of panicking.
        assert_eq!(proportion_bar(7.0, 2, true), "##");
        assert_eq!(proportion_bar(-1.0, 2, true), "  ");
    }
}
