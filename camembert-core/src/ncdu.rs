//! ncdu JSON export importer (spec §11, quirks per
//! `docs/design/dump-format-research.md` §1).
//!
//! [`import`] stream-parses an ncdu `-o` export — the nested
//! `[1, minor, metadata, tree]` array — directly into a [`Tree`] arena and
//! wraps it as a [`ScanOutcome`], so the ordinary dump writer (which sorts
//! siblings and computes ordered totals) emits it as a first-class ordered
//! `.cmbt`. That closes the adoption loop from HANDOFF §4/§5: an old ncdu
//! export becomes diffable against a fresh camembert scan.
//!
//! # Parser
//!
//! A hand-rolled byte-level JSON pull parser, not `serde_json`, for two
//! reasons: exports reach hundreds of MB (the ncdu spec itself recommends
//! SAX-style parsing) and — decisive — ncdu wrote **raw non-UTF-8 bytes**
//! inside JSON strings for years (rejected only since ncdu 2.5), which
//! UTF-8-validating parsers refuse. Names are handled as raw bytes
//! throughout, which is exactly what the dump format wants (§4). The
//! parser is iterative (explicit directory stack), so pathological tree
//! depth cannot overflow the call stack.
//!
//! # Field mapping (spec §11) and what is NOT carried
//!
//! | ncdu | camembert dump | notes |
//! |---|---|---|
//! | `name` | `n` / `path` | raw bytes, re-encoded per §4 |
//! | `asize` / `dsize` | `a` / `d` | absent → 0 |
//! | `dev` | entry `dev` | **absent ⇒ inherits the parent's** |
//! | `ino` + `nlink`/`hlnkc` | `i` / `l` | dedup by `(dev, ino)`, then canonical re-attribution (§8) |
//! | `read_error` | `err:true` (`d` line for dirs) | counted in `te` |
//! | `excluded: otherfs/othfs` | `d` stub with `ex:"otherfs"` | |
//! | `excluded: kernfs` | `d` stub with `ex:"kernfs"` | |
//! | `excluded: pattern/frmlink/…` | entry `ex:"otherfs"` | reason collapsed to `otherfs` (lossy) |
//! | `notreg` | kind [`Kind::Other`] | exact kind unknown; no `k` letter emitted |
//! | extended `mtime` | `m` | absent (export without `-e`) → `0` |
//! | extended `uid`/`gid`/`mode` | *dropped* | the arena has no ext storage yet (`ext:false` writer) |
//! | metadata block | *dropped* | ncdu documents it as ignored on import |
//! | `hlnkc` without `ino` | *not dedupable* | counted fully, debug-logged |
//! | `dev` of a non-hardlinked file | *dropped* | the packed node stores no per-entry device; the dump writer only emits `dev` for hardlinks |

use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use rustc_hash::FxHashSet;
use tracing::{debug, info, warn};

use crate::scan::{HardlinkLink, ScanOutcome};
use crate::size::Size;
use crate::tree::{ChildRun, DirId, ExcludedReason, Kind, NodeFlags, Tree};

/// Errors that abort an import. Per-entry oddities never abort — they are
/// mapped as documented in the module docs.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("I/O error reading ncdu export: {0}")]
    Io(#[from] std::io::Error),
    /// The input is not an ncdu JSON export (or is corrupt).
    #[error("not an ncdu JSON export (byte {offset}): {msg}")]
    Parse { offset: u64, msg: String },
    /// Top-level major version this importer does not know.
    #[error("unsupported ncdu export major version {found} (this importer understands major 1)")]
    UnsupportedMajor { found: i64 },
}

