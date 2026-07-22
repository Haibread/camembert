//! camembert-dump v1 streaming reader (`docs/format/dump-v1.md`).
//!
//! [`DumpReader`] iterates a `.cmbt` dump **block by block**: the header
//! is parsed at construction, then [`DumpReader::next_block`] yields one
//! [`DirBlock`] per `d` line together with its entry lines; `x` lines are
//! skipped and the `e` line is captured as [`DumpReader::end`]. Memory is
//! bounded by one zstd frame plus one directory block.
//!
//! Robustness follows the spec:
//!
//! - **Container** (§2): standard zstd frames decoded one at a time; the
//!   seek table (and any other skippable frame) is recognized by magic and
//!   skipped. A dump that is a single plain zstd frame (no seek table)
//!   reads fine — handy for tests and third-party writers.
//! - **Numbers** (§5): every u64/i64 field is accepted as JSON number or
//!   decimal string; `ino`/`dev` are usually strings but numbers are
//!   tolerated on read.
//! - **Names** (§4): percent-encoded JSON strings are decoded back to raw
//!   bytes with [`decode_name`].
//! - **Truncation** (§9): a torn trailing frame is dropped whole and every
//!   block before it is returned; [`DumpReader::is_complete`] reports
//!   whether the clean-completion `e` marker was seen. A dump without `e`
//!   is a valid prefix, not an error — callers that need completeness
//!   (the differ) check the flag.
//! - **Evolution** (§10): unknown keys and unknown `t` record types are
//!   ignored; unknown `k`/`ex` enum values are preserved opaquely.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use serde_json::Value;

use super::decode_name;

/// Errors that abort reading a dump. Truncation is *not* an error (see
/// module docs); malformed JSON on a line is, because it means the input
/// is not a camembert dump (or is corrupt beyond the §9 torn-frame model).
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("I/O error reading dump: {0}")]
    Io(#[from] io::Error),
    /// The input is not a camembert dump at all (not zstd, empty, or the
    /// first line is not a `camembert-dump` header).
    #[error("not a camembert dump: {0}")]
    NotADump(String),
    /// Header major version this reader does not know (spec §10: readers
    /// refuse unknown majors — a major bump changes comparator/encoding
    /// semantics and silently mis-diffing would be worse than refusing).
    #[error("unsupported camembert-dump major version {found} (this reader understands major 1)")]
    UnsupportedMajor { found: u64 },
    #[error("malformed dump line {line}: {msg}")]
    Malformed { line: u64, msg: String },
}

/// Parsed header line (`t:"h"`, spec §6.1).
#[derive(Debug, Clone)]
pub struct Header {
    /// Additive minor version (unknown minors are fine, §10).
    pub minor: u64,
    /// Wall-clock time of the dump (unix seconds).
    pub ts: u64,
    /// Scan root path, decoded to raw bytes.
    pub root: Vec<u8>,
    /// Device of the root (`st_dev`).
    pub dev: u64,
    /// Size semantics of defaults: `"blocks"` or `"apparent"`.
    pub sem: String,
    /// Extended metadata (uid/gid/mode) present.
    pub ext: bool,
    /// Tier-2 ordering (§7): DFS preorder, siblings by raw name bytes.
    pub ordered: bool,
    /// Every entry carries `i`, not just `nlink > 1` ones.
    pub allino: bool,
}

/// Subtree totals of a `d` line (present in ordered dumps only, §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    /// Subtree apparent bytes.
    pub ta: u64,
    /// Subtree disk bytes.
    pub td: u64,
    /// Subtree inode count (hardlink extras excluded).
    pub tn: u64,
    /// Subtree unreadable-children count.
    pub te: u64,
}

