//! Palette selection (Tokyo Night, light, high-contrast — design §Themes)
//! with per-capability downmapping, and the identity-color assignment
//! shared by the table and the wheel (design §Color and capabilities).
//!
//! Every palette entry carries its truecolor RGB and a hand-picked
//! ANSI-16 equivalent (algorithmic nearest-color gives poor picks for
//! pastels — coral would land on magenta); the 256-color rung is computed
//! by [`rgb_to_256`]. Mono renders everything as the terminal default.
//!
//! All three palettes reuse the exact same nine ANSI-16 tags for the
//! identity ranks (`Yellow, LightBlue, LightGreen, LightMagenta,
//! LightCyan, Red, Cyan, Blue, Magenta`, in rank order): what a
//! `Color::Yellow` actually renders as is the terminal's own color
//! scheme, not ours, so distinctness at that rung is independent of
//! which theme is active — only the truecolor/256 RGBs (what we *do*
//! control) differ per theme.

use clap::ValueEnum;
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

use super::caps::ColorLevel;

/// `--theme` (env `THEME`; also the config file's `theme` key): which
/// palette to render with.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[value(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    /// Truecolor-first dark palette (the default look).
    #[default]
    TokyoNight,
    /// Tokyo-Night-"day"-style variant: dark text assumptions, tuned for
    /// a light background. Auto-selected by OSC 11 background detection
    /// when nothing else picked a theme.
    Light,
    /// Maximum-contrast palette avoiding mid-greys; usable on either a
    /// dark or a light background.
    HighContrast,
}

/// One palette entry: truecolor RGB + its ANSI-16 downmapping.
#[derive(Debug, Clone, Copy)]
pub struct PaletteEntry {
    pub rgb: (u8, u8, u8),
    pub c16: Color,
}

const fn entry(rgb: (u8, u8, u8), c16: Color) -> PaletteEntry {
    PaletteEntry { rgb, c16 }
}

/// Number of identity ranks every palette defines (design: top children
/// of the viewed directory get an identity color by size rank).
pub const IDENTITY_LEN: usize = 9;

/// A full theme's color set. Design constraints that hold across every
/// palette (design §Color and capabilities): `error` always the same
/// coral family; `accent` stays recognizably amber; `identity[0]` is
/// always `accent` (the biggest child carries the signature accent).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub accent: PaletteEntry,
    pub error: PaletteEntry,
    pub muted: PaletteEntry,
    pub good: PaletteEntry,
    pub info: PaletteEntry,
    pub mauve: PaletteEntry,
    pub selection_bg: PaletteEntry,
    pub identity: [PaletteEntry; IDENTITY_LEN],
}

/// Tokyo Night (default): the original truecolor-first dark palette.
const TOKYO_NIGHT: Palette = Palette {
    accent: entry((0xe0, 0xaf, 0x68), Color::Yellow),
    error: entry((0xf7, 0x76, 0x8e), Color::LightRed),
    muted: entry((0x56, 0x5f, 0x89), Color::DarkGray),
    good: entry((0x9e, 0xce, 0x6a), Color::LightGreen),
    info: entry((0x7d, 0xcf, 0xff), Color::LightCyan),
    mauve: entry((0xbb, 0x9a, 0xf7), Color::LightMagenta),
    selection_bg: entry((0x29, 0x2e, 0x42), Color::Black),
    identity: [
        entry((0xe0, 0xaf, 0x68), Color::Yellow), // amber (accent)
        entry((0x7a, 0xa2, 0xf7), Color::LightBlue), // blue
        entry((0x9e, 0xce, 0x6a), Color::LightGreen), // green (good)
        entry((0xbb, 0x9a, 0xf7), Color::LightMagenta), // mauve
        entry((0x7d, 0xcf, 0xff), Color::LightCyan), // sky (info)
        entry((0xff, 0x9e, 0x64), Color::Red),    // orange
        entry((0x73, 0xda, 0xca), Color::Cyan),   // teal
        entry((0x2a, 0xc3, 0xde), Color::Blue),   // cyan
        entry((0x9d, 0x7c, 0xd8), Color::Magenta), // purple
    ],
};