/// Import an ncdu JSON export into a [`ScanOutcome`] (hardlinks already
/// canonically attributed): ready for [`crate::dump::write_dump`].
///
/// Accepts ncdu minor versions 0–2 (1.9 through current); a higher minor
/// is imported anyway with a warning, since unknown fields are ignored by
/// construction.
pub fn import<R: Read>(input: R) -> Result<ScanOutcome, ImportError> {
    let start = Instant::now();
    let mut lexer = Lexer::new(input);

    lexer.expect(TokenKind::ArrStart)?;
    let major = lexer.expect_int("major version")?;
    if major != 1 {
        return Err(ImportError::UnsupportedMajor { found: major });
    }
    let minor = lexer.expect_int("minor version")?;
    if !(0..=2).contains(&minor) {
        warn!(
            minor,
            "unknown ncdu export minor version; importing anyway (unknown fields are ignored)"
        );
    }
    let meta = lexer.next()?;
    if meta.kind() != TokenKind::ObjStart {
        return Err(lexer.err("expected the metadata object"));
    }
    lexer.skip_value(meta)?; // documented as ignored on import
    lexer.expect(TokenKind::ArrStart)?; // the root directory array

    let importer = Importer::parse(&mut lexer)?;
    lexer.expect(TokenKind::ArrEnd)?; // close of the top-level array

    let Importer {
        tree,
        root,
        root_name,
        links,
        inodes,
        excluded_dirs,
        excluded_kernfs,
        ..
    } = importer;

    use std::os::unix::ffi::OsStrExt;
    let root_path = PathBuf::from(std::ffi::OsStr::from_bytes(&root_name));
    let mut outcome = ScanOutcome::from_tree(
        tree,
        root,
        root_path,
        links,
        inodes.len() as u64,
        excluded_dirs,
        excluded_kernfs,
        start.elapsed(),
    );
    // Canonical (smallest-path) hardlink attribution, spec §8: the dump
    // writer and the differ both rely on it.
    outcome.finalize_hardlinks();
    info!(
        entries = outcome.entries,
        dirs = outcome.dirs,
        errors = outcome.errors,
        hardlink_inodes = outcome.hardlink_inodes,
        elapsed_ms = outcome.elapsed.as_millis() as u64,
        "ncdu import complete"
    );
    Ok(outcome)
}

/// One parsed infoblock (the per-entry object of the ncdu format).
#[derive(Debug, Default)]
struct Info {
    name: Vec<u8>,
    asize: u64,
    dsize: u64,
    dev: Option<u64>,
    ino: Option<u64>,
    nlink: Option<u64>,
    hlnkc: bool,
    read_error: bool,
    notreg: bool,
    excluded: Option<Vec<u8>>,
    mtime: Option<i64>,
}

/// Direct-children sums of one directory (mirror of the scan's section
/// sums: hardlink extras contribute 0).
#[derive(Debug, Default, Clone, Copy)]
struct Sums {
    apparent: u64,
    disk: u64,
    count: u64,
    errors: u32,
}

/// One open directory during the iterative DFS parse.
struct Frame {
    dir: DirId,
    /// Device inherited by children without their own `dev`.
    dev: u64,
    /// Start of the current contiguous child run in the node arena
    /// (descending into a subdirectory closes the run; returning opens a
    /// new one — the D2 multi-run representation).
    run_start: u32,
    sums: Sums,
    read_error: bool,
}

struct Importer {
    tree: Tree,
    root: DirId,
    root_name: Vec<u8>,
    links: Vec<HardlinkLink>,
    inodes: FxHashSet<(u64, u64)>,
    excluded_dirs: u64,
    excluded_kernfs: u64,
}

impl Importer {
    fn parse<R: Read>(lexer: &mut Lexer<R>) -> Result<Self, ImportError> {
        // The root directory array's first element is its infoblock.
        let first = lexer.next()?;
        if first.kind() != TokenKind::ObjStart {
            return Err(lexer.err("directory array must start with an infoblock object"));
        }
        let root_info = parse_info(lexer)?;
        let root_dev = root_info.dev.unwrap_or_else(|| {
            debug!("root infoblock has no dev; defaulting to 0");
            0
        });

        let mut tree = Tree::new();
        let root_node = tree.push_root_node(
            &root_info.name,
            Size {
                apparent: root_info.asize,
                real: root_info.dsize,
            },
            root_info.mtime.unwrap_or(0),
        );
        let root = tree.add_dir(root_node, None, root_dev);
        let mut this = Self {
            tree,
            root,
            root_name: root_info.name.clone(),
            links: Vec::new(),
            inodes: FxHashSet::default(),
            excluded_dirs: 0,
            excluded_kernfs: 0,
        };

        let mut stack = vec![Frame {
            dir: root,
            dev: root_dev,
            run_start: this.node_count(),
            sums: Sums::default(),
            read_error: root_info.read_error,
        }];
        while !stack.is_empty() {
            let token = lexer.next()?;
            match token.kind() {
                TokenKind::ObjStart => {
                    let info = parse_info(lexer)?;
                    let frame = stack.last_mut().expect("loop condition");
                    this.file_entry(frame, &info);
                }
                TokenKind::ArrStart => {
                    let first = lexer.next()?;
                    if first.kind() != TokenKind::ObjStart {
                        return Err(
                            lexer.err("directory array must start with an infoblock object")
                        );
                    }
                    let info = parse_info(lexer)?;
                    let frame = stack.last_mut().expect("loop condition");
                    let child = this.open_dir(frame, &info);
                    stack.push(child);
                }
                TokenKind::ArrEnd => {
                    let frame = stack.pop().expect("loop condition");
                    this.close_dir(frame);
                    if let Some(parent) = stack.last_mut() {
                        // The subtree interleaved into the arena: the
                        // parent's next child run starts after it (D2).
                        parent.run_start = this.node_count();
                    }
                }
                _ => return Err(lexer.err("unexpected value in a directory array")),
            }
        }
        Ok(this)
    }

