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
}
