//! `camembert.toml` config file (design slice 6, design §Color and
//! capabilities: "Config file: `camembert.toml` (XDG config dir)").
//!
//! Three optional keys, all `None`/absent by default: `theme`, `color`,
//! `no_motion`. Precedence, for each of the three (design slice 6 point
//! 5): **CLI flag > env var > config file > default** (the OSC 11
//! background probe slots in between the config file and the default,
//! for `theme` only — that step lives in `ui::resolve_theme_name`, since
//! it needs a real terminal).
//!
//! A missing file is silently fine (this is opt-in configuration, most
//! users will never have one). An unparseable file, or one with unknown
//! keys, is never fatal — it is not this tool's job to break someone's
//! scan over a typo in a config file — but it does log a `warn!` so the
//! mistake is discoverable instead of silently ignored forever.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, warn};

use crate::ui::caps::ColorMode;
use crate::ui::theme::ThemeName;

/// The config file's contents, already validated and stripped of
/// whatever it does not recognize.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileConfig {
    pub theme: Option<ThemeName>,
    pub color: Option<ColorMode>,
    pub no_motion: Option<bool>,
}

/// Deserialization target: `#[serde(flatten)]`ing the leftovers into
/// `unknown` is what lets unrecognized keys be *detected* (and warned
/// about) without `deny_unknown_fields` making them a hard parse error
/// — the design explicitly calls for forward-compatible ignoring, not
/// rejection.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    theme: Option<ThemeName>,
    color: Option<ColorMode>,
    no_motion: Option<bool>,
    #[serde(flatten)]
    unknown: std::collections::BTreeMap<String, toml::Value>,
}

/// `$XDG_CONFIG_HOME/camembert/camembert.toml`, falling back to
/// `~/.config/camembert/camembert.toml` when `XDG_CONFIG_HOME` is unset
/// or empty (the XDG base-directory spec's own fallback rule). `None`
/// when neither variable is available — treated exactly like a missing
/// file.
fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("camembert/camembert.toml"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/camembert/camembert.toml"))
}

/// Load the config file from its standard location. Never fails: a
/// missing file, an unreadable one, or invalid TOML all fall back to
/// `FileConfig::default()` (every key `None`, i.e. "defer to the next
/// precedence step").
pub fn load() -> FileConfig {
    let Some(path) = config_path() else {
        debug!("no XDG_CONFIG_HOME or HOME: skipping camembert.toml");
        return FileConfig::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, &path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no config file: using defaults");
            FileConfig::default()
        }
        Err(err) => {
            warn!(path = %path.display(), %err, "cannot read config file: using defaults");
            FileConfig::default()
        }
    }
}

/// Parse and validate config file text already read from `path` (the
/// path is only used for diagnostics). Split out from [`load`] so tests
/// can exercise it without touching the filesystem.
fn parse(text: &str, path: &Path) -> FileConfig {
    let raw: RawConfig = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(err) => {
            warn!(path = %path.display(), %err, "invalid config file: using defaults");
            return FileConfig::default();
        }
    };
    if !raw.unknown.is_empty() {
        let keys: Vec<&String> = raw.unknown.keys().collect();
        warn!(
            path = %path.display(),
            keys = ?keys,
            "config file has unrecognized key(s): ignoring them"
        );
    }
    debug!(
        path = %path.display(),
        theme = ?raw.theme,
        color = ?raw.color,
        no_motion = raw.no_motion,
        "config file loaded"
    );
    FileConfig {
        theme: raw.theme,
        color: raw.color,
        no_motion: raw.no_motion,
    }
}

/// Theme precedence, minus the OSC 11 step (design slice 6 point 5):
/// `--theme`/`THEME` (already merged by clap's own `env` handling into
/// one `Option`) beats the config file's `theme` key; if neither set
/// one, `None` propagates so the caller can still try OSC 11 detection
/// before falling back to the default dark theme.
pub fn resolve_theme(cli_or_env: Option<ThemeName>, file: Option<ThemeName>) -> Option<ThemeName> {
    cli_or_env.or(file)
}

/// Color-mode precedence (design slice 6 point 5): `--color`/`COLOR` >
/// the config file's `color` key > `auto`.
pub fn resolve_color(cli_or_env: Option<ColorMode>, file: Option<ColorMode>) -> ColorMode {
    cli_or_env.or(file).unwrap_or(ColorMode::Auto)
}

