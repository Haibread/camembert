//! `camembert.toml` config file (design slice 6, design §Color and
//! capabilities: "Config file: `camembert.toml` (XDG config dir)"; flat
//! view + pattern aggregation decisions D4,
//! `docs/design/flat-view-decisions.md`).
//!
//! Keys, all optional (absent = "defer to the next precedence step" or the
//! built-in default): `theme`, `color`, `no_motion`, `flat_cap`, and the
//! `[patterns]` table (label = "glob", D4).
//!
//! A missing file is silently fine (this is opt-in configuration, most
//! users will never have one). An unparseable file is never fatal — it is
//! not this tool's job to break someone's scan over a typo in a config
//! file — but it does log a `warn!` so the mistake is discoverable instead
//! of silently ignored forever.
//!
//! # Per-section resilience (D4 / attack finding 3)
//!
//! Parsing is **per-key resilient**, not all-or-nothing: the file is first
//! parsed into a generic [`toml::Table`] (fails only on genuinely broken
//! TOML syntax — the one case that still resets everything, unchanged from
//! before), then each top-level key is deserialized *independently*.
//! A malformed value for one key (a bad `flat_cap`, a `[patterns]` entry
//! whose value isn't a string) is warned about and defaulted **on its
//! own** — it can no longer take `theme`/`color`/`no_motion` down with it,
//! which is exactly the failure the pre-D4 all-or-nothing
//! `#[derive(Deserialize)]` struct had (one bad key reset the whole file).
//! `[patterns]` gets the same per-entry treatment one level down: one bad
//! glob-spec entry is skipped, the rest of the table still loads.
//!
//! `toml`'s `preserve_order` feature is enabled workspace-wide (see the
//! root `Cargo.toml`) so `[patterns]` iterates in file order — D4's
//! same-name-shadowing/precedence rule depends on it (without it, the
//! crate's default `BTreeMap` backing would silently alphabetize entries).

use std::path::{Path, PathBuf};

use camembert_core::flat::{DEFAULT_FLAT_CAP, FlatConfig, PatternSet};
use tracing::{debug, warn};

use crate::ui::caps::ColorMode;
use crate::ui::theme::ThemeName;

/// The config file's contents, already validated and stripped of whatever
/// it does not recognize.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileConfig {
    pub theme: Option<ThemeName>,
    pub color: Option<ColorMode>,
    pub no_motion: Option<bool>,
    /// Flat-view top-N cap (D4), when present and well-typed; `None` means
    /// the built-in [`DEFAULT_FLAT_CAP`] applies (see
    /// [`resolve_flat_cap`]).
    pub flat_cap: Option<usize>,
    /// `[patterns]` entries in file order: `(label, glob spec)` (D4). Only
    /// well-typed (string) entries make it here — a malformed one is
    /// dropped with a reason recorded in [`Self::pattern_warnings`].
    pub patterns: Vec<(String, String)>,
    /// Reasons a `[patterns]` entry (or the whole table) was dropped while
    /// parsing (attack finding 3) — distinct from the glob-*compile*
    /// warnings [`camembert_core::flat::PatternSet::warnings`] raises once
    /// the entries are actually pushed; `main` combines both counts into
    /// one startup toast.
    pub pattern_warnings: Vec<String>,
    /// `[queries]` entries in file order: `(label, query string)` (D6).
    /// Read-only — the palette shows them when its input is empty; there
    /// is no way to write one back from the UI. Same per-entry resilience
    /// as `[patterns]`: only well-typed (string) entries make it here.
    pub queries: Vec<(String, String)>,
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
/// missing file, an unreadable one, or invalid TOML *syntax* all fall
/// back to `FileConfig::default()`; a valid-TOML file with a bad
/// individual key falls back only on that key (see the module docs).
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
    let mut table: toml::Table = match toml::from_str(text) {
        Ok(table) => table,
        Err(err) => {
            warn!(path = %path.display(), %err, "invalid config file: using defaults");
            return FileConfig::default();
        }
    };

    let mut cfg = FileConfig::default();
    take_scalar(&mut table, "theme", path, &mut cfg.theme);
    take_scalar(&mut table, "color", path, &mut cfg.color);
    take_scalar(&mut table, "no_motion", path, &mut cfg.no_motion);
    take_scalar(&mut table, "flat_cap", path, &mut cfg.flat_cap);
    take_patterns(&mut table, path, &mut cfg);
    take_queries(&mut table, path, &mut cfg);

    if !table.is_empty() {
        let keys: Vec<&String> = table.keys().collect();
        warn!(
            path = %path.display(),
            keys = ?keys,
            "config file has unrecognized key(s): ignoring them"
        );
    }
    debug!(
        path = %path.display(),
        theme = ?cfg.theme,
        color = ?cfg.color,
        no_motion = cfg.no_motion,
        flat_cap = cfg.flat_cap,
        patterns = cfg.patterns.len(),
        "config file loaded"
    );
    cfg
}