/// One directory block: the `d` line plus its entry lines (spec §6.2/6.4).
#[derive(Debug, Clone)]
pub struct DirBlock {
    /// Full path, decoded to raw bytes.
    pub path: Vec<u8>,
    /// The directory inode's own apparent size.
    pub apparent: u64,
    /// The directory inode's own disk size.
    pub disk: u64,
    /// The directory inode's mtime (unix seconds).
    pub mtime: i64,
    /// Direct file / subdirectory counts.
    pub nf: u64,
    pub nd: u64,
    /// Subtree totals (ordered dumps).
    pub totals: Option<Totals>,
    /// The directory itself could not be read.
    pub err: bool,
    /// Excluded reason (`"otherfs"`, `"kernfs"`, …) for never-scanned
    /// mount-point stubs; unknown values preserved opaquely.
    pub ex: Option<String>,
    /// Non-directory children, in file order (raw-byte sorted, §7 tier 1).
    pub entries: Vec<Entry>,
}

/// One entry line (spec §6.4), block defaults (`s` line) applied.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Name, decoded to raw bytes.
    pub name: Vec<u8>,
    /// Apparent size (`st_size`).
    pub apparent: u64,
    /// Disk size (`st_blocks * 512`, bytes).
    pub disk: u64,
    /// mtime, unix seconds.
    pub mtime: i64,
    /// Kind letter (`l`, `b`, `c`, `f`, `s`); `None` for regular files.
    /// Unknown letters are preserved opaquely (§10).
    pub kind: Option<String>,
    /// Inode, when emitted (`nlink > 1`, or all entries with `allino`).
    pub ino: Option<u64>,
    /// `st_nlink`, when `> 1`.
    pub nlink: Option<u64>,
    /// Device, when it differs from the block default. `None` means "same
    /// filesystem as the containing directory".
    pub dev: Option<u64>,
    /// stat/read failed; sizes are zero.
    pub err: bool,
    /// Excluded reason, unknown values preserved opaquely.
    pub ex: Option<String>,
}

/// Parsed end marker (`t:"e"`, spec §6.5). Its very presence is the
/// clean-completion signal.
#[derive(Debug, Clone, Copy)]
pub struct End {
    pub entries: u64,
    pub dirs: u64,
    pub errors: u64,
    /// Root subtree totals (mirror of the root `d` line).
    pub ta: u64,
    pub td: u64,
    /// Scan wall time, seconds.
    pub elapsed: f64,
}

/// `s`-line block defaults (spec §6.3), reset at each `d` line.
#[derive(Debug, Clone, Copy, Default)]
struct BlockDefaults {
    dev: Option<u64>,
}

/// Streaming dump reader; see the module docs for the contract.
pub struct DumpReader<R: Read> {
    lines: LineReader<R>,
    header: Header,
    /// `d` line already consumed for the *next* block.
    pending: Option<DirBlock>,
    end: Option<End>,
    /// All blocks consumed (`e` seen or clean EOF); only `x`/`e` remain.
    blocks_done: bool,
    line_no: u64,
    defaults: BlockDefaults,
}

impl DumpReader<BufReader<File>> {
    /// Open a `.cmbt` file and parse its header.
    pub fn open(path: &Path) -> Result<Self, ReadError> {
        let file = File::open(path)?;
        Self::new(BufReader::new(file))
    }
}

