//! Pure text/number formatting for the cockpit: humanized ages, path
//! abbreviation, and the disk-gauge arithmetic. No terminal types.

use std::time::Duration;

/// Disk space of the filesystem holding the scan root (statvfs), in
/// bytes. Captured once at UI startup.
#[derive(Debug, Clone, Copy)]
pub struct DiskSpace {
    pub capacity: u64,
    pub used: u64,
}

impl DiskSpace {
    /// Fraction of capacity occupied, in `[0, 1]`.
    pub fn used_fraction(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        (self.used as f64 / self.capacity as f64).clamp(0.0, 1.0)
    }

    /// Fraction of the *occupied* space covered by this scan's total,
    /// clamped to `[0, 1]` — a scan can transiently exceed `used`
    /// (hardlinks pre-finalization, concurrent writes), and claiming
    /// more than 100% coverage would be dishonest.
    pub fn coverage_fraction(&self, scan_disk_bytes: u64) -> f64 {
        if self.used == 0 {
            return 0.0;
        }
        (scan_disk_bytes as f64 / self.used as f64).clamp(0.0, 1.0)
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
    if path.chars().count() <= max {
        return path.to_owned();
    }
    let last = path.rsplit('/').next().unwrap_or(path);
    // "…/<last>" when it fits, else "…<tail of last>".
    let with_prefix_len = last.chars().count() + 2;
    if with_prefix_len <= max {
        // Keep as much of the leading path as fits before "…/last".
        let budget = max - with_prefix_len;
        let prefix: String = path.chars().take(budget).collect();
        return format!("{prefix}…/{last}");
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
/// `/`-separated components, in order — the breadcrumb's clickable
/// column ranges once offset by where the path starts on screen. Byte
/// offsets would misalign multi-byte UTF-8 against terminal columns, so
/// this counts chars; leading/doubled slashes contribute no component
/// (never a zero-width clickable span).
pub fn path_segments(path: &str) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start: Option<usize> = None;
    let mut chars = 0usize;
    for (i, ch) in path.chars().enumerate() {
        chars = i + 1;
        if ch == '/' {
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
        };
        assert!((disk.used_fraction() - 0.4).abs() < 1e-9);
        assert!((disk.coverage_fraction(100) - 0.25).abs() < 1e-9);
        // Scan bigger than used: clamped, never > 100%.
        assert!((disk.coverage_fraction(9999) - 1.0).abs() < 1e-9);
        // Degenerate filesystems never divide by zero.
        let empty = DiskSpace {
            capacity: 0,
            used: 0,
        };
        assert_eq!(empty.used_fraction(), 0.0);
        assert_eq!(empty.coverage_fraction(5), 0.0);
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