    fn node_count(&self) -> u32 {
        u32::try_from(self.tree.node_count()).expect("node arena exceeds u32")
    }

    /// A non-directory infoblock (file, excluded stub, …) in `frame`.
    fn file_entry(&mut self, frame: &mut Frame, info: &Info) {
        let parent_node = self.tree.dir(frame.dir).node;
        let dev = info.dev.unwrap_or(frame.dev);

        // Mount-point exclusions become never-scanned directory stubs
        // (`d` line with `ex`, like the scanner's own excluded mounts).
        if let Some(reason) = info.excluded.as_deref().and_then(mount_exclusion) {
            let node = self.tree.push_node(
                &info.name,
                Kind::Dir,
                NodeFlags::EXCLUDED,
                parent_node,
                Size {
                    apparent: info.asize,
                    real: info.dsize,
                },
                info.mtime.unwrap_or(0),
            );
            self.tree.set_excluded(node, reason);
            self.excluded_dirs += 1;
            if reason == ExcludedReason::KernFs {
                self.excluded_kernfs += 1;
            }
            frame.sums.apparent += info.asize;
            frame.sums.disk += info.dsize;
            frame.sums.count += 1;
            return;
        }

        let mut flags = NodeFlags::default();
        if info.read_error {
            flags.insert(NodeFlags::ERROR);
        }
        // Non-mount exclusions (pattern, frmlink, unknown): plain entries
        // flagged excluded; the reason collapses to "otherfs" (lossy,
        // module docs).
        let excluded_entry = info.excluded.is_some();
        if excluded_entry {
            flags.insert(NodeFlags::EXCLUDED);
        }
        let kind = if info.notreg { Kind::Other } else { Kind::File };
        let is_hardlink = kind != Kind::Dir && (info.nlink.is_some_and(|n| n > 1) || info.hlnkc);
        let extra = is_hardlink
            && info
                .ino
                .is_some_and(|ino| self.inodes.contains(&(dev, ino)));
        if extra {
            flags.insert(NodeFlags::HARDLINK_EXTRA);
        }

        let node = self.tree.push_node(
            &info.name,
            kind,
            flags,
            parent_node,
            Size {
                apparent: info.asize,
                real: info.dsize,
            },
            info.mtime.unwrap_or(0),
        );
        if excluded_entry {
            self.tree.set_excluded(node, ExcludedReason::OtherFs);
        }
        if is_hardlink {
            match info.ino {
                Some(ino) => {
                    if !extra {
                        self.inodes.insert((dev, ino));
                        self.tree.mark_hardlink_first(node);
                    }
                    self.links.push(HardlinkLink {
                        node,
                        dev,
                        ino,
                        nlink: u32::try_from(info.nlink.unwrap_or(2)).unwrap_or(u32::MAX),
                    });
                }
                None => debug!(
                    name = %String::from_utf8_lossy(&info.name),
                    "hardlink flag without ino (old ncdu export): cannot deduplicate, counted fully"
                ),
            }
        }
        if !extra {
            frame.sums.apparent += info.asize;
            frame.sums.disk += info.dsize;
            frame.sums.count += 1;
        }
        if info.read_error {
            frame.sums.errors += 1;
        }
    }

    /// Enter a subdirectory: its node joins the parent's current run
    /// (closing it — grandchildren interleave next), its `DirMeta` opens.
    fn open_dir(&mut self, parent: &mut Frame, info: &Info) -> Frame {
        let parent_node = self.tree.dir(parent.dir).node;
        let dev = info.dev.unwrap_or(parent.dev);
        let node = self.tree.push_node(
            &info.name,
            Kind::Dir,
            NodeFlags::default(),
            parent_node,
            Size {
                apparent: info.asize,
                real: info.dsize,
            },
            info.mtime.unwrap_or(0),
        );
        parent.sums.apparent += info.asize;
        parent.sums.disk += info.dsize;
        parent.sums.count += 1;
        let end = self.node_count();
        self.tree.push_run(
            parent.dir,
            ChildRun {
                start: parent.run_start,
                len: end - parent.run_start,
            },
        );
        parent.run_start = end;
        let dir = self.tree.add_dir(node, Some(parent.dir), dev);
        Frame {
            dir,
            dev,
            run_start: end,
            sums: Sums::default(),
            read_error: info.read_error,
        }
    }