impl<R: Read> DumpReader<R> {
    /// Wrap any byte stream (it need not be seekable) and parse the
    /// header. Fails with [`ReadError::NotADump`] on non-dump input and
    /// [`ReadError::UnsupportedMajor`] on a major version bump.
    pub fn new(input: R) -> Result<Self, ReadError> {
        let mut lines = LineReader::new(input);
        let first = match lines.next_line() {
            Ok(Some(line)) => line,
            Ok(None) => return Err(ReadError::NotADump("empty input".into())),
            Err(err) => return Err(ReadError::NotADump(format!("not a zstd stream: {err}"))),
        };
        let value: Value = serde_json::from_slice(&first)
            .map_err(|err| ReadError::NotADump(format!("first line is not JSON: {err}")))?;
        if str_field(&value, "t") != Some("h")
            || str_field(&value, "format") != Some("camembert-dump")
        {
            return Err(ReadError::NotADump(
                "first line is not a camembert-dump header".into(),
            ));
        }
        let major = u64_field(&value, "v").ok().flatten().unwrap_or(0);
        if major != 1 {
            return Err(ReadError::UnsupportedMajor { found: major });
        }
        let at = |msg: String| ReadError::Malformed { line: 1, msg };
        let header = Header {
            minor: u64_field(&value, "minor").map_err(&at)?.unwrap_or(0),
            ts: u64_field(&value, "ts").map_err(&at)?.unwrap_or(0),
            root: decode_name(str_field(&value, "root").unwrap_or("")),
            dev: u64_field(&value, "dev").map_err(&at)?.unwrap_or(0),
            sem: str_field(&value, "sem").unwrap_or("blocks").to_owned(),
            ext: bool_field(&value, "ext"),
            ordered: bool_field(&value, "ordered"),
            allino: bool_field(&value, "allino"),
        };
        Ok(Self {
            lines,
            header,
            pending: None,
            end: None,
            blocks_done: false,
            line_no: 1,
            defaults: BlockDefaults::default(),
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The `e` end marker, once the blocks are exhausted.
    pub fn end(&self) -> Option<&End> {
        self.end.as_ref()
    }

    /// Whether the clean-completion `e` line was seen. Meaningful once
    /// [`DumpReader::next_block`] has returned `None`; `false` there means
    /// the dump is a valid but incomplete prefix (§9).
    pub fn is_complete(&self) -> bool {
        self.end.is_some()
    }

    /// Whether a torn trailing frame (or partial line) was dropped (§9).
    pub fn truncated(&self) -> bool {
        self.lines.truncated
    }

    /// Next directory block, or `None` after the last one. Call until
    /// `None` before trusting [`DumpReader::is_complete`].
    pub fn next_block(&mut self) -> Result<Option<DirBlock>, ReadError> {
        if self.blocks_done {
            return Ok(None);
        }
        let mut current = self.pending.take();
        loop {
            let line = match self.lines.next_line()? {
                Some(line) => line,
                None => {
                    // EOF without `e`: truncated dump, valid prefix (§9).
                    self.blocks_done = true;
                    return Ok(current.take());
                }
            };
            self.line_no += 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value =
                serde_json::from_slice(&line).map_err(|err| ReadError::Malformed {
                    line: self.line_no,
                    msg: format!("invalid JSON: {err}"),
                })?;
            match str_field(&value, "t") {
                None => {
                    let Some(block) = current.as_mut() else {
                        return Err(ReadError::Malformed {
                            line: self.line_no,
                            msg: "entry line before any directory line".into(),
                        });
                    };
                    let entry = self.parse_entry(&value)?;
                    block.entries.push(entry);
                }
                Some("d") => {
                    self.defaults = BlockDefaults::default();
                    let next = self.parse_dir(&value)?;
                    match current.take() {
                        Some(done) => {
                            self.pending = Some(next);
                            return Ok(Some(done));
                        }
                        None => current = Some(next),
                    }
                }
                Some("s") => {
                    self.defaults.dev =
                        u64_field(&value, "dev").map_err(|msg| ReadError::Malformed {
                            line: self.line_no,
                            msg,
                        })?;
                }
                Some("e") => {
                    self.end = Some(self.parse_end(&value)?);
                    self.blocks_done = true;
                    return Ok(current.take());
                }
                // `x` frame-index lines and unknown record types (§10).
                Some(_) => {}
            }
        }
    }

    fn malformed(&self, msg: String) -> ReadError {
        ReadError::Malformed {
            line: self.line_no,
            msg,
        }
    }

    fn parse_dir(&self, value: &Value) -> Result<DirBlock, ReadError> {
        let at = |msg| self.malformed(msg);
        let path =
            str_field(value, "path").ok_or_else(|| self.malformed("d line without path".into()))?;
        let totals = match u64_field(value, "ta").map_err(at)? {
            Some(ta) => Some(Totals {
                ta,
                td: u64_field(value, "td").map_err(at)?.unwrap_or(0),
                tn: u64_field(value, "tn").map_err(at)?.unwrap_or(0),
                te: u64_field(value, "te").map_err(at)?.unwrap_or(0),
            }),
            None => None,
        };
        Ok(DirBlock {
            path: decode_name(path),
            apparent: u64_field(value, "a").map_err(at)?.unwrap_or(0),
            disk: u64_field(value, "d").map_err(at)?.unwrap_or(0),
            mtime: i64_field(value, "m").map_err(at)?.unwrap_or(0),
            nf: u64_field(value, "nf").map_err(at)?.unwrap_or(0),
            nd: u64_field(value, "nd").map_err(at)?.unwrap_or(0),
            totals,
            err: bool_field(value, "err"),
            ex: str_field(value, "ex").map(str::to_owned),
            entries: Vec::new(),
        })
    }

    fn parse_entry(&self, value: &Value) -> Result<Entry, ReadError> {
        let at = |msg| self.malformed(msg);
        let name =
            str_field(value, "n").ok_or_else(|| self.malformed("entry line without n".into()))?;
        Ok(Entry {
            name: decode_name(name),
            apparent: u64_field(value, "a").map_err(at)?.unwrap_or(0),
            disk: u64_field(value, "d").map_err(at)?.unwrap_or(0),
            mtime: i64_field(value, "m").map_err(at)?.unwrap_or(0),
            kind: str_field(value, "k").map(str::to_owned),
            ino: u64_field(value, "i").map_err(at)?,
            nlink: u64_field(value, "l").map_err(at)?,
            dev: u64_field(value, "dev").map_err(at)?.or(self.defaults.dev),
            err: bool_field(value, "err"),
            ex: str_field(value, "ex").map(str::to_owned),
        })
    }

    fn parse_end(&self, value: &Value) -> Result<End, ReadError> {
        let at = |msg| self.malformed(msg);
        Ok(End {
            entries: u64_field(value, "entries").map_err(at)?.unwrap_or(0),
            dirs: u64_field(value, "dirs").map_err(at)?.unwrap_or(0),
            errors: u64_field(value, "errors").map_err(at)?.unwrap_or(0),
            ta: u64_field(value, "ta").map_err(at)?.unwrap_or(0),
            td: u64_field(value, "td").map_err(at)?.unwrap_or(0),
            elapsed: value.get("elapsed").and_then(Value::as_f64).unwrap_or(0.0),
        })
    }
}

// ---- field extraction under the D4 number policy (§5) ----

/// u64 field: JSON number or decimal string, both accepted.
fn u64_field(value: &Value, key: &str) -> Result<Option<u64>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("field {key}: not a u64: {n}")),
        Some(Value::String(s)) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("field {key}: not a decimal u64 string: {s:?}")),
        Some(other) => Err(format!(
            "field {key}: expected number or string, got {other}"
        )),
    }
}

