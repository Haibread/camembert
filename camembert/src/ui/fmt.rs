//! Pure text/number formatting for the cockpit: humanized ages, path
//! abbreviation, and the disk-gauge arithmetic. No terminal types.

use std::time::Duration;

/// Disk space of the filesystem holding the scan root (statvfs), in
/// bytes. Captured once at UI startup. `compressed` records whether the
/// scan root sits on a transparently-compressed mount (btrfs `compress=`,
/// detected once via mountinfo) — it changes what a coverage comparison
/// *means* (see [`DiskSpace::coverage`]).
#[derive(Debug, Clone, Copy)]
pub struct DiskSpace {
    pub capacity: u64,
    pub used: u64,
    pub compressed: bool,
}

/// How much of the filesystem's occupied space a scan accounts for.
///
/// Coverage compares two quantities that are **not the same unit** on a
/// compressed filesystem: the scan's *logical* footprint (Σ `st_blocks`,
/// what `du` and the size columns report) against statvfs's *physical*
/// `used` (what the disk actually holds, post-compression). When they
/// coincide (no compression), the ratio is a real percentage; when they
/// don't, the scan's logical bytes can — and routinely do — exceed
/// physical `used`, so a clamped "100%" would be a lie about what the
/// scan covers. This enum keeps the two cases distinct instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Coverage {
    /// No occupied space to compare against (empty / degenerate mount).
    Unknown,
    /// The scan accounts for this fraction, in `[0, 1]`, of physical
    /// `used`. Only produced when the logical footprint does not exceed
    /// physical `used`, so it never needs clamping.
    Fraction(f64),
    /// The scan's logical footprint exceeds physical `used`. Expected and
    /// routine on a `compressed` mount (logical > on-disk); off one it is
    /// a transient (mid-scan hardlink attribution, or the filesystem
    /// shrinking underneath the scan). The flag lets the caller word the
    /// two honestly rather than collapsing both into a bogus percentage.
    Exceeds { compressed: bool },
}

impl DiskSpace {
    /// Fraction of capacity occupied, in `[0, 1]`. Both terms are
    /// statvfs physical bytes, so this is always a truthful percentage.
    pub fn used_fraction(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        (self.used as f64 / self.capacity as f64).clamp(0.0, 1.0)
    }

    /// Classify how much of physical `used` this scan's logical footprint
    /// accounts for. See [`Coverage`] for why "logical exceeds physical"
    /// is a first-class answer rather than a clamped 100%.
    pub fn coverage(&self, scan_disk_bytes: u64) -> Coverage {
        if self.used == 0 {
            return Coverage::Unknown;
        }
        if scan_disk_bytes > self.used {
            return Coverage::Exceeds {
                compressed: self.compressed,
            };
        }
        Coverage::Fraction(scan_disk_bytes as f64 / self.used as f64)
    }

    /// Fill fraction, in `[0, 1]`, for the gauge's "covered" segment. An
    /// `Exceeds` scan fills the whole occupied region (it accounts for at
    /// least all of physical `used`); `Unknown` fills nothing.
    pub fn coverage_bar_fraction(&self, scan_disk_bytes: u64) -> f64 {
        match self.coverage(scan_disk_bytes) {
            Coverage::Unknown => 0.0,
            Coverage::Fraction(f) => f,
            Coverage::Exceeds { .. } => 1.0,
        }
    }

    /// The gauge's coverage caption. A scan spanning more than one
    /// filesystem (`device_count > 1` — crossing mounts is the CLI default,
    /// `--one-filesystem` opts out) makes any percentage against this one
    /// statvfs dishonest: the scan total then includes bytes from *other*
    /// filesystems (RAM-backed `tmpfs` included), not just this one. That
    /// framing takes priority over [`Coverage`] — including over
    /// `Exceeds { compressed: true }` — because spanning filesystems is the
    /// actual cause even when the root happens to also be compressed.
    /// `device_count` is `1` for an ordinary same-filesystem scan (and for
    /// a mid-scan gauge, before there is a [`camembert_core::scan::ScanOutcome`]
    /// to count devices over).
    pub fn coverage_caption(&self, scan_disk_bytes: u64, device_count: usize) -> String {
        if device_count > 1 {
            return format!("spans {device_count} filesystems · gauge shows the scan root's");
        }
        match self.coverage(scan_disk_bytes) {
            Coverage::Fraction(f) => format!("this scan covers {:.0}% of used", f * 100.0),
            Coverage::Exceeds { compressed: true } => {
                "scan logical exceeds on-disk (compressed mount)".to_string()
            }
            Coverage::Exceeds { compressed: false } => {
                "scan exceeds used (changed mid-scan)".to_string()
            }
            Coverage::Unknown => "coverage unavailable".to_string(),
        }
    }
}