/// Light: a Tokyo-Night-"day"-style variant — every entry darkened /
/// deepened relative to Tokyo Night's for contrast against a light
/// background, same hue family per role so the identity still reads as
/// "the same theme" across both.
const LIGHT: Palette = Palette {
    accent: entry((0x8f, 0x5e, 0x0d), Color::Yellow), // amber, darkened
    error: entry((0xc4, 0x4d, 0x6c), Color::Red),     // coral family, darkened
    muted: entry((0x6b, 0x72, 0x94), Color::DarkGray),
    good: entry((0x38, 0x7a, 0x38), Color::Green),
    info: entry((0x00, 0x71, 0x97), Color::Cyan),
    mauve: entry((0x84, 0x3d, 0xd1), Color::Magenta),
    selection_bg: entry((0xd6, 0xda, 0xe8), Color::White),
    identity: [
        entry((0x8f, 0x5e, 0x0d), Color::Yellow), // amber (accent)
        entry((0x2e, 0x7d, 0xe9), Color::LightBlue), // blue
        entry((0x38, 0x7a, 0x38), Color::LightGreen), // green (good)
        entry((0x84, 0x3d, 0xd1), Color::LightMagenta), // mauve
        entry((0x00, 0x71, 0x97), Color::LightCyan), // sky (info)
        entry((0xb1, 0x5c, 0x00), Color::Red),    // orange
        entry((0x11, 0x8c, 0x80), Color::Cyan),   // teal
        entry((0x06, 0x5f, 0xb8), Color::Blue),   // cyan
        entry((0x7a, 0x3f, 0xc9), Color::Magenta), // purple
    ],
};

/// High-contrast: vivid, saturated colors instead of desaturated
/// mid-greys (which read poorly against *either* a dark or a light
/// background) — the muted/chrome role in particular trades "grey" for
/// a saturated teal so it stays legible regardless of terminal
/// background. [`Theme::selection_style`] additionally forces the
/// cursor row to reverse-video for this theme at every color level: the
/// highest-contrast possible marker, and background-agnostic by
/// construction.
const HIGH_CONTRAST: Palette = Palette {
    accent: entry((0xff, 0xa5, 0x00), Color::Yellow),
    error: entry((0xff, 0x4d, 0x6d), Color::LightRed), // coral family, vivid
    muted: entry((0x00, 0x9a, 0xb0), Color::Cyan),     // saturated, not grey
    good: entry((0x00, 0xb0, 0x5c), Color::LightGreen),
    info: entry((0x00, 0xc8, 0xff), Color::LightCyan),
    mauve: entry((0xb3, 0x5b, 0xff), Color::LightMagenta),
    selection_bg: entry((0x3a, 0x3a, 0x5a), Color::Black),
    identity: [
        entry((0xff, 0xa5, 0x00), Color::Yellow), // amber (accent)
        entry((0x00, 0x8c, 0xff), Color::LightBlue), // blue
        entry((0x00, 0xb0, 0x5c), Color::LightGreen), // green (good)
        entry((0xb3, 0x5b, 0xff), Color::LightMagenta), // mauve
        entry((0x00, 0xc8, 0xff), Color::LightCyan), // sky (info)
        entry((0xff, 0x6a, 0x00), Color::Red),    // orange
        entry((0x00, 0xc8, 0xa0), Color::Cyan),   // teal
        entry((0x00, 0xa0, 0xc8), Color::Blue),   // cyan
        entry((0x8a, 0x2b, 0xe0), Color::Magenta), // purple
    ],
};

/// The palette a theme name selects.
pub fn palette_for(name: ThemeName) -> &'static Palette {
    match name {
        ThemeName::TokyoNight => &TOKYO_NIGHT,
        ThemeName::Light => &LIGHT,
        ThemeName::HighContrast => &HIGH_CONTRAST,
    }
}

/// One of a [`Palette`]'s non-identity roles, named the way call sites
/// already spell them (`theme::ACCENT`, `theme::ERROR`, ...) — kept as
/// plain constants of this type (rather than inlined at each call site)
/// so `Theme::color` can look the *current* palette's entry up instead
/// of a single hardcoded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Accent,
    Error,
    Muted,
    Good,
    Info,
    Mauve,
}

pub const ACCENT: Slot = Slot::Accent;
pub const ERROR: Slot = Slot::Error;
pub const MUTED: Slot = Slot::Muted;
pub const GOOD: Slot = Slot::Good;
pub const INFO: Slot = Slot::Info;
pub const MAUVE: Slot = Slot::Mauve;

impl Palette {
    fn get(&self, slot: Slot) -> PaletteEntry {
        match slot {
            Slot::Accent => self.accent,
            Slot::Error => self.error,
            Slot::Muted => self.muted,
            Slot::Good => self.good,
            Slot::Info => self.info,
            Slot::Mauve => self.mauve,
        }
    }
}

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

/// The active theme: a palette projected onto the detected color level.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    name: ThemeName,
    palette: &'static Palette,
    level: ColorLevel,
}

impl Theme {
    pub fn new(name: ThemeName, level: ColorLevel) -> Self {
        Self {
            name,
            palette: palette_for(name),
            level,
        }
    }