    /// Leave a directory: final run, aggregate delta up the ancestor
    /// chain, completion token (mirrors the scan owner's integration).
    fn close_dir(&mut self, frame: Frame) {
        let end = self.node_count();
        self.tree.push_run(
            frame.dir,
            ChildRun {
                start: frame.run_start,
                len: end - frame.run_start,
            },
        );
        let mut errors = frame.sums.errors;
        if frame.read_error {
            self.tree.mark_error(frame.dir);
            errors += 1;
        }
        self.tree.apply_delta(
            frame.dir,
            frame.sums.apparent,
            frame.sums.disk,
            frame.sums.count,
            errors,
        );
        self.tree.release_token(frame.dir);
    }
}

/// `excluded` values that mean "mount point never descended into".
fn mount_exclusion(reason: &[u8]) -> Option<ExcludedReason> {
    match reason {
        b"otherfs" | b"othfs" => Some(ExcludedReason::OtherFs),
        b"kernfs" => Some(ExcludedReason::KernFs),
        _ => None,
    }
}

/// Parse an infoblock; the opening `{` is already consumed. Unknown keys
/// are skipped wholesale.
fn parse_info<R: Read>(lexer: &mut Lexer<R>) -> Result<Info, ImportError> {
    let mut info = Info::default();
    loop {
        let token = lexer.next()?;
        let key = match token {
            Token::ObjEnd => return Ok(info),
            Token::Str(key) => key,
            _ => return Err(lexer.err("expected an object key or `}`")),
        };
        let value = lexer.next()?;
        // Known keys are all scalars; a container where a scalar belongs
        // (corrupt/hostile input) is consumed whole so the object stays
        // aligned, and the field reads as absent.
        let value = match value.kind() {
            TokenKind::ArrStart | TokenKind::ObjStart => {
                lexer.skip_value(value)?;
                Token::Null
            }
            _ => value,
        };
        match key.as_slice() {
            b"name" => match value {
                Token::Str(name) => info.name = name,
                _ => return Err(lexer.err("name must be a string")),
            },
            b"asize" => info.asize = value.as_u64().unwrap_or(0),
            b"dsize" => info.dsize = value.as_u64().unwrap_or(0),
            b"dev" => info.dev = value.as_u64(),
            b"ino" => info.ino = value.as_u64(),
            b"nlink" => info.nlink = value.as_u64(),
            b"hlnkc" => info.hlnkc = value.as_bool(),
            b"read_error" => info.read_error = value.as_bool(),
            b"notreg" => info.notreg = value.as_bool(),
            b"excluded" => {
                if let Token::Str(reason) = value {
                    info.excluded = Some(reason);
                }
            }
            b"mtime" => info.mtime = value.as_i64(),
            // uid / gid / mode (extended) and anything newer: dropped
            // (module docs); nested values are skipped structurally.
            _ => lexer.skip_value(value)?,
        }
    }
}

// ---- minimal byte-level JSON pull lexer ----

#[derive(Debug, Clone, PartialEq)]
enum Token {
    ArrStart,
    ArrEnd,
    ObjStart,
    ObjEnd,
    /// String content as raw bytes (escapes resolved; non-UTF-8 kept).
    Str(Vec<u8>),
    /// Integer: sign + magnitude (covers the full u64 range).
    Num {
        neg: bool,
        mag: u64,
    },
    /// Non-integer number (float/exponent): tolerated, value unused.
    Float,
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    ArrStart,
    ArrEnd,
    ObjStart,
    ObjEnd,
    Str,
    Num,
    Float,
    Bool,
    Null,
}

impl Token {
    fn kind(&self) -> TokenKind {
        match self {
            Token::ArrStart => TokenKind::ArrStart,
            Token::ArrEnd => TokenKind::ArrEnd,
            Token::ObjStart => TokenKind::ObjStart,
            Token::ObjEnd => TokenKind::ObjEnd,
            Token::Str(_) => TokenKind::Str,
            Token::Num { .. } => TokenKind::Num,
            Token::Float => TokenKind::Float,
            Token::Bool(_) => TokenKind::Bool,
            Token::Null => TokenKind::Null,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        match self {
            Token::Num { neg: false, mag } => Some(*mag),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Token::Num { neg: false, mag } => i64::try_from(*mag).ok(),
            Token::Num { neg: true, mag } => {
                i64::try_from(*mag).ok().map(|v| -v).or(if *mag == 1 << 63 {
                    Some(i64::MIN)
                } else {
                    None
                })
            }
            _ => None,
        }
    }

    fn as_bool(&self) -> bool {
        matches!(self, Token::Bool(true))
    }
}

struct Lexer<R: Read> {
    input: R,
    buf: Vec<u8>,
    pos: usize,
    /// Absolute offset of `buf[pos]` in the input (error reporting).
    offset: u64,
}

impl<R: Read> Lexer<R> {
    const CHUNK: usize = 64 * 1024;

