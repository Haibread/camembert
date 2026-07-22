//! zstd *seekable* container writer (spec §2).
//!
//! The byte stream is a sequence of **independent zstd frames** (target
//! ~512 KiB uncompressed each, checksummed for §9 torn-frame detection),
//! followed by the seek table in a zstd **skippable frame** per the
//! standard zstd seekable format. Frame boundaries always fall between
//! lines: [`FrameWriter::write_line`] is the only append primitive and a
//! flush can only happen after a whole line landed. Stock `zstd -d` /
//! `zstdcat` decode the frames and silently skip the seek table.

use std::io::{self, Write};

use zstd::bulk::Compressor;
use zstd::zstd_safe::CParameter;

/// Target uncompressed frame size (spec §2: ~512 KiB).
pub(crate) const TARGET_FRAME_UNCOMPRESSED: usize = 512 * 1024;

/// Skippable-frame magic used by the seekable format (0x184D2A5E: the
/// generic skippable range 0x184D2A5? with nibble 0xE).
const SKIPPABLE_MAGIC_SEEKABLE: u32 = 0x184D_2A5E;

/// Seekable-format footer magic.
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;

/// Seek-table descriptor byte: no per-frame checksums in the table
/// (the data frames carry their own zstd content checksums).
const SEEK_TABLE_DESCRIPTOR: u8 = 0x00;

/// One data frame's seek-table entry.
#[derive(Debug, Clone, Copy)]
struct FrameEntry {
    compressed: u32,
    decompressed: u32,
}

/// Streaming writer of the seekable container: lines in, frames out, seek
/// table appended by [`FrameWriter::finish`].
pub(crate) struct FrameWriter<W: Write> {
    out: W,
    /// Uncompressed lines pending in the current frame.
    buf: Vec<u8>,
    compressor: Compressor<'static>,
    frames: Vec<FrameEntry>,
    target: usize,
}

impl<W: Write> FrameWriter<W> {
    /// `target` is [`TARGET_FRAME_UNCOMPRESSED`] in production; tests pass
    /// smaller values to exercise frame boundaries without megabytes of
    /// fixture data.
    pub(crate) fn with_target(out: W, target: usize) -> io::Result<Self> {
        let mut compressor = Compressor::new(zstd::DEFAULT_COMPRESSION_LEVEL)?;
        compressor.set_parameter(CParameter::ChecksumFlag(true))?;
        Ok(Self {
            out,
            buf: Vec::with_capacity(target + 4 * 1024),
            compressor,
            frames: Vec::new(),
            target,
        })
    }

    /// Ordinal of the frame the *next* written line will land in (`x`-line
    /// bookkeeping: the pending buffer flushes as this ordinal).
    pub(crate) fn frame_ordinal(&self) -> u64 {
        self.frames.len() as u64
    }