/// i64 field: JSON number or decimal string, both accepted.
fn i64_field(value: &Value, key: &str) -> Result<Option<i64>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("field {key}: not an i64: {n}")),
        Some(Value::String(s)) => s
            .parse::<i64>()
            .map(Some)
            .map_err(|_| format!("field {key}: not a decimal i64 string: {s:?}")),
        Some(other) => Err(format!(
            "field {key}: expected number or string, got {other}"
        )),
    }
}

fn str_field<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// ---- container layer: frames in, lines out ----

/// Skippable-frame magic range (`0x184D2A50..=0x184D2A5F`): the seek
/// table and any other metadata frame stock zstd would skip (§2).
const SKIPPABLE_MAGIC_LOW: u32 = 0x184D_2A50;
const SKIPPABLE_MAGIC_HIGH: u32 = 0x184D_2A5F;

/// Newline-terminated lines out of a sequence of zstd frames. A torn
/// trailing frame is dropped whole (§9) and flagged, never an error.
struct LineReader<R: Read> {
    frames: FrameDecoder<R>,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
    truncated: bool,
}

impl<R: Read> LineReader<R> {
    fn new(input: R) -> Self {
        Self {
            frames: FrameDecoder::new(input),
            buf: Vec::new(),
            pos: 0,
            done: false,
            truncated: false,
        }
    }