    fn new(input: R) -> Self {
        Self {
            input,
            buf: Vec::new(),
            pos: 0,
            offset: 0,
        }
    }

    fn err(&self, msg: &str) -> ImportError {
        ImportError::Parse {
            offset: self.offset,
            msg: msg.to_owned(),
        }
    }

    fn peek_byte(&mut self) -> Result<Option<u8>, ImportError> {
        if self.pos >= self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            self.buf.resize(Self::CHUNK, 0);
            let got = self.input.read(&mut self.buf)?;
            self.buf.truncate(got);
            if got == 0 {
                return Ok(None);
            }
        }
        Ok(Some(self.buf[self.pos]))
    }

    fn bump(&mut self) {
        self.pos += 1;
        self.offset += 1;
    }

    fn next_byte(&mut self) -> Result<Option<u8>, ImportError> {
        let byte = self.peek_byte()?;
        if byte.is_some() {
            self.bump();
        }
        Ok(byte)
    }

    /// Next token. Commas and colons are treated as whitespace: the ncdu
    /// grammar never needs them for disambiguation, and machine-written
    /// JSON is trusted to place them correctly.
    fn next(&mut self) -> Result<Token, ImportError> {
        loop {
            let Some(byte) = self.peek_byte()? else {
                return Err(self.err("unexpected end of input"));
            };
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' | b',' | b':' => self.bump(),
                b'[' => {
                    self.bump();
                    return Ok(Token::ArrStart);
                }
                b']' => {
                    self.bump();
                    return Ok(Token::ArrEnd);
                }
                b'{' => {
                    self.bump();
                    return Ok(Token::ObjStart);
                }
                b'}' => {
                    self.bump();
                    return Ok(Token::ObjEnd);
                }
                b'"' => {
                    self.bump();
                    return self.string();
                }
                b't' => {
                    self.literal(b"true")?;
                    return Ok(Token::Bool(true));
                }
                b'f' => {
                    self.literal(b"false")?;
                    return Ok(Token::Bool(false));
                }
                b'n' => {
                    self.literal(b"null")?;
                    return Ok(Token::Null);
                }
                b'-' | b'0'..=b'9' => return self.number(),
                _ => return Err(self.err("unexpected character")),
            }
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Result<Token, ImportError> {
        let token = self.next()?;
        if token.kind() != kind {
            return Err(self.err(&format!("expected {kind:?}, found {:?}", token.kind())));
        }
        Ok(token)
    }

    fn expect_int(&mut self, what: &str) -> Result<i64, ImportError> {
        let token = self.next()?;
        token
            .as_i64()
            .ok_or_else(|| self.err(&format!("expected an integer ({what})")))
    }

    /// Consume the rest of a value whose first token is `first`.
    fn skip_value(&mut self, first: Token) -> Result<(), ImportError> {
        let mut depth = match first.kind() {
            TokenKind::ArrStart | TokenKind::ObjStart => 1u64,
            TokenKind::ArrEnd | TokenKind::ObjEnd => {
                return Err(self.err("unbalanced close bracket"));
            }
            _ => return Ok(()),
        };
        while depth > 0 {
            match self.next()?.kind() {
                TokenKind::ArrStart | TokenKind::ObjStart => depth += 1,
                TokenKind::ArrEnd | TokenKind::ObjEnd => depth -= 1,
                _ => {}
            }
        }
        Ok(())
    }

    fn literal(&mut self, expected: &[u8]) -> Result<(), ImportError> {
        for &want in expected {
            if self.next_byte()? != Some(want) {
                return Err(self.err("invalid literal"));
            }
        }
        Ok(())
    }

    fn number(&mut self) -> Result<Token, ImportError> {
        let mut text = Vec::new();
        while let Some(byte) = self.peek_byte()? {
            match byte {
                b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9' => {
                    text.push(byte);
                    self.bump();
                }
                _ => break,
            }
        }
        if text.iter().any(|b| matches!(b, b'.' | b'e' | b'E')) {
            // Floats appear only in fields the importer ignores.
            return Ok(Token::Float);
        }
        let neg = text.first() == Some(&b'-');
        let digits = if neg { &text[1..] } else { &text[..] };
        let s = std::str::from_utf8(digits).expect("digits are ASCII");
        let mag: u64 = s.parse().map_err(|_| self.err("invalid number"))?;
        Ok(Token::Num { neg, mag })
    }