/// "modified X ago" for the selection card: coarse, human units. Future
/// mtimes (clock skew, broken archives) are called out, not negated.
pub fn humanize_age(now_secs: i64, mtime_secs: i64) -> String {
    let delta = now_secs - mtime_secs;
    if delta < 0 {
        return "in the future".to_owned();
    }
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    match delta {
        0..5 => "just now".to_owned(),
        5..MINUTE => format!("{delta}s ago"),
        MINUTE..HOUR => format!("{} min ago", delta / MINUTE),
        HOUR..DAY => format!("{} h ago", delta / HOUR),
        DAY..MONTH => format!("{} days ago", delta / DAY),
        MONTH..YEAR => format!("{} months ago", delta / MONTH),
        _ => format!("{} years ago", delta / YEAR),
    }
}

/// Humanize a scan's elapsed time for the "scan finished" toast: seconds
/// with one decimal below a minute, `Xm YYs` at or above it (a scan long
/// enough to matter is long enough that decimals stop being useful).
pub fn humanize_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = elapsed.as_secs();
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

/// Abbreviate a path to at most `max` characters by replacing middle
/// components with `…`, always keeping the last component (itself
/// truncated with a leading `…` when alone too long).
pub fn abbreviate_path(path: &str, max: usize) -> String {
    abbreviate_path_on(path, max, cfg!(windows))
}

/// Whether `ch` ends a path component.
///
/// Windows accepts both separators and its own APIs hand back `\`, so both
/// must count there. They cannot simply count everywhere: `\` is a
/// perfectly legal *character in a filename* on Unix, and treating it as a
/// separator would chop such names into fake components.
fn is_sep(ch: char, windows: bool) -> bool {
    ch == '/' || (windows && ch == '\\')
}