    fn next_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(nl) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                let line = self.buf[self.pos..self.pos + nl].to_vec();
                self.pos += nl + 1;
                return Ok(Some(line));
            }
            self.buf.drain(..self.pos);
            self.pos = 0;
            if self.done {
                if !self.buf.is_empty() {
                    // Partial line at EOF: only a torn write can produce
                    // it (frames end at line boundaries, §2); drop it.
                    self.truncated = true;
                    self.buf.clear();
                }
                return Ok(None);
            }
            if !self.frames.next_frame(&mut self.buf)? {
                self.done = true;
                self.truncated |= self.frames.truncated;
            }
        }
    }
}

/// Decodes one zstd frame at a time, skipping skippable frames by magic.
struct FrameDecoder<R: Read> {
    input: PeekBuf<R>,
    truncated: bool,
}

impl<R: Read> FrameDecoder<R> {
    fn new(input: R) -> Self {
        Self {
            input: PeekBuf::new(input),
            truncated: false,
        }
    }

    /// Append the next data frame's decompressed bytes to `out`. `false`
    /// on end of input; a torn frame sets [`Self::truncated`], contributes
    /// nothing to `out` (dropped whole, §9), and ends the stream.
    ///
    /// I/O errors from the underlying reader are real errors; zstd decode
    /// errors are truncation (the §9 torn-frame model) — distinguished by
    /// whether the error happened before any valid frame content.
    fn next_frame(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        loop {
            let head = self.input.peek(4)?;
            if head.is_empty() {
                return Ok(false);
            }
            if head.len() < 4 {
                // Fewer than 4 bytes cannot start any zstd frame.
                self.truncated = true;
                return Ok(false);
            }
            let magic = u32::from_le_bytes(head[..4].try_into().expect("4 bytes"));
            if (SKIPPABLE_MAGIC_LOW..=SKIPPABLE_MAGIC_HIGH).contains(&magic) {
                if self.skip_skippable().is_err() {
                    self.truncated = true;
                    return Ok(false);
                }
                continue;
            }
            let start = out.len();
            let mut decoder = match zstd::stream::read::Decoder::with_buffer(&mut self.input) {
                Ok(decoder) => decoder.single_frame(),
                Err(_) => {
                    self.truncated = true;
                    return Ok(false);
                }
            };
            match decoder.read_to_end(out) {
                Ok(_) => return Ok(true),
                Err(_) => {
                    // Torn or corrupt frame (checksum, unexpected EOF):
                    // drop whatever it partially produced.
                    out.truncate(start);
                    self.truncated = true;
                    return Ok(false);
                }
            }
        }
    }

    fn skip_skippable(&mut self) -> io::Result<()> {
        let mut header = [0u8; 8];
        self.input.read_exact(&mut header)?;
        let len = u64::from(u32::from_le_bytes(
            header[4..8].try_into().expect("4 bytes"),
        ));
        let skipped = io::copy(&mut (&mut self.input).take(len), &mut io::sink())?;
        if skipped != len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}

/// A [`std::io::BufRead`] with a guaranteed multi-byte peek: `fill_buf` on
/// an arbitrary `BufRead` may legally return a single byte, which is not
/// enough to check a 4-byte frame magic without consuming it.
struct PeekBuf<R: Read> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
}

impl<R: Read> PeekBuf<R> {
    const CHUNK: usize = 64 * 1024;

    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// At least `n` buffered bytes unless the input ends first.
    fn peek(&mut self, n: usize) -> io::Result<&[u8]> {
        while self.buf.len() - self.pos < n {
            if self.pos > 0 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            let start = self.buf.len();
            self.buf.resize(start + Self::CHUNK, 0);
            let got = self.inner.read(&mut self.buf[start..])?;
            self.buf.truncate(start + got);
            if got == 0 {
                break;
            }
        }
        Ok(&self.buf[self.pos..])
    }
}