/// Pull one scalar top-level key out of `table` and try to deserialize it
/// into `slot`, independently of every other key (D4 per-section
/// resilience): a type mismatch warns and leaves `slot` at its default
/// (`None`), without touching anything else already parsed.
fn take_scalar<T>(table: &mut toml::Table, key: &str, path: &Path, slot: &mut Option<T>)
where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = table.remove(key) else {
        return;
    };
    match value.try_into::<T>() {
        Ok(parsed) => *slot = Some(parsed),
        Err(err) => {
            warn!(
                path = %path.display(),
                key,
                %err,
                "invalid config value: ignoring this key, keeping the rest"
            );
        }
    }
}

/// Pull `[patterns]` out of `table` and validate it entry-by-entry (D4):
/// each `label = "glob"` pair that is not a string is dropped with a
/// warning, the rest of the table still loads; a `patterns` key that
/// isn't a table at all drops the whole section the same way. Either way,
/// every *other* config key is untouched (attack finding 3 — the exact
/// failure this function exists to isolate).
fn take_patterns(table: &mut toml::Table, path: &Path, cfg: &mut FileConfig) {
    let Some(value) = table.remove("patterns") else {
        return;
    };
    let toml::Value::Table(entries) = value else {
        let reason = "[patterns] must be a table of label = \"glob\": ignoring".to_owned();
        warn!(path = %path.display(), "{reason}");
        cfg.pattern_warnings.push(reason);
        return;
    };
    for (label, entry) in entries {
        match entry {
            toml::Value::String(spec) => cfg.patterns.push((label, spec)),
            other => {
                let reason = format!("[patterns].{label} is not a string: ignoring");
                warn!(path = %path.display(), label, value = ?other, "{reason}");
                cfg.pattern_warnings.push(reason);
            }
        }
    }
}

