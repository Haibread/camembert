//! Size semantics.
//!
//! Every entry carries two sizes that legitimately disagree — sparse files,
//! filesystem compression, and tail slack all drive them apart:
//!
//! - **apparent**: `st_size`, what `ls -l` shows.
//! - **real**: `st_blocks * 512`, what the file actually occupies on disk.
//!
//! Real is the default everywhere (it answers "what can I free"); apparent is
//! always kept alongside so the divergence itself can be surfaced (slack,
//! sparseness, compression ratio).

use std::fmt;

/// The two sizes of an entry, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    /// `st_size`: length of the file as seen by readers.
    pub apparent: u64,
    /// `st_blocks * 512`: bytes actually allocated on disk.
    pub real: u64,
}

impl Size {
    /// Unit of `st_blocks` as defined by POSIX, independent of the
    /// filesystem's block size.
    pub const BLOCK_UNIT: u64 = 512;

    pub fn new(apparent: u64, blocks: u64) -> Self {
        Self {
            apparent,
            real: blocks * Self::BLOCK_UNIT,
        }
    }

    /// Aggregate a child's sizes into this (parent) total.
    pub fn add(&mut self, other: Size) {
        self.apparent += other.apparent;
        self.real += other.real;
    }
}

/// Human-readable byte count in binary units (`KiB`, `MiB`, …), one decimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanSize(pub u64);

impl fmt::Display for HumanSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
        let bytes = self.0;
        if bytes < 1024 {
            return write!(f, "{bytes} B");
        }
        let exp = (u64::BITS - 1 - bytes.leading_zeros()) as usize / 10;
        let exp = exp.min(UNITS.len() - 1);
        let value = bytes as f64 / (1u64 << (10 * exp)) as f64;
        write!(f, "{value:.1} {}", UNITS[exp])
    }
}

/// Signed byte delta in binary units with an explicit sign, for diff
/// output: `+1.5 GiB`, `-2.0 MiB`, `+0 B`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedHumanSize(pub i64);

impl fmt::Display for SignedHumanSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { '-' } else { '+' };
        write!(f, "{sign}{}", HumanSize(self.0.unsigned_abs()))
    }
}

/// [`parse_size`] failure: the input is not a recognizable size.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid size {input:?}: expected NUMBER[K|M|G|T|P][iB|B], e.g. 500M, 2G, 1.5GiB")]
pub struct ParseSizeError {
    input: String,
}

/// Parse a human size like `500M`, `2G`, `1.5GiB`, `1048576`.
///
/// Grammar: a non-negative decimal number (fraction allowed), optionally
/// followed by a unit — `K`, `M`, `G`, `T`, `P` (binary multiples: `K` =
/// 1024 bytes, `M` = 1024², …), with an optional `iB`/`B` suffix and in
/// any case. A bare number is bytes. Whitespace around and between number
/// and unit is ignored.
pub fn parse_size(input: &str) -> Result<u64, ParseSizeError> {
    let err = || ParseSizeError {
        input: input.to_owned(),
    };
    let s = input.trim();
    let split = s
        .find(|c: char| c != '.' && !c.is_ascii_digit())
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let value: f64 = number.parse().map_err(|_| err())?;
    if !value.is_finite() || value < 0.0 {
        return Err(err());
    }
    let exp = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0u32,
        "k" | "kib" | "kb" => 1,
        "m" | "mib" | "mb" => 2,
        "g" | "gib" | "gb" => 3,
        "t" | "tib" | "tb" => 4,
        "p" | "pib" | "pb" => 5,
        _ => return Err(err()),
    };
    let bytes = value * 1024f64.powi(exp as i32);
    if bytes > u64::MAX as f64 {
        return Err(err());
    }
    Ok(bytes.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_from_stat_fields() {
        // 1 apparent byte in a 4 KiB-block file: real > apparent.
        let size = Size::new(1, 8);
        assert_eq!(size.apparent, 1);
        assert_eq!(size.real, 4096);
    }

    #[test]
    fn sparse_file_real_below_apparent() {
        // 1 GiB apparent, one 4 KiB block actually allocated.
        let size = Size::new(1 << 30, 8);
        assert!(size.real < size.apparent);
    }

    #[test]
    fn aggregation_sums_both_sizes() {
        let mut total = Size::default();
        total.add(Size::new(100, 8));
        total.add(Size::new(200, 16));
        assert_eq!(total.apparent, 300);
        assert_eq!(total.real, 24 * Size::BLOCK_UNIT);
    }

    #[test]
    fn human_size_formatting() {
        assert_eq!(HumanSize(0).to_string(), "0 B");
        assert_eq!(HumanSize(1023).to_string(), "1023 B");
        assert_eq!(HumanSize(1024).to_string(), "1.0 KiB");
        assert_eq!(HumanSize(1536).to_string(), "1.5 KiB");
        assert_eq!(HumanSize(1 << 20).to_string(), "1.0 MiB");
        assert_eq!(HumanSize(u64::MAX).to_string(), "16.0 EiB");
    }

    #[test]
    fn signed_human_size_carries_the_sign() {
        assert_eq!(SignedHumanSize(0).to_string(), "+0 B");
        assert_eq!(SignedHumanSize(1536).to_string(), "+1.5 KiB");
        assert_eq!(SignedHumanSize(-(1 << 21)).to_string(), "-2.0 MiB");
        assert_eq!(SignedHumanSize(i64::MIN).to_string(), "-8.0 EiB");
    }

    #[test]
    fn parse_size_accepts_the_documented_forms() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert_eq!(parse_size("500M"), Ok(500 << 20));
        assert_eq!(parse_size("2G"), Ok(2 << 30));
        assert_eq!(parse_size("1.5G"), Ok(3 << 29));
        assert_eq!(parse_size("1.5GiB"), Ok(3 << 29));
        assert_eq!(parse_size("2gb"), Ok(2 << 30));
        assert_eq!(parse_size(" 10 K "), Ok(10 << 10));
        assert_eq!(parse_size("7B"), Ok(7));
    }

    #[test]
    fn parse_size_rejects_junk() {
        for bad in ["", "G", "-1G", "1X", "1.2.3", "1 giga", "NaN"] {
            assert!(parse_size(bad).is_err(), "{bad:?} must be rejected");
        }
    }
}