impl<R: Read> Read for PeekBuf<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.buf.len() {
            let n = out.len().min(self.buf.len() - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(out)
    }
}

impl<R: Read> io::BufRead for PeekBuf<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            self.buf.resize(Self::CHUNK, 0);
            let got = self.inner.read(&mut self.buf)?;
            self.buf.truncate(got);
        }
        Ok(&self.buf[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.buf.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size::Size;
    use crate::tree::{ChildRun, Kind, NodeFlags, Tree};

    /// Compress JSON-lines text as one plain zstd frame (no seek table):
    /// a valid §2 container the reader must accept.
    fn plain_dump(lines: &str) -> Vec<u8> {
        zstd::stream::encode_all(lines.as_bytes(), 0).expect("zstd encode")
    }

    const HEADER: &str = r#"{"t":"h","format":"camembert-dump","v":1,"minor":0,"ts":1753142400,"root":"/r","dev":"5","sem":"blocks","ext":false,"ordered":true,"allino":false}"#;

    fn reader_over(lines: &str) -> DumpReader<&[u8]> {
        // Leak is fine in tests; keeps the reader non-generic over Cursor.
        let bytes: &'static [u8] = Box::leak(plain_dump(lines).into_boxed_slice());
        DumpReader::new(bytes).expect("valid dump")
    }

    #[test]
    fn rejects_non_dumps_and_unknown_majors() {
        let empty = DumpReader::new(&b""[..]);
        assert!(matches!(empty, Err(ReadError::NotADump(_))));

        let garbage = DumpReader::new(&b"hello world, definitely not zstd"[..]);
        assert!(matches!(garbage, Err(ReadError::NotADump(_))));

        let not_json = plain_dump("this is not json\n");
        assert!(matches!(
            DumpReader::new(&not_json[..]),
            Err(ReadError::NotADump(_))
        ));

        let wrong_format = plain_dump("{\"t\":\"h\",\"format\":\"something-else\",\"v\":1}\n");
        assert!(matches!(
            DumpReader::new(&wrong_format[..]),
            Err(ReadError::NotADump(_))
        ));

        let major2 = plain_dump("{\"t\":\"h\",\"format\":\"camembert-dump\",\"v\":2}\n");
        assert!(matches!(
            DumpReader::new(&major2[..]),
            Err(ReadError::UnsupportedMajor { found: 2 })
        ));
    }

    #[test]
    fn header_fields_parse_with_string_dev() {
        let text = format!("{HEADER}\n");
        let mut reader = reader_over(&text);
        let header = reader.header().clone();
        assert_eq!(header.minor, 0);
        assert_eq!(header.root, b"/r");
        assert_eq!(header.dev, 5);
        assert!(header.ordered);
        assert!(!header.ext);
        assert!(reader.next_block().unwrap().is_none());
        assert!(!reader.is_complete(), "no e line");
    }

    #[test]
    fn u64_fields_accept_number_or_string() {
        let big = (1u64 << 53) + 17;
        let text = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            format_args!(
                r#"{{"t":"d","path":"/r","a":4096,"d":"4096","m":100,"nf":1,"nd":0,"ta":"{big}","td":8192,"tn":2,"te":0}}"#
            ),
            r#"{"n":"f","a":"10","d":512,"m":"-5","i":"9007199254740993","l":2}"#,
            r#"{"t":"e","entries":"2","dirs":1,"errors":0,"ta":100,"td":8192,"elapsed":1.5}"#,
        );
        let mut reader = reader_over(&text);
        let block = reader.next_block().unwrap().expect("one block");
        assert_eq!(block.disk, 4096, "string u64 accepted");
        assert_eq!(block.totals.unwrap().ta, big, "big string u64");
        let entry = &block.entries[0];
        assert_eq!(entry.apparent, 10);
        assert_eq!(entry.mtime, -5, "string i64 accepted");
        assert_eq!(entry.ino, Some((1 << 53) + 1), "string ino above 2^53");
        assert!(reader.next_block().unwrap().is_none());
        assert!(reader.is_complete());
        assert_eq!(reader.end().unwrap().entries, 2);
        assert!((reader.end().unwrap().elapsed - 1.5).abs() < 1e-9);
    }

    #[test]
    fn unknown_records_keys_and_enums_are_tolerated() {
        let text = format!(
            "{HEADER}\n{}\n{}\n{}\n{}\n{}\n",
            r#"{"t":"future-record","whatever":true}"#,
            r#"{"t":"d","path":"/r","a":0,"d":0,"m":0,"nf":1,"nd":0,"newkey":1}"#,
            r#"{"t":"s","dev":"77"}"#,
            r#"{"n":"weird","a":1,"d":512,"m":0,"k":"z","ex":"mystery","novel":[1,2]}"#,
            r#"{"t":"x","f":0,"p":"/r"}"#,
        );
        let mut reader = reader_over(&text);
        let block = reader.next_block().unwrap().expect("block");
        assert!(block.totals.is_none(), "d line without totals");
        let entry = &block.entries[0];
        assert_eq!(entry.kind.as_deref(), Some("z"), "unknown k preserved");
        assert_eq!(entry.ex.as_deref(), Some("mystery"), "unknown ex preserved");
        assert_eq!(entry.dev, Some(77), "s-line block default applied");
        assert!(reader.next_block().unwrap().is_none());
    }

    /// The golden round trip: a tree with non-UTF-8 names, a large string
    /// ino, an excluded mount and an errored entry goes through the writer
    /// and comes back losslessly.
    #[test]
    fn writer_round_trip_is_lossless() {
        let (tree, root, links) = writer_sample();
        let stats = crate::dump::EndStats {
            entries: 8,
            dirs: 2,
            errors: 2,
            elapsed_secs: 1.25,
        };
        let bytes = crate::dump::write_records(
            &tree,
            root,
            &links,
            &stats,
            1_753_142_400,
            Vec::new(),
            96, // tiny frames: exercise multi-frame reads + x lines
        )
        .expect("write");

        let mut reader = DumpReader::new(&bytes[..]).expect("read back");
        assert_eq!(reader.header().root, b"/r");
        assert_eq!(reader.header().dev, 5);
        assert!(reader.header().ordered);

        let mut blocks = Vec::new();
        while let Some(block) = reader.next_block().expect("block") {
            blocks.push(block);
        }
        assert!(reader.is_complete(), "e line seen");
        assert!(!reader.truncated());
        let end = reader.end().unwrap();
        assert_eq!((end.entries, end.dirs, end.errors), (8, 2, 2));

        let paths: Vec<&[u8]> = blocks.iter().map(|b| b.path.as_slice()).collect();
        assert_eq!(
            paths,
            [b"/r" as &[u8], b"/r/a", b"/r/broken", b"/r/mnt"],
            "DFS preorder, raw-byte siblings"
        );

        let root_block = &blocks[0];
        let names: Vec<&[u8]> = root_block
            .entries
            .iter()
            .map(|e| e.name.as_slice())
            .collect();
        assert_eq!(
            names,
            [b"b.txt" as &[u8], b"~", b"\xff"],
            "non-UTF-8 name decoded back to raw bytes, raw-byte order"
        );
        assert_eq!(root_block.totals.unwrap().tn, 9);

        let a = &blocks[1];
        let leaf = a.entries.iter().find(|e| e.name == b"leaf").unwrap();
        assert_eq!(leaf.ino, Some(1 << 60), "string ino round-trips");
        assert_eq!(leaf.nlink, Some(2));
        assert_eq!(leaf.dev, Some(6), "foreign dev round-trips");
        let bad = a.entries.iter().find(|e| e.name == b"bad").unwrap();
        assert!(bad.err);
        assert_eq!(
            a.totals.unwrap(),
            Totals {
                ta: 4096 + 100,
                td: 4096 + 512,
                tn: 3,
                te: 1
            }
        );

        let broken = &blocks[2];
        assert!(broken.err);
        assert_eq!(broken.entries.len(), 0);
        let mnt = &blocks[3];
        assert_eq!(mnt.ex.as_deref(), Some("otherfs"));
    }

    #[test]
    fn truncated_dump_yields_the_valid_prefix() {
        let (tree, root, links) = writer_sample();
        let stats = crate::dump::EndStats {
            entries: 8,
            dirs: 2,
            errors: 2,
            elapsed_secs: 1.25,
        };
        // Tiny frames so cutting the file leaves several intact ones.
        let bytes =
            crate::dump::write_records(&tree, root, &links, &stats, 1_753_142_400, Vec::new(), 64)
                .expect("write");

        // Cut inside the byte stream: everything decodable before the torn
        // frame must come back; completeness must be false.
        let cut = &bytes[..bytes.len() * 2 / 3];
        let mut reader = DumpReader::new(cut).expect("header still readable");
        let mut blocks = 0;
        while let Some(_block) = reader.next_block().expect("prefix reads cleanly") {
            blocks += 1;
        }
        assert!(blocks >= 1, "at least the root block survives the cut");
        assert!(!reader.is_complete(), "no e line: incomplete flagged");

        // Full file for comparison: complete, more or equal blocks.
        let mut full = DumpReader::new(&bytes[..]).expect("full");
        let mut full_blocks = 0;
        while full.next_block().expect("full reads").is_some() {
            full_blocks += 1;
        }
        assert!(full.is_complete());
        assert!(full_blocks >= blocks);
    }

    /// Same shape as `dump::tests::sample`: root `/r` with files
    /// (including `\xff`), dir `a` (hardlinked `leaf` + errored `bad`),
    /// excluded mount `mnt`, stat-failed dir `broken`.
    fn writer_sample() -> (Tree, crate::tree::DirId, Vec<crate::scan::HardlinkLink>) {
        use crate::tree::ExcludedReason;
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/r", Size::new(4096, 8), 100);
        let root = tree.add_dir(root_node, None, 5);
        let first = tree.push_node(
            b"~",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(10, 1),
            1,
        );
        tree.push_node(
            b"\xff",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(20, 1),
            2,
        );
        tree.push_node(
            b"b.txt",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(30, 1),
            3,
        );
        let a_node = tree.push_node(
            b"a",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::new(4096, 8),
            4,
        );
        let mnt_node = tree.push_node(
            b"mnt",
            Kind::Dir,
            NodeFlags::EXCLUDED,
            root_node,
            Size::new(4096, 8),
            5,
        );
        tree.set_excluded(mnt_node, ExcludedReason::OtherFs);
        tree.push_node(
            b"broken",
            Kind::Dir,
            NodeFlags::ERROR,
            root_node,
            Size::default(),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: first.index() as u32,
                len: 6,
            },
        );
        tree.apply_delta(root, 10 + 20 + 30 + 4096 + 4096, 512 * 3 + 4096 * 2, 6, 1);

        let a = tree.add_dir(a_node, Some(root), 5);
        let leaf = tree.push_node(
            b"leaf",
            Kind::File,
            NodeFlags::default(),
            a_node,
            Size::new(100, 1),
            6,
        );
        tree.push_node(
            b"bad",
            Kind::File,
            NodeFlags::ERROR,
            a_node,
            Size::default(),
            0,
        );
        tree.push_run(
            a,
            ChildRun {
                start: leaf.index() as u32,
                len: 2,
            },
        );
        tree.apply_delta(a, 100, 512, 2, 1);
        tree.release_token(a);
        tree.release_token(root);

        let links = vec![crate::scan::HardlinkLink {
            node: leaf,
            dev: 6,
            ino: 1 << 60,
            nlink: 2,
        }];
        (tree, root, links)
    }
}