fn abbreviate_path_on(path: &str, max: usize, windows: bool) -> String {
    if path.chars().count() <= max {
        return path.to_owned();
    }
    // The separator put back is the one the path itself used, not the
    // platform's: a Unix-style path displayed on Windows (a dump written
    // elsewhere, a path typed with forward slashes) must not have a
    // backslash spliced into it.
    let (last, sep) = match path
        .char_indices()
        .rev()
        .find(|&(_, ch)| is_sep(ch, windows))
    {
        Some((i, ch)) => (&path[i + ch.len_utf8()..], ch),
        None => (path, '/'),
    };
    // "…/<last>" when it fits, else "…<tail of last>".
    let with_prefix_len = last.chars().count() + 2;
    if with_prefix_len <= max {
        // Keep as much of the leading path as fits before "…/last".
        let budget = max - with_prefix_len;
        let prefix: String = path.chars().take(budget).collect();
        return format!("{prefix}…{sep}{last}");
    }
    let tail: String = last
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// Char-index spans (start inclusive, end exclusive) of `path`'s non-empty
/// separator-delimited components, in order — the breadcrumb's clickable
/// column ranges once offset by where the path starts on screen. Byte
/// offsets would misalign multi-byte UTF-8 against terminal columns, so
/// this counts chars; leading/doubled separators contribute no component
/// (never a zero-width clickable span).
///
/// The separator set is platform-dependent (see [`is_sep`]). Splitting a
/// Windows path on `/` alone made `C:\Program Files\Common Files` a single
/// component, and the sole component is always the current directory's own
/// — which is deliberately *not* clickable — so the breadcrumb had no live
/// segment at all on Windows, whatever the depth.
pub fn path_segments(path: &str) -> Vec<(usize, usize)> {
    path_segments_on(path, cfg!(windows))
}

fn path_segments_on(path: &str, windows: bool) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start: Option<usize> = None;
    let mut chars = 0usize;
    for (i, ch) in path.chars().enumerate() {
        chars = i + 1;
        if is_sep(ch, windows) {
            if let Some(s) = start.take() {
                segments.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        segments.push((s, chars));
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanized_ages() {
        assert_eq!(humanize_age(1000, 998), "just now");
        assert_eq!(humanize_age(1000, 990), "10s ago");
        assert_eq!(humanize_age(1000, 1000 - 3 * 60), "3 min ago");
        assert_eq!(humanize_age(1_000_000, 1_000_000 - 5 * 3600), "5 h ago");
        assert_eq!(
            humanize_age(10_000_000, 10_000_000 - 3 * 86400),
            "3 days ago"
        );
        assert_eq!(
            humanize_age(100_000_000, 100_000_000 - 70 * 86400),
            "2 months ago"
        );
        assert_eq!(
            humanize_age(1_000_000_000, 1_000_000_000 - 800 * 86400),
            "2 years ago"
        );
        assert_eq!(humanize_age(1000, 2000), "in the future");
    }

    #[test]
    fn disk_gauge_fractions() {
        let disk = DiskSpace {
            capacity: 1000,
            used: 400,
            compressed: false,
        };
        assert!((disk.used_fraction() - 0.4).abs() < 1e-9);
        assert_eq!(disk.coverage(100), Coverage::Fraction(0.25));
        assert!((disk.coverage_bar_fraction(100) - 0.25).abs() < 1e-9);
        // Degenerate filesystems never divide by zero.
        let empty = DiskSpace {
            capacity: 0,
            used: 0,
            compressed: false,
        };
        assert_eq!(empty.used_fraction(), 0.0);
        assert_eq!(empty.coverage(5), Coverage::Unknown);
        assert_eq!(empty.coverage_bar_fraction(5), 0.0);
    }

    /// A scan spanning more than one filesystem (crossing mounts is the CLI
    /// default; `--one-filesystem` opts out) makes a percentage against one
    /// statvfs dishonest, so `device_count > 1` wins over every `Coverage`
    /// case — including `Exceeds { compressed: true }`, since spanning
    /// filesystems is the actual cause even when the root also happens to
    /// be compressed.
    #[test]
    fn coverage_caption_multi_fs_wins_over_a_percentage() {
        let disk = DiskSpace {
            capacity: 1000,
            used: 400,
            compressed: false,
        };
        assert_eq!(
            disk.coverage_caption(100, 3),
            "spans 3 filesystems · gauge shows the scan root's"
        );
        // Single device: back to the ordinary percentage.
        assert_eq!(
            disk.coverage_caption(100, 1),
            "this scan covers 25% of used"
        );
    }

    #[test]
    fn coverage_caption_multi_fs_wins_over_compressed_exceeds() {
        let compressed = DiskSpace {
            capacity: 1000,
            used: 400,
            compressed: true,
        };
        // Logical (500) > physical used (400): on its own this would be
        // "scan logical exceeds on-disk (compressed mount)" — but spanning
        // filesystems is the actual cause here, so that wording wins.
        assert_eq!(
            compressed.coverage_caption(500, 2),
            "spans 2 filesystems · gauge shows the scan root's"
        );
        assert_eq!(
            compressed.coverage_caption(500, 1),
            "scan logical exceeds on-disk (compressed mount)"
        );
    }

    #[test]
    fn coverage_logical_exceeds_physical_is_not_clamped() {
        // Uncompressed: a scan larger than `used` is a transient (hardlink
        // attribution mid-scan, a shrinking filesystem) — reported as
        // such, never as a fabricated 100%.
        let plain = DiskSpace {
            capacity: 1000,
            used: 400,
            compressed: false,
        };
        assert_eq!(
            plain.coverage(9999),
            Coverage::Exceeds { compressed: false }
        );
        // Compressed: logical > physical is the *expected* steady state,
        // and the flag records it so the gauge can say why.
        let zstd = DiskSpace {
            capacity: 1000,
            used: 400,
            compressed: true,
        };
        assert_eq!(zstd.coverage(9999), Coverage::Exceeds { compressed: true });
        // Both fill the whole occupied segment of the bar.
        assert!((plain.coverage_bar_fraction(9999) - 1.0).abs() < 1e-9);
        assert!((zstd.coverage_bar_fraction(9999) - 1.0).abs() < 1e-9);
        // Exactly equal is still a truthful 100% fraction, not "exceeds".
        assert_eq!(zstd.coverage(400), Coverage::Fraction(1.0));
    }

    #[test]
    fn humanize_duration_below_a_minute_shows_one_decimal() {
        assert_eq!(humanize_duration(Duration::from_millis(1234)), "1.2s");
        assert_eq!(humanize_duration(Duration::from_secs_f64(59.9)), "59.9s");
        assert_eq!(humanize_duration(Duration::ZERO), "0.0s");
    }

    #[test]
    fn humanize_duration_at_or_above_a_minute_switches_to_minutes_seconds() {
        assert_eq!(humanize_duration(Duration::from_secs(60)), "1m 00s");
        assert_eq!(humanize_duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(humanize_duration(Duration::from_secs(3661)), "61m 01s");
    }

    #[test]
    fn path_segments_absolute() {
        // "/home/user": chars 0='/',1..5="home",5='/',6..10="user".
        assert_eq!(path_segments("/home/user"), vec![(1, 5), (6, 10)]);
    }

    #[test]
    fn path_segments_edge_cases() {
        assert_eq!(path_segments(""), Vec::<(usize, usize)>::new());
        assert_eq!(
            path_segments("/"),
            Vec::<(usize, usize)>::new(),
            "bare root: no component"
        );
        assert_eq!(
            path_segments("//a//b/"),
            vec![(2, 3), (5, 6)],
            "doubled slashes collapse"
        );
        assert_eq!(
            path_segments("relative/path"),
            vec![(0, 8), (9, 13)],
            "no leading slash"
        );
        assert_eq!(path_segments("noslash"), vec![(0, 7)]);
        // Multi-byte chars count as one char each, not by UTF-8 byte width.
        assert_eq!(path_segments("/café/日本"), vec![(1, 5), (6, 8)]);
    }

    /// Windows paths arrive backslash-separated, and splitting them on `/`
    /// alone yielded one component — always the current directory's own,
    /// which is never clickable — so the breadcrumb was inert on Windows at
    /// any depth. The converse must stay true: `\` is a legal filename
    /// character on Unix and must not split anything there.
    #[test]
    fn separators_are_platform_dependent() {
        assert_eq!(
            path_segments_on(r"C:\Program Files\Common Files", true),
            vec![(0, 2), (3, 16), (17, 29)],
            "windows: drive, then each component"
        );
        assert_eq!(
            path_segments_on(r"C:\Program Files\Common Files", false),
            vec![(0, 29)],
            "unix: one component, backslashes are just characters"
        );
        // Windows accepts forward slashes too, and mixed input.
        assert_eq!(path_segments_on("C:/Users/theo", true).len(), 3);
        // A Unix file genuinely named `back\slash` keeps its name whole.
        assert_eq!(
            path_segments_on(r"/home/back\slash", false),
            vec![(1, 5), (6, 16)]
        );

        // Abbreviation finds the last component by the same rule, and puts
        // back the separator the path itself used rather than the
        // platform's — a Unix path shown on Windows keeps its slashes.
        assert_eq!(
            abbreviate_path_on(r"C:\Users\theo\projects\deep\nested\dir", 20, true),
            r"C:\Users\theo\p…\dir"
        );
        assert_eq!(
            abbreviate_path_on("/home/theo/projects/deep/nested/dir", 20, true),
            "/home/theo/proj…/dir",
            "a unix-style path keeps its own separator on windows"
        );
    }

    #[test]
    fn path_abbreviation() {
        assert_eq!(abbreviate_path("/home/x", 20), "/home/x", "fits: unchanged");
        assert_eq!(
            abbreviate_path("/home/theo/projects/deep/nested/dir", 20),
            "/home/theo/proj…/dir"
        );
        // Last component alone too long: keep its tail.
        assert_eq!(
            abbreviate_path("/a/really-long-component-name", 10),
            "…nent-name"
        );
        assert_eq!(abbreviate_path("abc", 0), "…", "degenerate budget");
    }
}