    /// String body; the opening quote is consumed. Non-escape bytes pass
    /// through raw (ncdu wrote invalid UTF-8 names for years; the dump
    /// format wants raw bytes anyway).
    fn string(&mut self) -> Result<Token, ImportError> {
        let mut out = Vec::new();
        loop {
            let Some(byte) = self.next_byte()? else {
                return Err(self.err("unterminated string"));
            };
            match byte {
                b'"' => return Ok(Token::Str(out)),
                b'\\' => {
                    let Some(esc) = self.next_byte()? else {
                        return Err(self.err("unterminated escape"));
                    };
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let unit = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&unit) {
                                // High surrogate: needs a \uXXXX low half.
                                match self.low_surrogate()? {
                                    Some(low) => {
                                        let c = 0x10000
                                            + ((u32::from(unit) - 0xD800) << 10)
                                            + (u32::from(low) - 0xDC00);
                                        char::from_u32(c).unwrap_or('\u{FFFD}')
                                    }
                                    None => '\u{FFFD}',
                                }
                            } else {
                                char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}')
                            };
                            let mut utf8 = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
                        }
                        _ => return Err(self.err("unknown escape")),
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, ImportError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let Some(byte) = self.next_byte()? else {
                return Err(self.err("truncated \\u escape"));
            };
            let digit = (byte as char)
                .to_digit(16)
                .ok_or_else(|| self.err("invalid \\u escape"))?;
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }

    /// A `\uXXXX` low surrogate immediately following a high one, if
    /// present and valid.
    fn low_surrogate(&mut self) -> Result<Option<u16>, ImportError> {
        if self.peek_byte()? != Some(b'\\') {
            return Ok(None);
        }
        self.bump();
        if self.next_byte()? != Some(b'u') {
            return Err(self.err("expected \\u after high surrogate"));
        }
        let low = self.hex4()?;
        if (0xDC00..0xE000).contains(&low) {
            Ok(Some(low))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffOptions, diff_dumps};
    use crate::dump::read::DumpReader;
    use crate::dump::{DumpMeta, write_dump};
    use std::time::SystemTime;

    fn lex_all(json: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(json.as_bytes());
        let mut tokens = Vec::new();
        while let Ok(token) = lexer.next() {
            tokens.push(token);
        }
        tokens
    }

    #[test]
    fn lexer_handles_escapes_and_raw_bytes() {
        let tokens = lex_all(r#""a\"b\\c\né🧀""#);
        let Token::Str(s) = &tokens[0] else {
            panic!("expected string, got {tokens:?}");
        };
        assert_eq!(s, "a\"b\\c\né🧀".as_bytes());

        // Raw invalid UTF-8 inside a JSON string (old ncdu exports).
        let raw: &[u8] = b"[\"caf\xe9.log\"]";
        let mut lexer = Lexer::new(raw);
        assert_eq!(lexer.next().unwrap(), Token::ArrStart);
        assert_eq!(lexer.next().unwrap(), Token::Str(b"caf\xe9.log".to_vec()));
    }

    #[test]
    fn lexer_numbers_and_literals() {
        let tokens = lex_all(r#"[1, -5, 18446744073709551615, 1.5, true, false, null]"#);
        assert_eq!(
            tokens,
            vec![
                Token::ArrStart,
                Token::Num { neg: false, mag: 1 },
                Token::Num { neg: true, mag: 5 },
                Token::Num {
                    neg: false,
                    mag: u64::MAX
                },
                Token::Float,
                Token::Bool(true),
                Token::Bool(false),
                Token::Null,
                Token::ArrEnd,
            ]
        );
    }

    /// The mapping fixture: dev inheritance, an hlnkc pair across
    /// directories, an unreadable dir and file, a non-ASCII name, mount
    /// and pattern exclusions, a non-regular file, extended mtime.
    const FIXTURE: &str = r#"[1,2,{"progname":"ncdu","progver":"1.19","timestamp":1753000000},
      [{"name":"/data","dev":100,"asize":4096,"dsize":4096,"mtime":50},
       {"name":"café.log","asize":1000,"dsize":1024,"mtime":60},
       {"name":"link1","asize":500,"dsize":512,"ino":42,"nlink":2,"hlnkc":true},
       [{"name":"sub","asize":4096,"dsize":4096,"mtime":70},
        {"name":"link2","asize":500,"dsize":512,"ino":42,"nlink":2,"hlnkc":true},
        {"name":"otherdev","asize":10,"dsize":512,"dev":200}],
       [{"name":"locked","read_error":true,"asize":4096,"dsize":4096}],
       {"name":"badfile","read_error":true},
       {"name":"mnt","excluded":"othfs","asize":4096,"dsize":4096},
       {"name":"proc","excluded":"kernfs"},
       {"name":"skipped.tmp","excluded":"pattern"},
       {"name":"notregular","notreg":true,"asize":7,"dsize":512}
      ]]"#;

    fn dump_of(outcome: &ScanOutcome) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_dump(
            outcome,
            &mut bytes,
            &DumpMeta {
                timestamp: SystemTime::UNIX_EPOCH,
            },
        )
        .expect("write dump");
        bytes
    }

    #[test]
    fn fixture_imports_with_the_documented_mapping() {
        let outcome = import(FIXTURE.as_bytes()).expect("import");
        assert_eq!(outcome.root_path().as_os_str(), "/data");
        assert_eq!(outcome.hardlink_inodes, 1);
        assert_eq!(outcome.errors, 2, "locked dir + badfile");
        assert_eq!(outcome.excluded_dirs, 2, "mnt + proc");
        assert_eq!(outcome.excluded_kernfs, 1);

        // Read the emitted dump back and check the §11 mapping.
        let bytes = dump_of(&outcome);
        let mut reader = DumpReader::new(&bytes[..]).expect("read back");
        assert_eq!(reader.header().root, b"/data");
        assert_eq!(reader.header().dev, 100);
        assert!(reader.header().ordered);

        let mut blocks = Vec::new();
        while let Some(block) = reader.next_block().expect("block") {
            blocks.push(block);
        }
        assert!(reader.is_complete());

        let paths: Vec<&[u8]> = blocks.iter().map(|b| b.path.as_slice()).collect();
        assert_eq!(
            paths,
            [
                b"/data" as &[u8],
                b"/data/locked",
                b"/data/mnt",
                b"/data/proc",
                b"/data/sub",
            ],
            "DFS preorder, siblings raw-byte sorted regardless of ncdu order"
        );

        let root = &blocks[0];
        let names: Vec<&[u8]> = root.entries.iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(
            names,
            [
                b"badfile" as &[u8],
                "café.log".as_bytes(),
                b"link1",
                b"notregular",
                b"skipped.tmp",
            ]
        );
        let cafe = &root.entries[1];
        assert_eq!((cafe.apparent, cafe.disk, cafe.mtime), (1000, 1024, 60));
        let link1 = &root.entries[2];
        assert_eq!(link1.ino, Some(42));
        assert_eq!(link1.nlink, Some(2));
        let badfile = &root.entries[0];
        assert!(badfile.err);
        let notreg = &root.entries[3];
        assert_eq!(notreg.kind, None, "notreg maps to kind Other, no k letter");
        assert_eq!(notreg.disk, 512);
        let skipped = &root.entries[4];
        assert_eq!(
            skipped.ex.as_deref(),
            Some("otherfs"),
            "pattern exclusion collapses to otherfs (documented lossy)"
        );

        let locked = &blocks[1];
        assert!(locked.err, "read_error dir maps to err d line");
        assert_eq!(locked.totals.unwrap().te, 1);
        let mnt = &blocks[2];
        assert_eq!(mnt.ex.as_deref(), Some("otherfs"), "othfs normalized");
        let proc = &blocks[3];
        assert_eq!(proc.ex.as_deref(), Some("kernfs"));

        // Hardlink pair: dev inherited (both 100), same ino, canonical
        // owner is /data/link1 (smallest path) — sub's totals exclude the
        // extra link, the root counts the inode once.
        let sub = &blocks[4];
        let link2 = sub.entries.iter().find(|e| e.name == b"link2").unwrap();
        assert_eq!(link2.ino, Some(42), "inherited dev groups the pair");
        assert_eq!(
            sub.totals.unwrap(),
            crate::dump::read::Totals {
                ta: 4096 + 10,
                td: 4096 + 512,
                tn: 2,
                te: 0
            },
            "link2 is the extra link: contributes 0 to sub's totals"
        );
        let root_totals = root.totals.unwrap();
        assert_eq!(
            root_totals.td,
            4096 + 1024 + 512 + (4096 + 512) + 4096 + 4096 + 512,
            "root td: own + café + link1(once) + sub subtree + locked + mnt + notregular"
        );
        assert_eq!(root_totals.tn, 11, "inodes, hardlink extra not counted");
        assert_eq!(root_totals.te, 2);
    }

    #[test]
    fn import_then_self_diff_is_zero() {
        let outcome = import(FIXTURE.as_bytes()).expect("import");
        let bytes = dump_of(&outcome);
        let report = diff_dumps(
            DumpReader::new(&bytes[..]).expect("a"),
            DumpReader::new(&bytes[..]).expect("b"),
            &DiffOptions::default(),
        )
        .expect("diff");
        assert_eq!(report.disk_delta, 0);
        assert_eq!(report.entry_delta, 0);
        assert_eq!(
            report.counts.added + report.counts.removed + report.counts.changed(),
            0
        );
    }

    /// The round-trip promise (HANDOFF §4): a tree imported from ncdu and
    /// the same logical tree built natively must diff to zero.
    #[test]
    fn imported_tree_diffs_to_zero_against_native_writer() {
        let ncdu = r#"[1,1,{},
          [{"name":"/x","dev":7,"asize":4096,"dsize":4096,"mtime":10},
           {"name":"b.txt","asize":300,"dsize":512,"mtime":20},
           [{"name":"a","asize":4096,"dsize":4096,"mtime":30},
            {"name":"leaf","asize":100,"dsize":512,"mtime":40}]
          ]]"#;
        let imported = import(ncdu.as_bytes()).expect("import");
        let imported_dump = dump_of(&imported);

        // The same logical tree, built the way the scanner would.
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/x", Size::new(4096, 8), 10);
        let root = tree.add_dir(root_node, None, 7);
        let first = tree.push_node(
            b"b.txt",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(300, 1),
            20,
        );
        let a_node = tree.push_node(
            b"a",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            Size::new(4096, 8),
            30,
        );
        tree.push_run(
            root,
            ChildRun {
                start: first.index() as u32,
                len: 2,
            },
        );
        tree.apply_delta(root, 300 + 4096, 512 + 4096, 2, 0);
        let a = tree.add_dir(a_node, Some(root), 7);
        let leaf = tree.push_node(
            b"leaf",
            Kind::File,
            NodeFlags::default(),
            a_node,
            Size::new(100, 1),
            40,
        );
        tree.push_run(
            a,
            ChildRun {
                start: leaf.index() as u32,
                len: 1,
            },
        );
        tree.apply_delta(a, 100, 512, 1, 0);
        tree.release_token(a);
        tree.release_token(root);
        let mut native = ScanOutcome::from_tree(
            tree,
            root,
            PathBuf::from("/x"),
            Vec::new(),
            0,
            0,
            0,
            std::time::Duration::from_secs(0),
        );
        native.finalize_hardlinks();
        let native_dump = dump_of(&native);

        let report = diff_dumps(
            DumpReader::new(&imported_dump[..]).expect("imported"),
            DumpReader::new(&native_dump[..]).expect("native"),
            &DiffOptions::default(),
        )
        .expect("diff");
        assert_eq!(report.disk_delta, 0, "round-trip promise");
        assert_eq!(report.apparent_delta, 0);
        assert_eq!(report.entry_delta, 0);
        assert_eq!(report.counts, crate::diff::DiffCounts::default());
    }

    #[test]
    fn version_gates() {
        let major2 = r#"[2,0,{},[{"name":"/r"}]]"#;
        assert!(matches!(
            import(major2.as_bytes()),
            Err(ImportError::UnsupportedMajor { found: 2 })
        ));

        // Unknown minor: accepted (warn only), unknown fields ignored.
        let minor9 =
            r#"[1,9,{},[{"name":"/r","asize":1,"dsize":512,"futurefield":{"deep":[1,2]}}]]"#;
        let outcome = import(minor9.as_bytes()).expect("unknown minor imports");
        assert_eq!(outcome.entries, 1);
    }

    #[test]
    fn junk_is_rejected_with_offsets() {
        for junk in [
            "",
            "{}",
            "[true]",
            "[1,0,{}]",
            "[1,0,{},[true]]",
            "not json",
        ] {
            assert!(
                matches!(
                    import(junk.as_bytes()),
                    Err(ImportError::Parse { .. } | ImportError::UnsupportedMajor { .. })
                ),
                "{junk:?} must be rejected"
            );
        }
    }
}