    /// Append one newline-terminated line; flush a frame when the target
    /// size is reached (so no line ever spans frames).
    pub(crate) fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        debug_assert!(line.ends_with(b"\n"), "lines must be newline-terminated");
        self.buf.extend_from_slice(line);
        if self.buf.len() >= self.target {
            self.flush_frame()?;
        }
        Ok(())
    }

    fn flush_frame(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let compressed = self.compressor.compress(&self.buf)?;
        self.out.write_all(&compressed)?;
        self.frames.push(FrameEntry {
            compressed: u32::try_from(compressed.len()).expect("frame compressed size fits u32"),
            decompressed: u32::try_from(self.buf.len()).expect("frame uncompressed size fits u32"),
        });
        self.buf.clear();
        Ok(())
    }

    /// Flush the pending frame, append the seek table, and hand back the
    /// underlying writer (unflushed).
    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.flush_frame()?;
        let mut table = Vec::with_capacity(self.frames.len() * 8 + 17);
        table.extend_from_slice(&SKIPPABLE_MAGIC_SEEKABLE.to_le_bytes());
        let payload_len = u32::try_from(self.frames.len() * 8 + 9).expect("seek table fits u32");
        table.extend_from_slice(&payload_len.to_le_bytes());
        for frame in &self.frames {
            table.extend_from_slice(&frame.compressed.to_le_bytes());
            table.extend_from_slice(&frame.decompressed.to_le_bytes());
        }
        let count = u32::try_from(self.frames.len()).expect("frame count fits u32");
        table.extend_from_slice(&count.to_le_bytes());
        table.push(SEEK_TABLE_DESCRIPTOR);
        table.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
        self.out.write_all(&table)?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(i: usize) -> Vec<u8> {
        format!("{{\"n\":\"file-{i:04}\",\"a\":1,\"d\":512,\"m\":0}}\n").into_bytes()
    }

    /// Parsed trailing seek table: (compressed, decompressed) per frame.
    fn parse_seek_table(bytes: &[u8]) -> Vec<(u32, u32)> {
        let len = bytes.len();
        assert_eq!(&bytes[len - 4..], &SEEKABLE_MAGIC.to_le_bytes());
        assert_eq!(bytes[len - 5], SEEK_TABLE_DESCRIPTOR);
        let count = u32::from_le_bytes(bytes[len - 9..len - 5].try_into().unwrap()) as usize;
        let table_start = len - (8 + count * 8 + 9);
        assert_eq!(
            &bytes[table_start..table_start + 4],
            &SKIPPABLE_MAGIC_SEEKABLE.to_le_bytes(),
            "skippable magic"
        );
        let payload_len =
            u32::from_le_bytes(bytes[table_start + 4..table_start + 8].try_into().unwrap());
        assert_eq!(payload_len as usize, count * 8 + 9, "skippable frame size");
        (0..count)
            .map(|i| {
                let at = table_start + 8 + i * 8;
                (
                    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
                    u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
                )
            })
            .collect()
    }

    #[test]
    fn seek_table_golden_layout() {
        let mut writer = FrameWriter::with_target(Vec::new(), 256).unwrap();
        let mut content = Vec::new();
        for i in 0..40 {
            let l = line(i);
            writer.write_line(&l).unwrap();
            content.extend_from_slice(&l);
        }
        let bytes = writer.finish().unwrap();

        let entries = parse_seek_table(&bytes);
        assert!(entries.len() > 1, "small target must produce many frames");

        // Entries tile the file exactly: frames back to back from offset
        // 0, then the skippable frame to the end.
        let frames_len: usize = entries.iter().map(|&(c, _)| c as usize).sum();
        assert_eq!(frames_len + 8 + entries.len() * 8 + 9, bytes.len());
        let content_len: usize = entries.iter().map(|&(_, d)| d as usize).sum();
        assert_eq!(content_len, content.len());

        // Each frame decodes independently to whole lines.
        let mut offset = 0;
        let mut rebuilt = Vec::new();
        for &(compressed, decompressed) in &entries {
            let frame = &bytes[offset..offset + compressed as usize];
            let data = zstd::bulk::decompress(frame, decompressed as usize).unwrap();
            assert_eq!(data.len(), decompressed as usize);
            assert_eq!(data.last(), Some(&b'\n'), "no line spans frames");
            rebuilt.extend_from_slice(&data);
            offset += compressed as usize;
        }
        assert_eq!(rebuilt, content);
    }

    #[test]
    fn stock_stream_decode_ignores_the_seek_table() {
        let mut writer = FrameWriter::with_target(Vec::new(), 128).unwrap();
        let mut content = Vec::new();
        for i in 0..25 {
            let l = line(i);
            writer.write_line(&l).unwrap();
            content.extend_from_slice(&l);
        }
        let bytes = writer.finish().unwrap();
        let decoded = zstd::stream::decode_all(&bytes[..]).unwrap();
        assert_eq!(decoded, content);
    }

    #[test]
    fn frame_ordinal_tracks_flushes() {
        let mut writer = FrameWriter::with_target(Vec::new(), 64).unwrap();
        assert_eq!(writer.frame_ordinal(), 0);
        writer.write_line(&line(0)).unwrap(); // 41 bytes: stays buffered
        assert_eq!(writer.frame_ordinal(), 0);
        writer.write_line(&line(1)).unwrap(); // crosses 64: flushes
        assert_eq!(writer.frame_ordinal(), 1);
    }
}