    fn project(&self, entry: PaletteEntry) -> Color {
        let (r, g, b) = entry.rgb;
        match self.level {
            ColorLevel::Truecolor => Color::Rgb(r, g, b),
            ColorLevel::Ansi256 => Color::Indexed(rgb_to_256(r, g, b)),
            ColorLevel::Ansi16 => entry.c16,
            ColorLevel::Mono => Color::Reset,
        }
    }

    /// Project one of the palette's named roles onto the color level.
    /// Mono yields `Color::Reset` (the terminal default — a styling
    /// no-op).
    pub fn color(&self, slot: Slot) -> Color {
        self.project(self.palette.get(slot))
    }

    /// Identity color for a size rank (0 = largest child).
    pub fn identity(&self, rank: usize) -> Color {
        self.project(self.palette.identity[rank % IDENTITY_LEN])
    }

    /// Cursor-row highlight: the palette's selection background where the
    /// terminal can show it faithfully, REVERSED on ANSI-16/mono (a
    /// downmapped background color would be an invisible
    /// background-on-background) — and always REVERSED for the
    /// high-contrast theme regardless of rung, since reverse-video is
    /// the highest-contrast marker available and does not need to guess
    /// whether the terminal background is dark or light.
    pub fn selection_style(&self) -> Style {
        if self.name == ThemeName::HighContrast {
            return Style::new().add_modifier(Modifier::REVERSED);
        }
        match self.level {
            ColorLevel::Truecolor | ColorLevel::Ansi256 => {
                Style::new().bg(self.project(self.palette.selection_bg))
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
        let theme = Theme::new(ThemeName::TokyoNight, ColorLevel::Ansi16);
        assert_eq!(theme.color(ACCENT), Color::Yellow);
        assert_eq!(theme.color(ERROR), Color::LightRed, "errors stay red");
        assert_eq!(theme.color(MUTED), Color::DarkGray);
    }

    /// All identity colors distinct at 16 colors, for every theme: the
    /// three palettes deliberately reuse the same nine ANSI-16 tags (see
    /// the module doc), so this holds by construction, but pin it down
    /// per theme in case a future palette edit breaks that invariant.
    #[test]
    fn identity_distinct_at_ansi16_for_every_theme() {
        for name in ThemeName::value_variants() {
            let palette = palette_for(*name);
            let mut seen: Vec<Color> = palette.identity.iter().map(|e| e.c16).collect();
            seen.sort_by_key(|c| format!("{c:?}"));
            seen.dedup();
            assert_eq!(
                seen.len(),
                IDENTITY_LEN,
                "duplicate ANSI-16 tag in {name:?}"
            );
        }
    }

    /// Errors stay in the coral family and the accent stays recognizably
    /// amber in every palette (design constraint): coral is a warm
    /// pink-red where blue outweighs green (`#f7768e`'s own channel
    /// order: R > B > G); amber is a warm yellow-orange with
    /// R > G > B and blue clearly the smallest channel.
    #[test]
    fn cross_theme_identity_constraints_hold() {
        for name in ThemeName::value_variants() {
            let palette = palette_for(*name);
            let (r, g, b) = palette.error.rgb;
            assert!(r > b && b > g, "{name:?} error should read as warm coral");
            assert!(
                r > 150,
                "{name:?} error should stay a saturated coral, not muddy"
            );

            let (r, g, b) = palette.accent.rgb;
            assert!(r > g && g > b, "{name:?} accent should read as amber");
            assert_eq!(
                palette.identity[0].rgb, palette.accent.rgb,
                "rank 0 is the accent"
            );
        }
    }

    #[test]
    fn theme_projects_per_level() {
        assert_eq!(
            Theme::new(ThemeName::TokyoNight, ColorLevel::Truecolor).color(ACCENT),
            Color::Rgb(0xe0, 0xaf, 0x68)
        );
        assert_eq!(
            Theme::new(ThemeName::TokyoNight, ColorLevel::Ansi256).color(ACCENT),
            Color::Indexed(179)
        );
        assert_eq!(
            Theme::new(ThemeName::TokyoNight, ColorLevel::Mono).color(ACCENT),
            Color::Reset
        );
    }

    #[test]
    fn high_contrast_always_reverses_the_cursor_row() {
        for level in [
            ColorLevel::Truecolor,
            ColorLevel::Ansi256,
            ColorLevel::Ansi16,
            ColorLevel::Mono,
        ] {
            let theme = Theme::new(ThemeName::HighContrast, level);
            assert_eq!(
                theme.selection_style(),
                Style::new().add_modifier(Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn other_themes_use_background_highlight_on_truecolor() {
        let theme = Theme::new(ThemeName::Light, ColorLevel::Truecolor);
        assert_ne!(
            theme.selection_style(),
            Style::new().add_modifier(Modifier::REVERSED)
        );
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
