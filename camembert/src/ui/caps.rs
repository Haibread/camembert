//! Terminal capability ladder, detected once at startup (design §Color
//! and capabilities).
//!
//! Two independent ladders, both pure functions of the environment so the
//! whole matrix is unit-testable:
//!
//! **Color** — `truecolor → 256 → ANSI-16 → mono`:
//! - `--color never` (env `COLOR`) forces mono;
//! - `NO_COLOR` set to **any** value (even empty) forces mono under
//!   `--color auto` — the strictest reading of <https://no-color.org>;
//! - `COLORTERM` = `truecolor`/`24bit` (case-insensitive) → truecolor;
//! - `TERM` containing `256color` → 256;
//! - `TERM` unset or `dumb` → mono; anything else → ANSI-16;
//! - `--color always` skips `NO_COLOR` and never yields mono (floor:
//!   ANSI-16) — it cannot conjure truecolor out of a 16-color terminal.
//!
//! **Glyphs** — `sextants → half-blocks → ASCII`:
//! - sextants (Unicode 13 "Symbols for Legacy Computing", 2×3 subpixels
//!   per cell) only on truecolor terminals from the known-modern set
//!   (kitty, WezTerm, foot, alacritty, ghostty via `TERM`/`TERM_PROGRAM`)
//!   — font coverage for U+1FB00.. is unreliable elsewhere;
//! - half-blocks (`▀▄`, 1×2 subpixels) everywhere else with color;
//! - ASCII (`#` bars, no wheel) when mono or `TERM=dumb`: a donut whose
//!   slices cannot be told apart by color is decoration, not data.

use clap::ValueEnum;

/// `--color` (env `COLOR`): when to emit color at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Detect from the environment (`COLORTERM`, `TERM`, `NO_COLOR`).
    Auto,
    /// Ignore `NO_COLOR`; still capped by what the terminal advertises.
    Always,
    /// No color at all (implies ASCII bars, no wheel).
    Never,
}

/// Rung of the color ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorLevel {
    Mono,
    Ansi16,
    Ansi256,
    Truecolor,
}

/// Rung of the glyph ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphLevel {
    /// `#` bars only — the wheel is not drawn.
    Ascii,
    /// `▀▄` half-block wheel: 1×2 subpixels per cell.
    HalfBlock,
    /// U+1FB00.. sextant wheel: 2×3 subpixels per cell.
    Sextant,
}

/// The environment variables the detection reads, captured as plain data
/// so detection stays a pure, testable function.
#[derive(Debug, Clone, Default)]
pub struct TermEnv {
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub term_program: Option<String>,
    /// Present means set — the value is irrelevant (any value = no color).
    pub no_color: Option<String>,
}

impl TermEnv {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self {
            term: var("TERM"),
            colorterm: var("COLORTERM"),
            term_program: var("TERM_PROGRAM"),
            no_color: var("NO_COLOR"),
        }
    }
}

/// Detected capabilities, threaded through all rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub color: ColorLevel,
    pub glyphs: GlyphLevel,
}

/// Terminals known to ship fonts covering the sextant block (and to be
/// truecolor). Matched as substrings of `TERM` / `TERM_PROGRAM`,
/// case-insensitively.
const MODERN_TERMINALS: [&str; 5] = ["kitty", "wezterm", "foot", "alacritty", "ghostty"];

impl Caps {
    pub fn detect(env: &TermEnv, mode: ColorMode) -> Self {
        let color = detect_color(env, mode);
        let glyphs = detect_glyphs(env, color);
        Self { color, glyphs }
    }
}

fn detect_color(env: &TermEnv, mode: ColorMode) -> ColorLevel {
    if mode == ColorMode::Never {
        return ColorLevel::Mono;
    }
    if mode == ColorMode::Auto && env.no_color.is_some() {
        return ColorLevel::Mono;
    }
    let advertised = advertised_color(env);
    if mode == ColorMode::Always {
        return advertised.max(ColorLevel::Ansi16);
    }
    advertised
}

/// The ladder as the terminal advertises it, before any override.
fn advertised_color(env: &TermEnv) -> ColorLevel {
    if let Some(colorterm) = &env.colorterm {
        let ct = colorterm.to_ascii_lowercase();
        if ct == "truecolor" || ct == "24bit" {
            return ColorLevel::Truecolor;
        }
    }
    match &env.term {
        Some(term) if term.contains("256color") => ColorLevel::Ansi256,
        Some(term) if term == "dumb" => ColorLevel::Mono,
        Some(_) => ColorLevel::Ansi16,
        None => ColorLevel::Mono,
    }
}

