//! Tokyo-Night-family palette with per-capability downmapping, and the
//! identity-color assignment shared by the table and the wheel (design
//! §Color and capabilities).
//!
//! Every palette entry carries its truecolor RGB and a hand-picked
//! ANSI-16 equivalent (algorithmic nearest-color gives poor picks for
//! pastels — coral would land on magenta); the 256-color rung is computed
//! by [`rgb_to_256`]. Mono renders everything as the terminal default.

use ratatui::style::{Color, Modifier, Style};

use super::caps::ColorLevel;

/// One palette entry: truecolor RGB + its ANSI-16 downmapping.
#[derive(Debug, Clone, Copy)]
pub struct PaletteEntry {
    pub rgb: (u8, u8, u8),
    pub c16: Color,
}

const fn entry(rgb: (u8, u8, u8), c16: Color) -> PaletteEntry {
    PaletteEntry { rgb, c16 }
}

/// Signature accent (amber `#e0af68`).
pub const ACCENT: PaletteEntry = entry((0xe0, 0xaf, 0x68), Color::Yellow);
/// Errors — always the coral family (`#f7768e`).
pub const ERROR: PaletteEntry = entry((0xf7, 0x76, 0x8e), Color::LightRed);
/// Muted chrome (`#565f89`).
pub const MUTED: PaletteEntry = entry((0x56, 0x5f, 0x89), Color::DarkGray);
/// Green (`#9ece6a`): completion, positive states.
pub const GOOD: PaletteEntry = entry((0x9e, 0xce, 0x6a), Color::LightGreen);
/// Sky (`#7dcfff`): informational highlights.
pub const INFO: PaletteEntry = entry((0x7d, 0xcf, 0xff), Color::LightCyan);
/// Mauve (`#bb9af7`).
pub const MAUVE: PaletteEntry = entry((0xbb, 0x9a, 0xf7), Color::LightMagenta);
/// Background-friendly selection (`#292e42`); ANSI-16 falls back to
/// REVERSED (see [`Theme::selection_style`]).
pub const SELECTION_BG: PaletteEntry = entry((0x29, 0x2e, 0x42), Color::Black);

/// Identity colors, assigned to the top children of the viewed directory
/// by size rank. Amber first: the biggest child carries the signature
/// accent. Coral is deliberately absent — it stays reserved for errors.
pub const IDENTITY: [PaletteEntry; 9] = [
    ACCENT,                                      // amber
    entry((0x7a, 0xa2, 0xf7), Color::LightBlue), // blue
    GOOD,                                        // green
    MAUVE,                                       // mauve
    INFO,                                        // sky
    entry((0xff, 0x9e, 0x64), Color::Red),       // orange
    entry((0x73, 0xda, 0xca), Color::Cyan),      // teal
    entry((0x2a, 0xc3, 0xde), Color::Blue),      // cyan
    entry((0x9d, 0x7c, 0xd8), Color::Magenta),   // purple
];

/// Nearest xterm-256 index for an RGB color: best of the 6×6×6 cube
/// (16..231) and the grayscale ramp (232..255) by squared RGB distance.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    /// The cube's channel levels.
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest_level = |c: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|&(_, &level)| (i32::from(c) - level).abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (nearest_level(r), nearest_level(g), nearest_level(b));
    let cube_rgb = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_index = 16 + 36 * ri + 6 * gi + bi;

    // Grayscale ramp: 232 + i has value 8 + 10*i, i in 0..24.
    let gray_i = ((i32::from(r) + i32::from(g) + i32::from(b)) / 3 - 8)
        .div_euclid(10)
        .clamp(0, 23);
    let best_gray = (0..24)
        .filter(|i| (gray_i - i).abs() <= 1)
        .min_by_key(|&i| dist2((r, g, b), (8 + 10 * i, 8 + 10 * i, 8 + 10 * i)))
        .unwrap_or(gray_i);
    let gray_value = 8 + 10 * best_gray;
    let gray_index = 232 + best_gray;

    if dist2((r, g, b), (gray_value, gray_value, gray_value)) < dist2((r, g, b), cube_rgb) {
        gray_index as u8
    } else {
        cube_index as u8
    }
}

fn dist2(a: (u8, u8, u8), b: (i32, i32, i32)) -> i64 {
    let d = |x: u8, y: i32| i64::from(i32::from(x) - y).pow(2);
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

/// The active theme: the palette projected onto the detected color level.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    level: ColorLevel,
}

impl Theme {
    pub fn new(level: ColorLevel) -> Self {
        Self { level }
    }

    /// Project a palette entry onto the color level. Mono yields
    /// `Color::Reset` (the terminal default — a styling no-op).
    pub fn color(&self, entry: PaletteEntry) -> Color {
        let (r, g, b) = entry.rgb;
        match self.level {
            ColorLevel::Truecolor => Color::Rgb(r, g, b),
            ColorLevel::Ansi256 => Color::Indexed(rgb_to_256(r, g, b)),
            ColorLevel::Ansi16 => entry.c16,
            ColorLevel::Mono => Color::Reset,
        }
    }