/// Whether animation ends up disabled, folding in the config file's
/// `no_motion` key alongside `--no-motion`/`NO_MOTION` (design slice 6
/// point 5, extending the slice-5 rule the same doc comment describes
/// in `main.rs`). `--no-motion` has no way to force motion back *on*
/// (there is no `--motion` flag) — the only direction any of these three
/// sources can push is "disabled" — so precedence collapses to a plain
/// OR: whichever source(s) ask for it disabled, it is disabled, and
/// there is no scenario where a lower-precedence "disabled" gets
/// overridden by a higher-precedence "enabled" because no source can
/// express "enabled" other than the shared default of doing nothing.
pub fn resolve_no_motion(cli_flag: bool, env_set: bool, file: Option<bool>) -> bool {
    cli_flag || env_set || file.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resolve_* precedence (pure functions over Options) ----

    #[test]
    fn resolve_theme_precedence() {
        assert_eq!(
            resolve_theme(Some(ThemeName::Light), Some(ThemeName::HighContrast)),
            Some(ThemeName::Light),
            "cli/env beats the config file"
        );
        assert_eq!(
            resolve_theme(None, Some(ThemeName::HighContrast)),
            Some(ThemeName::HighContrast),
            "config file used when cli/env silent"
        );
        assert_eq!(
            resolve_theme(None, None),
            None,
            "neither set: caller tries OSC 11 next, not a default yet"
        );
    }

    #[test]
    fn resolve_color_precedence() {
        assert_eq!(
            resolve_color(Some(ColorMode::Never), Some(ColorMode::Always)),
            ColorMode::Never,
            "cli/env beats the config file"
        );
        assert_eq!(
            resolve_color(None, Some(ColorMode::Always)),
            ColorMode::Always,
            "config file used when cli/env silent"
        );
        assert_eq!(
            resolve_color(None, None),
            ColorMode::Auto,
            "neither set: falls back to auto"
        );
    }

    #[test]
    fn resolve_no_motion_precedence() {
        assert!(
            !resolve_no_motion(false, false, None),
            "nothing set: motion stays on"
        );
        assert!(resolve_no_motion(true, false, None), "--no-motion alone");
        assert!(resolve_no_motion(false, true, None), "NO_MOTION alone");
        assert!(resolve_no_motion(false, false, Some(true)), "config alone");
        assert!(
            !resolve_no_motion(false, false, Some(false)),
            "config false: no-op"
        );
        assert!(
            resolve_no_motion(true, false, Some(false)),
            "cli disables even if config says false"
        );
    }

    // ---- config file deserialization ----

    #[test]
    fn parse_valid_full_config() {
        let cfg = parse(
            "theme = \"high-contrast\"\ncolor = \"never\"\nno_motion = true\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(
            cfg,
            FileConfig {
                theme: Some(ThemeName::HighContrast),
                color: Some(ColorMode::Never),
                no_motion: Some(true),
            }
        );
    }

    #[test]
    fn parse_partial_config() {
        let cfg = parse("theme = \"light\"\n", Path::new("camembert.toml"));
        assert_eq!(
            cfg,
            FileConfig {
                theme: Some(ThemeName::Light),
                color: None,
                no_motion: None,
            }
        );
    }

    #[test]
    fn parse_empty_config_is_all_defaults() {
        assert_eq!(
            parse("", Path::new("camembert.toml")),
            FileConfig::default()
        );
    }

    #[test]
    fn parse_invalid_toml_falls_back_to_defaults() {
        let cfg = parse("this is not [valid toml", Path::new("camembert.toml"));
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn parse_invalid_value_falls_back_to_defaults() {
        // `theme` is a string enum: a number cannot deserialize into it.
        let cfg = parse("theme = 42\n", Path::new("camembert.toml"));
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn parse_invalid_enum_variant_falls_back_to_defaults() {
        let cfg = parse("theme = \"solarized\"\n", Path::new("camembert.toml"));
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn parse_unknown_keys_are_ignored_not_fatal() {
        let cfg = parse(
            "theme = \"light\"\nfuture_key = \"whatever\"\nanother = 5\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(
            cfg,
            FileConfig {
                theme: Some(ThemeName::Light),
                color: None,
                no_motion: None,
            },
            "unknown keys are dropped, known ones still apply"
        );
    }
}