fn detect_glyphs(env: &TermEnv, color: ColorLevel) -> GlyphLevel {
    if color == ColorLevel::Mono {
        return GlyphLevel::Ascii;
    }
    if matches!(&env.term, Some(term) if term == "dumb") {
        return GlyphLevel::Ascii;
    }
    if color == ColorLevel::Truecolor && is_modern_terminal(env) {
        return GlyphLevel::Sextant;
    }
    GlyphLevel::HalfBlock
}

fn is_modern_terminal(env: &TermEnv) -> bool {
    [&env.term, &env.term_program]
        .into_iter()
        .flatten()
        .map(|value| value.to_ascii_lowercase())
        .any(|value| MODERN_TERMINALS.iter().any(|known| value.contains(known)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        term: Option<&str>,
        colorterm: Option<&str>,
        term_program: Option<&str>,
        no_color: Option<&str>,
    ) -> TermEnv {
        TermEnv {
            term: term.map(str::to_owned),
            colorterm: colorterm.map(str::to_owned),
            term_program: term_program.map(str::to_owned),
            no_color: no_color.map(str::to_owned),
        }
    }

    #[test]
    fn color_ladder_matrix() {
        use ColorLevel::*;
        use ColorMode::*;
        let cases: [(TermEnv, ColorMode, ColorLevel); 12] = [
            // Truecolor via COLORTERM, both spellings, case-insensitive.
            (
                env(Some("xterm"), Some("truecolor"), None, None),
                Auto,
                Truecolor,
            ),
            (
                env(Some("xterm"), Some("24BIT"), None, None),
                Auto,
                Truecolor,
            ),
            // 256 via TERM.
            (env(Some("xterm-256color"), None, None, None), Auto, Ansi256),
            (
                env(Some("screen-256color"), None, None, None),
                Auto,
                Ansi256,
            ),
            // Plain terminals: ANSI-16.
            (env(Some("xterm"), None, None, None), Auto, Ansi16),
            (env(Some("linux"), None, None, None), Auto, Ansi16),
            // Dumb or absent TERM: mono.
            (env(Some("dumb"), None, None, None), Auto, Mono),
            (env(None, None, None, None), Auto, Mono),
            // NO_COLOR wins in auto — even set to the empty string.
            (
                env(Some("xterm-256color"), Some("truecolor"), None, Some("")),
                Auto,
                Mono,
            ),
            // --color always ignores NO_COLOR but cannot invent truecolor.
            (
                env(Some("xterm-256color"), None, None, Some("1")),
                Always,
                Ansi256,
            ),
            (env(Some("dumb"), None, None, None), Always, Ansi16),
            // --color never beats everything.
            (
                env(Some("xterm"), Some("truecolor"), None, None),
                Never,
                Mono,
            ),
        ];
        for (env, mode, expected) in cases {
            assert_eq!(
                Caps::detect(&env, mode).color,
                expected,
                "env {env:?} mode {mode:?}"
            );
        }
    }

    #[test]
    fn glyph_ladder_matrix() {
        use ColorMode::*;
        use GlyphLevel::*;
        let cases: [(TermEnv, ColorMode, GlyphLevel); 8] = [
            // Modern truecolor terminals get sextants (TERM or TERM_PROGRAM).
            (
                env(Some("xterm-kitty"), Some("truecolor"), None, None),
                Auto,
                Sextant,
            ),
            (
                env(
                    Some("xterm-256color"),
                    Some("truecolor"),
                    Some("WezTerm"),
                    None,
                ),
                Auto,
                Sextant,
            ),
            (
                env(Some("foot"), Some("truecolor"), None, None),
                Auto,
                Sextant,
            ),
            // Truecolor but unknown terminal: half-blocks.
            (
                env(Some("xterm-256color"), Some("truecolor"), None, None),
                Auto,
                HalfBlock,
            ),
            // Modern terminal but no truecolor advertised: half-blocks.
            (env(Some("xterm-kitty"), None, None, None), Auto, HalfBlock),
            // Mono (any cause) drops to ASCII: no color, no wheel.
            (
                env(Some("xterm-kitty"), Some("truecolor"), None, Some("1")),
                Auto,
                Ascii,
            ),
            (
                env(Some("xterm-kitty"), Some("truecolor"), None, None),
                Never,
                Ascii,
            ),
            (env(Some("dumb"), None, None, None), Always, Ascii),
        ];
        for (env, mode, expected) in cases {
            assert_eq!(
                Caps::detect(&env, mode).glyphs,
                expected,
                "env {env:?} mode {mode:?}"
            );
        }
    }
}