    /// Identity color for a size rank (0 = largest child).
    pub fn identity(&self, rank: usize) -> Color {
        self.color(IDENTITY[rank % IDENTITY.len()])
    }

    /// Cursor-row highlight: the palette's selection background where the
    /// terminal can show it faithfully, REVERSED on ANSI-16/mono (a
    /// downmapped `#292e42` would be an invisible black-on-black).
    pub fn selection_style(&self) -> Style {
        match self.level {
            ColorLevel::Truecolor | ColorLevel::Ansi256 => {
                Style::new().bg(self.color(SELECTION_BG))
            }
            ColorLevel::Ansi16 | ColorLevel::Mono => Style::new().add_modifier(Modifier::REVERSED),
        }
    }
}

/// Assign identity ranks to rows given their disk sizes (in snapshot
/// order): the `top_n` largest non-empty rows get `Some(rank)` with rank
/// 0 = largest; everything else `None` (rendered muted). Ties break by
/// snapshot position, so the assignment is stable within a generation
/// and unaffected by the display sort.
pub fn assign_identity(disks: &[u64], top_n: usize) -> Vec<Option<usize>> {
    let mut candidates: Vec<usize> = (0..disks.len()).filter(|&i| disks[i] > 0).collect();
    let by_size_desc = |&a: &usize, &b: &usize| disks[b].cmp(&disks[a]).then(a.cmp(&b));
    if candidates.len() > top_n && top_n > 0 {
        candidates.select_nth_unstable_by(top_n, by_size_desc);
        candidates.truncate(top_n);
    } else {
        candidates.truncate(top_n);
    }
    candidates.sort_unstable_by(by_size_desc);
    let mut ranks = vec![None; disks.len()];
    for (rank, &i) in candidates.iter().enumerate() {
        ranks[i] = Some(rank);
    }
    ranks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_to_256_known_values() {
        assert_eq!(rgb_to_256(0, 0, 0), 16, "black is the cube origin");
        assert_eq!(rgb_to_256(255, 255, 255), 231, "white is the cube top");
        assert_eq!(rgb_to_256(255, 0, 0), 196, "pure red");
        assert_eq!(rgb_to_256(0, 255, 0), 46, "pure green");
        assert_eq!(rgb_to_256(0, 0, 255), 21, "pure blue");
        assert_eq!(rgb_to_256(0x80, 0x80, 0x80), 244, "mid gray → gray ramp");
        assert_eq!(rgb_to_256(8, 8, 8), 232, "darkest ramp gray");
        assert_eq!(rgb_to_256(238, 238, 238), 255, "lightest ramp gray");
        // The signature amber lands in the warm part of the cube.
        assert_eq!(rgb_to_256(0xe0, 0xaf, 0x68), 179);
    }

    #[test]
    fn ansi16_downmap_is_semantic() {
        let theme = Theme::new(ColorLevel::Ansi16);
        assert_eq!(theme.color(ACCENT), Color::Yellow);
        assert_eq!(theme.color(ERROR), Color::LightRed, "errors stay red");
        assert_eq!(theme.color(MUTED), Color::DarkGray);
        // All identity colors distinct at 16 colors.
        let mut seen: Vec<Color> = IDENTITY.iter().map(|e| e.c16).collect();
        seen.sort_by_key(|c| format!("{c:?}"));
        seen.dedup();
        assert_eq!(seen.len(), IDENTITY.len());
    }

    #[test]
    fn theme_projects_per_level() {
        assert_eq!(
            Theme::new(ColorLevel::Truecolor).color(ACCENT),
            Color::Rgb(0xe0, 0xaf, 0x68)
        );
        assert_eq!(
            Theme::new(ColorLevel::Ansi256).color(ACCENT),
            Color::Indexed(179)
        );
        assert_eq!(Theme::new(ColorLevel::Mono).color(ACCENT), Color::Reset);
    }

    #[test]
    fn identity_assignment_ranks_by_size() {
        let ranks = assign_identity(&[10, 500, 0, 300, 42], 3);
        assert_eq!(ranks, vec![None, Some(0), None, Some(1), Some(2)]);
    }

    #[test]
    fn identity_assignment_breaks_ties_by_position() {
        let ranks = assign_identity(&[7, 7, 7], 2);
        assert_eq!(ranks, vec![Some(0), Some(1), None]);
    }

    #[test]
    fn identity_assignment_edge_cases() {
        assert_eq!(assign_identity(&[], 9), Vec::<Option<usize>>::new());
        assert_eq!(
            assign_identity(&[0, 0], 9),
            vec![None, None],
            "empty rows never ranked"
        );
        assert_eq!(assign_identity(&[5], 0), vec![None], "top_n = 0");
        // Fewer rows than slots: all ranked.
        assert_eq!(assign_identity(&[1, 2], 9), vec![Some(1), Some(0)]);
    }
}