/// Pull `[queries]` out of `table` and validate it entry-by-entry (D6,
/// mirroring [`take_patterns`]'s per-entry resilience): each `label =
/// "query string"` pair that is not a string is dropped with a warning,
/// the rest of the table still loads; a `queries` key that isn't a table
/// at all drops the whole section the same way, leaving every other
/// config key untouched.
fn take_queries(table: &mut toml::Table, path: &Path, cfg: &mut FileConfig) {
    let Some(value) = table.remove("queries") else {
        return;
    };
    let toml::Value::Table(entries) = value else {
        warn!(
            path = %path.display(),
            "[queries] must be a table of label = \"query string\": ignoring"
        );
        return;
    };
    for (label, entry) in entries {
        match entry {
            toml::Value::String(query) => cfg.queries.push((label, query)),
            other => {
                warn!(
                    path = %path.display(),
                    label,
                    value = ?other,
                    "[queries].{label} is not a string: ignoring"
                );
            }
        }
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

/// Whether the freeable `/proc` sweep ends up disabled (freeable phase 1,
/// D7): `--no-proc-sweep`/`NO_PROC_SWEEP`, presence semantics like
/// `NO_MOTION` — but, unlike motion/color/theme, deliberately **no**
/// `camembert.toml` key (the decisions doc keeps this flag+env only).
pub fn resolve_no_proc_sweep(cli_flag: bool, env_set: bool) -> bool {
    cli_flag || env_set
}

/// Flat-view top-N cap (D4): the config file's `flat_cap`, or
/// [`DEFAULT_FLAT_CAP`] — config-file only, no CLI flag or env var (the
/// decisions doc doesn't call for one, unlike `no_proc_sweep`).
pub fn resolve_flat_cap(file: Option<usize>) -> usize {
    file.unwrap_or(DEFAULT_FLAT_CAP)
}

/// Build the flat-view [`FlatConfig`] (presets + user `[patterns]` +
/// `flat_cap`, D4) from the loaded file config, and every warning worth
/// surfacing as one combined startup toast: the config-level structural
/// warnings ([`FileConfig::pattern_warnings`], attack finding 3) plus the
/// glob-*compile* warnings [`PatternSet::push`] raises for entries that
/// parsed fine as TOML but aren't valid globs (D4). Presets always load
/// first; user patterns are pushed in file order after them, so a
/// same-label entry shadows its preset in place (D1/D4).
pub fn build_flat_config(file: &FileConfig) -> (FlatConfig, Vec<String>) {
    let mut patterns = PatternSet::presets();
    for (label, spec) in &file.patterns {
        patterns.push(label, spec);
    }
    let mut warnings = file.pattern_warnings.clone();
    warnings.extend(patterns.warnings().iter().cloned());
    (
        FlatConfig {
            patterns,
            cap: resolve_flat_cap(file.flat_cap),
        },
        warnings,
    )
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

    #[test]
    fn resolve_no_proc_sweep_flag_or_env_only_no_config_key() {
        assert!(
            !resolve_no_proc_sweep(false, false),
            "nothing set: sweep stays enabled"
        );
        assert!(resolve_no_proc_sweep(true, false), "--no-proc-sweep alone");
        assert!(resolve_no_proc_sweep(false, true), "NO_PROC_SWEEP alone");
        assert!(
            resolve_no_proc_sweep(true, true),
            "both set: still disabled (OR, not XOR)"
        );
    }

    #[test]
    fn resolve_flat_cap_precedence() {
        assert_eq!(resolve_flat_cap(None), DEFAULT_FLAT_CAP);
        assert_eq!(resolve_flat_cap(Some(50)), 50);
    }

    // ---- config file deserialization ----

    #[test]
    fn parse_valid_full_config() {
        let cfg = parse(
            "theme = \"high-contrast\"\ncolor = \"never\"\nno_motion = true\nflat_cap = 500\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(
            cfg,
            FileConfig {
                theme: Some(ThemeName::HighContrast),
                color: Some(ColorMode::Never),
                no_motion: Some(true),
                flat_cap: Some(500),
                patterns: Vec::new(),
                pattern_warnings: Vec::new(),
                queries: Vec::new(),
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
                ..Default::default()
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
                ..Default::default()
            },
            "unknown keys are dropped, known ones still apply"
        );
    }

    // ---- D4 / attack finding 3: per-section resilience ----

    #[test]
    fn a_bad_flat_cap_does_not_reset_theme_or_patterns() {
        let cfg = parse(
            "theme = \"light\"\nflat_cap = \"many\"\n[patterns]\nlogs = \"*.log\"\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(cfg.theme, Some(ThemeName::Light), "theme survives");
        assert_eq!(cfg.flat_cap, None, "bad flat_cap defaults, alone");
        assert_eq!(cfg.patterns, vec![("logs".to_owned(), "*.log".to_owned())]);
    }

    #[test]
    fn a_broken_patterns_table_does_not_reset_theme_or_flat_cap() {
        // The attack's exact scenario: `presets = false` under [patterns]
        // is a bool in what should be a label -> glob-string map — a type
        // error for that one entry only, not the whole file.
        let cfg = parse(
            "theme = \"light\"\nflat_cap = 250\n[patterns]\npresets = false\nlogs = \"*.log\"\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(cfg.theme, Some(ThemeName::Light));
        assert_eq!(cfg.flat_cap, Some(250));
        assert_eq!(
            cfg.patterns,
            vec![("logs".to_owned(), "*.log".to_owned())],
            "the bad entry is dropped, the good one still loads"
        );
        assert_eq!(cfg.pattern_warnings.len(), 1);
    }

    #[test]
    fn patterns_not_a_table_is_dropped_alone() {
        let cfg = parse(
            "theme = \"light\"\npatterns = \"oops\"\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(cfg.theme, Some(ThemeName::Light));
        assert!(cfg.patterns.is_empty());
        assert_eq!(cfg.pattern_warnings.len(), 1);
    }

    #[test]
    fn patterns_preserve_file_order() {
        let cfg = parse(
            "[patterns]\nzzz = \"*.zzz\"\naaa = \"*.aaa\"\nmmm = \"*.mmm\"\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(
            cfg.patterns,
            vec![
                ("zzz".to_owned(), "*.zzz".to_owned()),
                ("aaa".to_owned(), "*.aaa".to_owned()),
                ("mmm".to_owned(), "*.mmm".to_owned()),
            ],
            "file order preserved, not alphabetized"
        );
    }

    // ---- D6: [queries] resilience ----

    #[test]
    fn queries_parse_in_file_order() {
        let cfg = parse(
            "[queries]\nbig_logs = \"*.log >100M\"\nold = \"older:1y\"\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(
            cfg.queries,
            vec![
                ("big_logs".to_owned(), "*.log >100M".to_owned()),
                ("old".to_owned(), "older:1y".to_owned()),
            ]
        );
    }

    #[test]
    fn a_broken_queries_entry_does_not_reset_theme_or_the_good_entries() {
        let cfg = parse(
            "theme = \"light\"\n[queries]\nbad = 5\ngood = \"*.tmp\"\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(cfg.theme, Some(ThemeName::Light));
        assert_eq!(cfg.queries, vec![("good".to_owned(), "*.tmp".to_owned())]);
    }

    #[test]
    fn queries_not_a_table_is_dropped_alone() {
        let cfg = parse(
            "theme = \"light\"\nqueries = \"oops\"\n",
            Path::new("camembert.toml"),
        );
        assert_eq!(cfg.theme, Some(ThemeName::Light));
        assert!(cfg.queries.is_empty());
    }

    // ---- build_flat_config ----

    #[test]
    fn build_flat_config_shadows_a_preset_in_place_and_combines_warnings() {
        let file = FileConfig {
            patterns: vec![
                ("*.log".to_owned(), "*.LOG".to_owned()), // shadows the preset
                ("bad".to_owned(), "a/b".to_owned()),     // invalid glob (full path)
            ],
            pattern_warnings: vec!["structural warning".to_owned()],
            flat_cap: Some(42),
            ..Default::default()
        };
        let (flat_config, warnings) = build_flat_config(&file);
        assert_eq!(flat_config.cap, 42);
        assert_eq!(flat_config.patterns.len(), 8, "shadow, not append");
        // One structural warning (config-level) + one glob-compile warning.
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn build_flat_config_defaults_cap_when_absent() {
        let (flat_config, warnings) = build_flat_config(&FileConfig::default());
        assert_eq!(flat_config.cap, DEFAULT_FLAT_CAP);
        assert!(warnings.is_empty());
    }
}
