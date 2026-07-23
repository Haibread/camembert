//! Filter query language: tokenizer, parser and the filtered fold
//! (decisions `docs/design/query-decisions.md`, amendments
//! `docs/design/query-attack-a.md`).
//!
//! # Grammar (phase 1, D1 — as implemented)
//!
//! A query is whitespace-separated **terms**, implicitly ANDed. Any term
//! can be negated with a leading `!`. A term is one of:
//!
//! | term | meaning |
//! |---|---|
//! | `report` | bare word: substring match on the basename, ASCII-smartcase (all-lowercase input matches case-insensitively; any ASCII capital makes it byte-exact) |
//! | `"q(1).log"` | double-quoted: **literal** byte substring on the basename, case-sensitive — the escape hatch for names containing syntax characters |
//! | `*.log`, `data?` | contains `*`/`?`: basename glob, exactly the flat-view D4 dialect (`{}`/`[]` literal, raw bytes, case-sensitive) |
//! | `node_modules/` | trailing `/`: ancestor constraint — matches entries lying under a directory whose name matches the glob |
//! | `>100M`, `<1G` | size sugar on **disk** bytes ([`crate::size::parse_size`] dialect: `500M`, `1.5GiB`, `2gb`); only treated as size when the sigil is immediately followed by a digit, so `>readme` is a plain substring |
//! | `older:6mo`, `newer:2w` | mtime age; duration units `h`, `d`, `w`, `mo` (30.44 d), `y` (365.25 d). `older:` means *not modified since*, never *not read* |
//! | `kind:file`, `kind:dir`, `kind:symlink` | entry kind (`kind:dir` matches only not-descended directory entries — excluded mounts and stat-failed stubs — because scanned directories are never candidates, see below) |
//! | `ext:log` | sugar for the `*.log` glob (literal suffix `.log`, byte-exact) |
//! | `is:hardlink`, `is:error`, `is:excluded` | node flags |
//! | `!term` | negation of any term (`!*.o`, `!older:1y`, `!node_modules/`) |
//!
//! **Quoting rules** (D1, documented precisely): a quoted term starts with
//! `"` at the beginning of the term (after an optional `!`) and runs to the
//! next unescaped `"`; it may contain whitespace. Inside quotes exactly two
//! escapes exist: `\"` (a literal quote) and `\\` (a literal backslash);
//! any other `\x` is an error. The closing quote must end the term. A
//! quoted term is always a literal substring — no glob, no qualifier, no
//! size sugar, no smartcase.
//!
//! **Reserved sigils** (D1): `(` `)` (grouping), `|` (OR), `;` (value
//! lists) are rejected outside quotes with an error naming the future
//! feature. `<`/`>` are **not** reserved — they are spent on size sugar
//! (attack A finding 3); grouping will use `(...)`. `user:`/`group:` parse
//! but error with the D7 wording (ownership is not retained by this scan).
//!
//! **Error model**: [`parse`] never fails and never panics — it returns
//! every valid term plus a list of structured [`ParseError`]s (byte span +
//! message) for the palette to render as dim hints (the Everything rule:
//! a broken term is inert, the rest of the query still applies). A CLI
//! consumer that wants strictness rejects when `errors` is non-empty.
//!
//! # Semantics (D3/D4)
//!
//! **Candidates** are entries without directory metadata: files, symlinks,
//! devices, and dir-kind *leaves* (excluded mount points, stat-failed dir
//! stubs). Scanned directories are structure, never candidates; their own
//! inode bytes form the [`FilterResult::residual`] the UI uses to explain
//! the "matched · of scanned" gap (attack A finding 7): with an empty
//! query, `matched == root aggregates − residual`, exactly.
//!
//! **Hardlinks match by any path** (D3): every link — canonical or
//! `HARDLINK_EXTRA` — is evaluated as a candidate. A matching extra link
//! is present in the match set (a 0-byte row the UI flags "counted under
//! the canonical path") **and pulls its canonical link into the match
//! set**, where the bytes are counted once, attributed to the canonical
//! owner as everywhere else (attack A finding 1). The extra→canonical map
//! is the [`HardlinkIndex`], built lazily by the caller from the
//! [`ScanOutcome`] and invalidated with the deletion epoch.
//!
//! **Breakdown under a filter** (attack A finding 6): [`FilterResult`]
//! carries pattern-group buckets computed over the match set with the flat
//! view's exact disjoint-partition semantics (directory coverage outermost,
//! list order among name matches), so `b` under a filter shows filtered
//! groups, never silent unfiltered ones. Group verdicts come from an
//! immutable per-unique-name table precomputed before the parallel pass —
//! the flat `NameMemo` is `&mut` and deliberately **not** reused (attack A
//! finding 5).
//!
//! **Scan root and `dir/` terms** (attack A finding 11): the root node's
//! interned name is the full scan path (`/home/theo/projects`) — the dump
//! header and path reconstruction depend on it, so the stored name is
//! untouched. Ancestor terms instead match the root against the **final
//! component** of that path (`projects/` works; `theo/` does not — path
//! components above the scan root are not scanned content). A root of `/`
//! has no final component and matches no ancestor token. Pattern-group
//! *coverage* keeps using the stored name, for exact parity with
//! [`crate::flat::fold`].
//!
//! # Engine (D5)
//!
//! [`apply`] is a pure function of a frozen `&Tree`:
//!
//! 1. sequential prep, `O(unique names)`: immutable per-name verdict
//!    tables (query name terms + pattern groups);
//! 2. sequential dir pass, `O(dirs)`, topological: ancestor-term masks,
//!    pattern coverage, the dir-inode residual;
//! 3. the candidate fold, chunked by contiguous `DirId` ranges — one
//!    thread per range with a disjoint slice of the per-dir totals, a
//!    private match bitvec, private group buckets and a private top-N
//!    heap, merged in fixed order (std scoped threads; every merge is
//!    commutative, so the result is **identical for any thread count**);
//! 4. sequential absorption of pulled hardlink canonicals (sorted,
//!    deduplicated);
//! 5. sequential reverse-topological sweep summing per-dir direct totals
//!    into filtered subtree totals.
//!
//! The result is stamped with `(query fingerprint, deletion epoch)` so
//! stale generations never render (the `FlatSummary.epoch` pattern).

use std::time::Instant;

use rustc_hash::{FxHashMap, FxHasher};
use tracing::debug;

use crate::flat::{GroupTotal, PatternKind, PatternSet, RestTotal, TopFile, TopHeap, glob_match};
use crate::scan::ScanOutcome;
use crate::size::{Size, parse_size};
use crate::tree::{DirId, Kind, NodeFlags, NodeId, Tree};

/// Maximum number of terms in one query. Bounds the per-name verdict
/// bitmask (`u64`) and keeps pathological inputs cheap; real queries have
/// a handful of terms.
pub const MAX_TERMS: usize = 64;

/// "No group" sentinel in coverage/verdict tables (distinct from every
/// [`crate::flat::GroupId`], which caps at `MAX_GROUPS`).
const NO_GROUP: u16 = u16::MAX;

// Duration units (fd/humantime conventions: a month is 30.44 days, a year
// 365.25 days).
const HOUR_SECS: f64 = 3_600.0;
const DAY_SECS: f64 = 86_400.0;
const WEEK_SECS: f64 = 604_800.0;
const MONTH_SECS: f64 = 2_630_016.0;
const YEAR_SECS: f64 = 31_557_600.0;

// ---------------------------------------------------------------------------
// Parsed representation
// ---------------------------------------------------------------------------

/// Byte range into the original input, for palette error rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Machine-readable category of a [`ParseError`] (the message is the
/// human-facing text; the kind lets the palette style/route hints).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// `(` `)` `;` `|` outside quotes — the future feature is named in the
    /// message.
    ReservedSigil,
    /// `word:value` with an alphabetic `word` no qualifier owns.
    UnknownQualifier,
    /// `user:`/`group:` — ownership is not retained by this scan (D7).
    OwnerNotRetained,
    /// A qualifier or sugar value that does not parse (size, duration,
    /// kind/is value, ext value).
    InvalidValue,
    /// A qualifier with no value (`older:`), an empty pattern (`/`), an
    /// empty quote (`""`), or a dangling `!`.
    EmptyTerm,
    /// Quoting problem: unterminated quote, unknown escape, or trailing
    /// bytes after the closing quote.
    Quote,
    /// `/` anywhere but a single trailing position: path globs are a
    /// future feature.
    PathPattern,
    /// `!!` — double negation is not supported.
    DoubleNegation,
    /// More than [`MAX_TERMS`] terms; the extras were ignored.
    TooManyTerms,
}

/// One structured parse diagnostic: where ([`Span`], byte offsets into the
/// input) and why. The offending term is **inert** — every other term of
/// the query still parsed into [`Parsed::query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub span: Span,
    pub message: String,
}

/// Entry kinds selectable with `kind:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KindTerm {
    File,
    Dir,
    Symlink,
}

/// Node flags selectable with `is:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsTerm {
    Hardlink,
    Error,
    Excluded,
}

/// One parsed predicate (see the module docs for the grammar table).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Pred {
    /// Bare word: smartcase substring on the basename.
    Substring {
        needle: Vec<u8>,
        /// ASCII-case-insensitive iff the input had no ASCII capital.
        case_insensitive: bool,
    },
    /// Double-quoted term: literal, case-sensitive substring.
    Literal { needle: Vec<u8> },
    /// Basename glob (`*`/`?` present), flat-view D4 dialect.
    Glob { pattern: Vec<u8> },
    /// `dir/`: some ancestor directory's name matches this glob.
    Ancestor { pattern: Vec<u8> },
    /// `>SIZE`: disk bytes strictly greater.
    SizeOver(u64),
    /// `<SIZE`: disk bytes strictly smaller.
    SizeUnder(u64),
    /// `older:DUR`: mtime at least this many seconds in the past.
    OlderThan(u64),
    /// `newer:DUR`: mtime at most this many seconds in the past.
    NewerThan(u64),
    /// `kind:VALUE`.
    Kind(KindTerm),
    /// `ext:VALUE`: basename ends with `.VALUE` (byte-exact).
    Ext { ext: Vec<u8> },
    /// `is:VALUE`.
    Is(IsTerm),
}

/// One term of a query: a predicate, optionally negated, with its input
/// span (for the palette's parse echo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub negated: bool,
    pub pred: Pred,
    pub span: Span,
}

/// A parsed query: the conjunction of its terms. Empty ⇒ matches every
/// candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    terms: Vec<Term>,
}

impl Query {
    pub fn terms(&self) -> &[Term] {
        &self.terms
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Stable fingerprint of the canonicalized query: spans (and therefore
    /// whitespace differences) are excluded, so `"a  b"` and `"a b"` hash
    /// identically. Keys a [`FilterResult`] together with the deletion
    /// epoch (D5).
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = FxHasher::default();
        self.terms.len().hash(&mut hasher);
        for term in &self.terms {
            term.negated.hash(&mut hasher);
            term.pred.hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Output of [`parse`]: the valid terms plus every diagnostic. Terms that
/// failed to parse are absent from `query` (inert), per the module docs'
/// error model.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub query: Query,
    pub errors: Vec<ParseError>,
}

// ---------------------------------------------------------------------------
// Tokenizer + parser
// ---------------------------------------------------------------------------

/// Parse a query string. Never fails, never panics: broken terms become
/// [`ParseError`]s (span + message) and the remaining terms still form the
/// query. See the module docs for the grammar.
pub fn parse(input: &str) -> Parsed {
    let bytes = input.as_bytes();
    let mut terms: Vec<Term> = Vec::new();
    let mut errors: Vec<ParseError> = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;

        let mut negated = false;
        if bytes[i] == b'!' {
            negated = true;
            i += 1;
            if i < bytes.len() && bytes[i] == b'!' {
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                errors.push(ParseError {
                    kind: ParseErrorKind::DoubleNegation,
                    span: Span { start, end: i },
                    message: "double negation (`!!`) is not supported".to_owned(),
                });
                continue;
            }
        }

        if i < bytes.len() && bytes[i] == b'"' {
            match scan_quoted(input, i) {
                Ok((needle, end)) => {
                    if end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                        let mut j = end;
                        while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        errors.push(ParseError {
                            kind: ParseErrorKind::Quote,
                            span: Span { start, end: j },
                            message: "a quoted term must end at the closing quote".to_owned(),
                        });
                        i = j;
                        continue;
                    }
                    if needle.is_empty() {
                        errors.push(ParseError {
                            kind: ParseErrorKind::EmptyTerm,
                            span: Span { start, end },
                            message: "empty quoted term".to_owned(),
                        });
                    } else {
                        terms.push(Term {
                            negated,
                            pred: Pred::Literal { needle },
                            span: Span { start, end },
                        });
                    }
                    i = end;
                }
                Err(err) => {
                    errors.push(err);
                    i = bytes.len();
                }
            }
            continue;
        }

        let mut j = i;
        while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j == i {
            // A lone `!` (the loop above consumed nothing).
            errors.push(ParseError {
                kind: ParseErrorKind::EmptyTerm,
                span: Span { start, end: j },
                message: "`!` needs a term to negate".to_owned(),
            });
            i = j;
            continue;
        }
        let span = Span { start, end: j };
        // Split points are ASCII (whitespace / `!`), so the slice is valid
        // UTF-8 at both ends.
        match parse_body(&input[i..j], i, span) {
            Ok(pred) => terms.push(Term {
                negated,
                pred,
                span,
            }),
            Err(err) => errors.push(err),
        }
        i = j;
    }

    if terms.len() > MAX_TERMS {
        errors.push(ParseError {
            kind: ParseErrorKind::TooManyTerms,
            span: Span {
                start: 0,
                end: input.len(),
            },
            message: format!(
                "too many terms ({} > {MAX_TERMS}); the extra terms are ignored",
                terms.len()
            ),
        });
        terms.truncate(MAX_TERMS);
    }

    Parsed {
        query: Query { terms },
        errors,
    }
}

/// Scan a double-quoted term starting at `open` (the `"` byte). Returns
/// the unescaped bytes and the index just past the closing quote.
fn scan_quoted(input: &str, open: usize) -> Result<(Vec<u8>, usize), ParseError> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut k = open + 1;
    while k < bytes.len() {
        match bytes[k] {
            b'"' => return Ok((out, k + 1)),
            b'\\' => match bytes.get(k + 1) {
                Some(b'"') => {
                    out.push(b'"');
                    k += 2;
                }
                Some(b'\\') => {
                    out.push(b'\\');
                    k += 2;
                }
                Some(_) => {
                    // Span the backslash plus the following character
                    // (which may be multi-byte).
                    let next_len = input[k + 1..].chars().next().map_or(1, char::len_utf8);
                    return Err(ParseError {
                        kind: ParseErrorKind::Quote,
                        span: Span {
                            start: k,
                            end: k + 1 + next_len,
                        },
                        message: format!(
                            "unknown escape `{}` — only `\\\"` and `\\\\` are recognized inside quotes",
                            &input[k..k + 1 + next_len]
                        ),
                    });
                }
                None => break,
            },
            b => {
                out.push(b);
                k += 1;
            }
        }
    }
    Err(ParseError {
        kind: ParseErrorKind::Quote,
        span: Span {
            start: open,
            end: bytes.len(),
        },
        message: "unterminated quote".to_owned(),
    })
}

/// Parse one unquoted term body (never empty). `offset` is the body's byte
/// offset in the input, `span` the whole term's span.
fn parse_body(body: &str, offset: usize, span: Span) -> Result<Pred, ParseError> {
    // Reserved sigils first: their presence anywhere in an unquoted term
    // is an error naming the future feature (D1).
    for (pos, ch) in body.char_indices() {
        let message = match ch {
            '(' | ')' => format!(
                "`{ch}` is reserved: grouping arrives with expressions — quote the term (\"…\") to match a literal `{ch}`"
            ),
            '|' => "`|` is reserved: OR arrives with expressions — quote the term (\"…\") to match a literal `|`"
                .to_owned(),
            ';' => "`;` is reserved: value lists (`ext:log;tmp`) arrive with expressions — quote the term (\"…\") to match a literal `;`"
                .to_owned(),
            _ => continue,
        };
        return Err(ParseError {
            kind: ParseErrorKind::ReservedSigil,
            span: Span {
                start: offset + pos,
                end: offset + pos + 1,
            },
            message,
        });
    }

    let bytes = body.as_bytes();

    // Size sugar, guarded by an immediately following digit (attack A
    // finding 8): `>100M` is a size, `>readme` is a substring.
    if (bytes[0] == b'>' || bytes[0] == b'<') && bytes.len() > 1 && bytes[1].is_ascii_digit() {
        return match parse_size(&body[1..]) {
            Ok(limit) if bytes[0] == b'>' => Ok(Pred::SizeOver(limit)),
            Ok(limit) => Ok(Pred::SizeUnder(limit)),
            Err(err) => Err(ParseError {
                kind: ParseErrorKind::InvalidValue,
                span,
                message: format!("invalid size in {body:?}: {err}"),
            }),
        };
    }

    // Trailing `/`: ancestor constraint.
    if let Some(stripped) = body.strip_suffix('/') {
        if stripped.is_empty() {
            return Err(ParseError {
                kind: ParseErrorKind::EmptyTerm,
                span,
                message: "empty directory pattern before `/`".to_owned(),
            });
        }
        if stripped.contains('/') {
            return Err(path_pattern_error(span));
        }
        return Ok(Pred::Ancestor {
            pattern: stripped.as_bytes().to_vec(),
        });
    }
    if body.contains('/') {
        return Err(path_pattern_error(span));
    }

    // Qualifier: `word:value` with an all-alphabetic word.
    if let Some(colon) = body.find(':') {
        let name = &body[..colon];
        if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphabetic()) {
            return parse_qualifier(name, &body[colon + 1..], body, span);
        }
    }

    // Glob or smartcase substring.
    if body.contains(['*', '?']) {
        return Ok(Pred::Glob {
            pattern: bytes.to_vec(),
        });
    }
    let case_insensitive = !bytes.iter().any(u8::is_ascii_uppercase);
    Ok(Pred::Substring {
        needle: bytes.to_vec(),
        case_insensitive,
    })
}

fn path_pattern_error(span: Span) -> ParseError {
    ParseError {
        kind: ParseErrorKind::PathPattern,
        span,
        message: "path patterns (`src/**/*.c`) arrive with expressions — only the trailing-`/` ancestor form is supported today"
            .to_owned(),
    }
}

/// Parse a recognized-or-reserved `name:value` qualifier.
fn parse_qualifier(name: &str, value: &str, body: &str, span: Span) -> Result<Pred, ParseError> {
    let lower = name.to_ascii_lowercase();
    let missing = |what: &str| ParseError {
        kind: ParseErrorKind::EmptyTerm,
        span,
        message: format!("`{lower}:` needs a value: {what}"),
    };
    let invalid = |message: String| ParseError {
        kind: ParseErrorKind::InvalidValue,
        span,
        message,
    };
    match lower.as_str() {
        "kind" => match value.to_ascii_lowercase().as_str() {
            "" => Err(missing("file, dir or symlink")),
            "file" => Ok(Pred::Kind(KindTerm::File)),
            "dir" => Ok(Pred::Kind(KindTerm::Dir)),
            "symlink" => Ok(Pred::Kind(KindTerm::Symlink)),
            other => Err(invalid(format!(
                "`kind:` expects file, dir or symlink (got `{other}`)"
            ))),
        },
        "is" => match value.to_ascii_lowercase().as_str() {
            "" => Err(missing("hardlink, error or excluded")),
            "hardlink" => Ok(Pred::Is(IsTerm::Hardlink)),
            "error" => Ok(Pred::Is(IsTerm::Error)),
            "excluded" => Ok(Pred::Is(IsTerm::Excluded)),
            other => Err(invalid(format!(
                "`is:` expects hardlink, error or excluded (got `{other}`)"
            ))),
        },
        "ext" => {
            let ext = value.strip_prefix('.').unwrap_or(value);
            if ext.is_empty() {
                return Err(missing("an extension, e.g. ext:log"));
            }
            if ext.contains(['*', '?']) {
                return Err(invalid(format!(
                    "`ext:` takes a literal extension — use a glob term (`*.{ext}`) for wildcards"
                )));
            }
            Ok(Pred::Ext {
                ext: ext.as_bytes().to_vec(),
            })
        }
        "older" | "newer" => {
            if value.is_empty() {
                return Err(missing(&format!(
                    "a duration, e.g. {lower}:6mo (units: h, d, w, mo, y)"
                )));
            }
            let secs = parse_duration(value)
                .map_err(|reason| invalid(format!("invalid duration in {body:?}: {reason}")))?;
            if lower == "older" {
                Ok(Pred::OlderThan(secs))
            } else {
                Ok(Pred::NewerThan(secs))
            }
        }
        // D7: reserved with the "not retained" wording, naming the future
        // capability.
        "user" | "group" => Err(ParseError {
            kind: ParseErrorKind::OwnerNotRetained,
            span,
            message: format!(
                "`{lower}:` filters need file ownership, which is not retained by this scan; owner predicates arrive once uid/gid capture lands"
            ),
        }),
        _ => Err(ParseError {
            kind: ParseErrorKind::UnknownQualifier,
            span,
            message: format!(
                "unknown qualifier `{lower}:` — term ignored; quote it (\"{body}\") to match literally"
            ),
        }),
    }
}

/// Parse a duration like `6mo`, `2w`, `36h`, `1.5y` into seconds.
fn parse_duration(value: &str) -> Result<u64, String> {
    let lower = value.to_ascii_lowercase();
    let split = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(split);
    let amount: f64 = number
        .parse()
        .map_err(|_| format!("`{number}` is not a number"))?;
    if !amount.is_finite() || amount < 0.0 {
        return Err(format!("`{number}` is not a non-negative number"));
    }
    let unit_secs = match unit {
        "h" => HOUR_SECS,
        "d" => DAY_SECS,
        "w" => WEEK_SECS,
        "mo" => MONTH_SECS,
        "y" => YEAR_SECS,
        "" => return Err("missing unit (h, d, w, mo, y)".to_owned()),
        "m" => return Err("ambiguous unit `m`: use `mo` for months".to_owned()),
        other => return Err(format!("unknown unit `{other}` (h, d, w, mo, y)")),
    };
    let secs = amount * unit_secs;
    if secs >= u64::MAX as f64 {
        Ok(u64::MAX)
    } else {
        Ok(secs.round() as u64)
    }
}

// ---------------------------------------------------------------------------
// Hardlink reverse map (D3)
// ---------------------------------------------------------------------------

/// Extra-link → canonical-link map for hardlink membership-by-any-path
/// (D3). Build it **lazily on first filter use** from the scan outcome and
/// cache it; rebuild when the deletion epoch changes (a removal can
/// tombstone a canonical link). [`apply`] additionally skips tombstoned
/// canonicals, so a stale index degrades to missing bytes for
/// just-deleted inodes, never to counting deleted ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HardlinkIndex {
    extra_to_canonical: FxHashMap<NodeId, NodeId>,
    epoch: u64,
}

impl HardlinkIndex {
    /// Build the reverse map. Requires canonical attribution
    /// ([`ScanOutcome::finalize_hardlinks`]) — both CLI modes run it right
    /// after the scan. `epoch` is the caller's deletion epoch at build
    /// time (see [`HardlinkIndex::epoch`]).
    pub fn build(outcome: &ScanOutcome, epoch: u64) -> Self {
        debug_assert!(
            outcome.hardlinks_finalized() || outcome.hardlink_links().is_empty(),
            "HardlinkIndex::build requires finalize_hardlinks() (canonical attribution)"
        );
        let tree = outcome.tree();
        let mut groups: FxHashMap<(u64, u64), (Option<NodeId>, Vec<NodeId>)> = FxHashMap::default();
        for link in outcome.hardlink_links() {
            if tree.is_removed(link.node) {
                continue;
            }
            let entry = groups.entry((link.dev, link.ino)).or_default();
            if tree
                .node(link.node)
                .flags()
                .contains(NodeFlags::HARDLINK_EXTRA)
            {
                entry.1.push(link.node);
            } else {
                entry.0 = Some(link.node);
            }
        }
        let mut extra_to_canonical = FxHashMap::default();
        for (canonical, extras) in groups.into_values() {
            // A group whose canonical link was deleted keeps its extras
            // unmapped: the aggregates already dropped those bytes.
            let Some(canonical) = canonical else { continue };
            for extra in extras {
                extra_to_canonical.insert(extra, canonical);
            }
        }
        debug!(
            extras = extra_to_canonical.len(),
            epoch, "hardlink reverse map built"
        );
        Self {
            extra_to_canonical,
            epoch,
        }
    }

    /// An empty index, for trees known to have no hardlink extras.
    pub fn empty(epoch: u64) -> Self {
        Self {
            extra_to_canonical: FxHashMap::default(),
            epoch,
        }
    }

    /// The deletion epoch this index was built against. When the caller's
    /// epoch has moved past it, rebuild before the next [`apply`].
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The canonical (counted) link of an extra link, if mapped. The UI
    /// uses this to render the "counted under `<canonical path>`" flag on
    /// matched 0-byte extra rows.
    pub fn canonical_of(&self, extra: NodeId) -> Option<NodeId> {
        self.extra_to_canonical.get(&extra).copied()
    }

    pub fn len(&self) -> usize {
        self.extra_to_canonical.len()
    }

    pub fn is_empty(&self) -> bool {
        self.extra_to_canonical.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Filtered fold
// ---------------------------------------------------------------------------

/// Knobs of one [`apply`] call.
#[derive(Debug, Clone, Copy)]
pub struct ApplyOptions {
    /// Top-N cap over the match set (the flat view's `flat_cap`).
    pub cap: usize,
    /// The caller's deletion epoch; stamped onto the result unchanged.
    pub epoch: u64,
    /// "Now" in unix seconds for `older:`/`newer:` — passed in (never read
    /// from the clock here) so results are reproducible.
    pub now_unix: i64,
    /// Worker threads for the candidate fold. `0` or `1` runs
    /// single-threaded; any count produces an identical result.
    pub threads: usize,
}

/// Filtered subtree totals of one directory (matched candidates only —
/// directory inodes are excluded by construction, D4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilteredDirTotals {
    /// Σ apparent bytes of matched candidates in the subtree.
    pub apparent: u64,
    /// Σ disk bytes of matched candidates in the subtree.
    pub disk: u64,
    /// Matched candidate count (hardlink bytes/entries counted once, at
    /// the canonical link).
    pub entries: u64,
}

impl FilteredDirTotals {
    fn add_size(&mut self, size: Size) {
        self.apparent += size.apparent;
        self.disk += size.real;
        self.entries += 1;
    }

    fn add(&mut self, other: FilteredDirTotals) {
        self.apparent += other.apparent;
        self.disk += other.disk;
        self.entries += other.entries;
    }
}

/// The directory-inode residual (D4 / attack A finding 7): bytes of
/// scanned directories' own inodes, which no query can ever match. The UI
/// renders the honest denominator from it: `scanned − residual =
/// filterable`, and an empty query matches exactly `filterable`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirResidual {
    pub apparent: u64,
    pub disk: u64,
    /// Live scanned directories contributing to the residual.
    pub dirs: u64,
}

/// Dense per-node match bitvec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSet {
    words: Vec<u64>,
}

impl MatchSet {
    fn new(nodes: usize) -> Self {
        Self {
            words: vec![0; nodes.div_ceil(64)],
        }
    }

    fn set(&mut self, id: NodeId) {
        let i = id.index();
        self.words[i / 64] |= 1 << (i % 64);
    }

    /// Whether `id` is in the match set. Matching hardlink extra links are
    /// present (0 bytes; flag them via [`HardlinkIndex::canonical_of`]),
    /// and canonical links pulled in by a matching extra are too.
    pub fn contains(&self, id: NodeId) -> bool {
        let i = id.index();
        self.words
            .get(i / 64)
            .is_some_and(|w| w >> (i % 64) & 1 != 0)
    }

    /// Number of set bits (matching rows, extras included).
    pub fn count(&self) -> u64 {
        self.words.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    fn or_with(&mut self, other: &MatchSet) {
        debug_assert_eq!(self.words.len(), other.words.len());
        for (word, o) in self.words.iter_mut().zip(&other.words) {
            *word |= o;
        }
    }
}

/// Result of one filtered fold: everything the UI needs to render a
/// filtered cockpit without touching the arena's own aggregates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterResult {
    /// Per-`DirId` filtered subtree totals (side table, arena untouched).
    dir_totals: Vec<FilteredDirTotals>,
    /// Per-node match bitvec (extras and pulled canonicals included).
    pub matched: MatchSet,
    /// Matched totals (== the root's [`FilterResult::dir_total`]).
    pub matched_apparent: u64,
    pub matched_disk: u64,
    pub matched_entries: u64,
    /// Matching `HARDLINK_EXTRA` rows: present in [`FilterResult::matched`]
    /// but contributing 0 bytes (counted at the canonical link).
    pub matched_extra_links: u64,
    /// The dir-inode residual, for the honest "of X filterable" subtitle.
    pub residual: DirResidual,
    /// Pattern-group totals over the match set ([`PatternSet`] order,
    /// empty groups included) — the breakdown view under a filter.
    pub groups: Vec<GroupTotal>,
    /// Matched candidates claimed by no group.
    pub rest: RestTotal,
    /// Top-N matched files (disk desc, `NodeId` asc — the flat ordering).
    pub top_files: Vec<TopFile>,
    /// More matched files than the cap were eligible.
    pub truncated: bool,
    /// [`Query::fingerprint`] of the applied query.
    pub query_hash: u64,
    /// The deletion epoch this result was computed for.
    pub epoch: u64,
}

impl FilterResult {
    /// Filtered subtree totals of a directory (zero for out-of-range ids).
    pub fn dir_total(&self, dir: DirId) -> FilteredDirTotals {
        self.dir_totals
            .get(dir.index())
            .copied()
            .unwrap_or_default()
    }

    /// The whole per-`DirId` side table, indexed like the dir arena.
    pub fn dir_totals(&self) -> &[FilteredDirTotals] {
        &self.dir_totals
    }
}

/// A compiled term: how to evaluate one predicate against a candidate.
#[derive(Debug, Clone, Copy)]
enum TermEval {
    /// Bit `slot` of the per-name verdict word.
    NameBit(u32),
    /// Bit `slot` of the per-dir ancestor mask.
    AncestorBit(u32),
    SizeOver(u64),
    SizeUnder(u64),
    /// `older:` — mtime at or before the cutoff.
    MtimeAtMost(i64),
    /// `newer:` — mtime at or after the cutoff.
    MtimeAtLeast(i64),
    KindIs(Kind),
    IsHardlink,
    IsError,
    IsExcluded,
}

#[derive(Debug, Clone, Copy)]
struct CompiledTerm {
    eval: TermEval,
    negated: bool,
}

/// A name predicate destined for the per-unique-name verdict table.
enum NamePred {
    Substring {
        needle: Vec<u8>,
        case_insensitive: bool,
    },
    Glob(Vec<u8>),
    /// `ext:` sugar: `.ext` suffix, byte-exact.
    Suffix(Vec<u8>),
}

impl NamePred {
    fn matches(&self, name: &[u8]) -> bool {
        match self {
            Self::Substring {
                needle,
                case_insensitive,
            } => substring_match(name, needle, *case_insensitive),
            Self::Glob(pattern) => glob_match(pattern, name),
            Self::Suffix(dotted) => name.ends_with(dotted),
        }
    }
}

/// Byte substring search, optionally ASCII-case-insensitive.
fn substring_match(haystack: &[u8], needle: &[u8], case_insensitive: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        if case_insensitive {
            window.eq_ignore_ascii_case(needle)
        } else {
            window == needle
        }
    })
}

/// The final path component of the root's stored full-path name (attack A
/// finding 11): `/home/theo/projects` → `projects`, `.` → `.`, `/` → `/`
/// (a root of `/` has no component and stays unmatchable — `/` cannot
/// appear in an ancestor pattern).
fn final_component(name: &[u8]) -> &[u8] {
    name.rsplit(|&b| b == b'/')
        .find(|component| !component.is_empty())
        .unwrap_or(name)
}

/// Everything the candidate fold reads. Immutable — shared by every worker
/// thread (the flat `NameMemo`'s lazy `&mut` fill is exactly what this
/// replaces, attack A finding 5).
struct EvalCtx<'a> {
    tree: &'a Tree,
    terms: &'a [CompiledTerm],
    /// Per-unique-name verdict word (bit = name-term slot). Empty when the
    /// query has no name terms.
    name_bits: &'a [u64],
    /// Per-dir ancestor-term mask (bit = ancestor-term slot), covering the
    /// chain root..=dir.
    anc_mask: &'a [u64],
    /// Per-dir pattern coverage (flat D1 semantics), [`NO_GROUP`] when
    /// uncovered.
    coverage: &'a [u16],
    /// Per-unique-name pattern verdicts (empty when no patterns).
    dir_verdict: &'a [u16],
    file_verdict: &'a [u16],
    hardlinks: &'a HardlinkIndex,
    group_count: usize,
    cap: usize,
    node_count: usize,
}

impl EvalCtx<'_> {
    /// Whether a candidate satisfies every term. `dir_mask` is the
    /// ancestor mask of its containing directory.
    fn matches(&self, id: NodeId, dir_mask: u64) -> bool {
        let node = self.tree.node(id);
        let name_bits = self
            .name_bits
            .get(node.name_ref().0 as usize)
            .copied()
            .unwrap_or(0);
        for term in self.terms {
            let holds = match term.eval {
                TermEval::NameBit(slot) => name_bits >> slot & 1 != 0,
                TermEval::AncestorBit(slot) => dir_mask >> slot & 1 != 0,
                TermEval::SizeOver(limit) => node.size().real > limit,
                TermEval::SizeUnder(limit) => node.size().real < limit,
                TermEval::MtimeAtMost(cutoff) => node.mtime() <= cutoff,
                TermEval::MtimeAtLeast(cutoff) => node.mtime() >= cutoff,
                TermEval::KindIs(kind) => node.kind() == kind,
                TermEval::IsHardlink => self.tree.is_hardlink(id),
                TermEval::IsError => node.flags().contains(NodeFlags::ERROR),
                TermEval::IsExcluded => node.flags().contains(NodeFlags::EXCLUDED),
            };
            if holds == term.negated {
                return false;
            }
        }
        true
    }

    /// Disjoint-partition group of a matched candidate (flat D1: coverage
    /// outermost, else first name match of the right kind).
    fn group_of(&self, parent_cov: u16, id: NodeId, kind: Kind) -> u16 {
        if self.group_count == 0 {
            return NO_GROUP;
        }
        if parent_cov != NO_GROUP {
            return parent_cov;
        }
        let name_id = self.tree.node(id).name_ref().0 as usize;
        let table = if kind.is_dir() {
            self.dir_verdict
        } else {
            self.file_verdict
        };
        table.get(name_id).copied().unwrap_or(NO_GROUP)
    }
}

/// One worker's private accumulation state.
struct Partial {
    bits: MatchSet,
    groups: Vec<FilteredDirTotals>,
    rest: FilteredDirTotals,
    top: TopHeap,
    /// Canonical links pulled in by matching extra links (D3), absorbed
    /// sequentially after the merge.
    pulled: Vec<NodeId>,
    extra_links: u64,
}

impl Partial {
    fn new(ctx: &EvalCtx<'_>) -> Self {
        Self {
            bits: MatchSet::new(ctx.node_count),
            groups: vec![FilteredDirTotals::default(); ctx.group_count],
            rest: FilteredDirTotals::default(),
            top: TopHeap::new(ctx.cap),
            pulled: Vec::new(),
            extra_links: 0,
        }
    }

    fn bucket_mut(&mut self, group: u16) -> &mut FilteredDirTotals {
        if group == NO_GROUP {
            &mut self.rest
        } else {
            &mut self.groups[group as usize]
        }
    }

    /// Merge another worker's partial. Every operation is commutative
    /// (OR, adds, bounded-heap union), so the merged result is identical
    /// for any thread count and any merge order; merging in thread order
    /// keeps it obviously fixed.
    fn merge(&mut self, other: Partial) {
        self.bits.or_with(&other.bits);
        for (bucket, o) in self.groups.iter_mut().zip(&other.groups) {
            bucket.add(*o);
        }
        self.rest.add(other.rest);
        self.top.merge(other.top);
        self.pulled.extend(other.pulled);
        self.extra_links += other.extra_links;
    }
}

/// Fold the candidates of the contiguous dir range starting at `lo`
/// (length = `direct.len()`), accumulating per-dir direct totals into the
/// caller's disjoint `direct` slice and everything else into a private
/// [`Partial`].
fn fold_range(ctx: &EvalCtx<'_>, lo: usize, direct: &mut [FilteredDirTotals]) -> Partial {
    let mut partial = Partial::new(ctx);
    for (offset, slot) in direct.iter_mut().enumerate() {
        let dir_index = lo + offset;
        let dir = DirId::from_raw(dir_index as u32);
        let mask = ctx.anc_mask[dir_index];
        let cov = ctx.coverage[dir_index];
        for child in ctx.tree.children(dir) {
            if ctx.tree.dir_of(child).is_some() {
                continue; // scanned directory: structure, not a candidate.
            }
            if !ctx.matches(child, mask) {
                continue;
            }
            partial.bits.set(child);
            let node = ctx.tree.node(child);
            if node.flags().contains(NodeFlags::HARDLINK_EXTRA) {
                // Present (0 bytes) + pulls its canonical link (D3).
                partial.extra_links += 1;
                if let Some(canonical) = ctx.hardlinks.canonical_of(child) {
                    partial.pulled.push(canonical);
                }
                continue;
            }
            let size = node.size();
            let kind = node.kind();
            slot.add_size(size);
            partial
                .bucket_mut(ctx.group_of(cov, child, kind))
                .add_size(size);
            if kind == Kind::File {
                partial.top.offer(
                    size.real,
                    child,
                    ctx.tree.is_hardlink(child),
                    ctx.tree.name(child),
                );
            }
        }
    }
    partial
}

/// Absorb pulled hardlink canonicals (D3): sorted and deduplicated, each
/// not-already-matched, not-removed canonical joins the match set with its
/// bytes attributed at its own location.
fn absorb_pulled(ctx: &EvalCtx<'_>, partial: &mut Partial, direct: &mut [FilteredDirTotals]) {
    let mut pulled = std::mem::take(&mut partial.pulled);
    pulled.sort_unstable_by_key(|id| id.index());
    pulled.dedup();
    for canonical in pulled {
        if partial.bits.contains(canonical) || ctx.tree.is_removed(canonical) {
            continue;
        }
        partial.bits.set(canonical);
        let node = ctx.tree.node(canonical);
        debug_assert!(
            !node.flags().contains(NodeFlags::HARDLINK_EXTRA),
            "hardlink reverse map points at an extra link"
        );
        let parent_dir = ctx
            .tree
            .dir_of(node.parent())
            .expect("a canonical link's parent is a scanned directory");
        let size = node.size();
        let kind = node.kind();
        direct[parent_dir.index()].add_size(size);
        let cov = ctx.coverage[parent_dir.index()];
        partial
            .bucket_mut(ctx.group_of(cov, canonical, kind))
            .add_size(size);
        if kind == Kind::File {
            partial.top.offer(
                size.real,
                canonical,
                ctx.tree.is_hardlink(canonical),
                ctx.tree.name(canonical),
            );
        }
    }
}

/// Run the filtered fold: re-derive every directory total over the
/// candidates matching `query`, per the module docs' semantics. Pure
/// function of the frozen arena — deterministic for any
/// [`ApplyOptions::threads`].
///
/// `hardlinks` must have been built against the same tree state (rebuild
/// it when the deletion epoch moves; see [`HardlinkIndex`]). Pass
/// [`HardlinkIndex::empty`] for trees without hardlinks.
pub fn apply(
    tree: &Tree,
    query: &Query,
    patterns: &PatternSet,
    hardlinks: &HardlinkIndex,
    opts: &ApplyOptions,
) -> FilterResult {
    let started = Instant::now();
    let query_hash = query.fingerprint();
    let dir_count = tree.dir_count();
    let node_count = tree.node_count();
    let group_count = patterns.len();

    if dir_count == 0 {
        return FilterResult {
            dir_totals: Vec::new(),
            matched: MatchSet::new(node_count),
            matched_apparent: 0,
            matched_disk: 0,
            matched_entries: 0,
            matched_extra_links: 0,
            residual: DirResidual::default(),
            groups: empty_groups(patterns),
            rest: RestTotal::default(),
            top_files: Vec::new(),
            truncated: false,
            query_hash,
            epoch: opts.epoch,
        };
    }

    // Compile: assign name/ancestor slots, resolve age cutoffs.
    let mut terms: Vec<CompiledTerm> = Vec::with_capacity(query.terms.len());
    let mut name_preds: Vec<(u32, NamePred)> = Vec::new();
    let mut anc_preds: Vec<(u32, Vec<u8>)> = Vec::new();
    for term in &query.terms {
        let eval = match &term.pred {
            Pred::Substring {
                needle,
                case_insensitive,
            } => name_slot(
                &mut name_preds,
                NamePred::Substring {
                    needle: needle.clone(),
                    case_insensitive: *case_insensitive,
                },
            ),
            Pred::Literal { needle } => name_slot(
                &mut name_preds,
                NamePred::Substring {
                    needle: needle.clone(),
                    case_insensitive: false,
                },
            ),
            Pred::Glob { pattern } => name_slot(&mut name_preds, NamePred::Glob(pattern.clone())),
            Pred::Ext { ext } => {
                let mut dotted = Vec::with_capacity(ext.len() + 1);
                dotted.push(b'.');
                dotted.extend_from_slice(ext);
                name_slot(&mut name_preds, NamePred::Suffix(dotted))
            }
            Pred::Ancestor { pattern } => {
                let slot = anc_preds.len() as u32;
                anc_preds.push((slot, pattern.clone()));
                TermEval::AncestorBit(slot)
            }
            Pred::SizeOver(limit) => TermEval::SizeOver(*limit),
            Pred::SizeUnder(limit) => TermEval::SizeUnder(*limit),
            Pred::OlderThan(secs) => TermEval::MtimeAtMost(age_cutoff(opts.now_unix, *secs)),
            Pred::NewerThan(secs) => TermEval::MtimeAtLeast(age_cutoff(opts.now_unix, *secs)),
            Pred::Kind(kind) => TermEval::KindIs(match kind {
                KindTerm::File => Kind::File,
                KindTerm::Dir => Kind::Dir,
                KindTerm::Symlink => Kind::Symlink,
            }),
            Pred::Is(flag) => match flag {
                IsTerm::Hardlink => TermEval::IsHardlink,
                IsTerm::Error => TermEval::IsError,
                IsTerm::Excluded => TermEval::IsExcluded,
            },
        };
        terms.push(CompiledTerm {
            eval,
            negated: term.negated,
        });
    }

    // Pass 0: immutable per-unique-name verdict tables.
    let name_count = tree.name_count();
    let name_bits: Vec<u64> = if name_preds.is_empty() {
        Vec::new()
    } else {
        (0..name_count as u32)
            .map(|id| {
                let name = tree.name_bytes(id);
                let mut bits = 0u64;
                for (slot, pred) in &name_preds {
                    if pred.matches(name) {
                        bits |= 1 << slot;
                    }
                }
                bits
            })
            .collect()
    };
    let (dir_verdict, file_verdict): (Vec<u16>, Vec<u16>) = if group_count == 0 {
        (Vec::new(), Vec::new())
    } else {
        (0..name_count as u32)
            .map(|id| {
                let name = tree.name_bytes(id);
                (
                    patterns
                        .first_match(name, PatternKind::Dir)
                        .unwrap_or(NO_GROUP),
                    patterns
                        .first_match(name, PatternKind::File)
                        .unwrap_or(NO_GROUP),
                )
            })
            .unzip()
    };

    // Pass 1: dir tables in topological order (parent index < child
    // index): ancestor masks, pattern coverage, dir-inode residual.
    let mut anc_mask = vec![0u64; dir_count];
    let mut coverage = vec![NO_GROUP; dir_count];
    let mut residual = DirResidual::default();
    let mut root: Option<DirId> = None;
    for dir in tree.dir_ids() {
        let meta = tree.dir(dir);
        let node_id = meta.node;
        let name_id = tree.node(node_id).name_ref().0 as usize;
        let (mask, cov) = match meta.parent {
            Some(parent) => {
                debug_assert!(parent.index() < dir.index(), "dir table is not topological");
                let mut mask = anc_mask[parent.index()];
                if !anc_preds.is_empty() {
                    let name = tree.name(node_id);
                    for (slot, pattern) in &anc_preds {
                        if glob_match(pattern, name) {
                            mask |= 1 << slot;
                        }
                    }
                }
                let parent_cov = coverage[parent.index()];
                let cov = if parent_cov != NO_GROUP {
                    parent_cov
                } else {
                    dir_verdict.get(name_id).copied().unwrap_or(NO_GROUP)
                };
                (mask, cov)
            }
            None => {
                root = Some(dir);
                // Ancestor terms see the root as its final path component
                // (attack A finding 11); pattern coverage keeps the stored
                // full-path name for exact flat-fold parity.
                let mut mask = 0u64;
                if !anc_preds.is_empty() {
                    let display = final_component(tree.name(node_id));
                    for (slot, pattern) in &anc_preds {
                        if glob_match(pattern, display) {
                            mask |= 1 << slot;
                        }
                    }
                }
                let cov = dir_verdict.get(name_id).copied().unwrap_or(NO_GROUP);
                (mask, cov)
            }
        };
        anc_mask[dir.index()] = mask;
        coverage[dir.index()] = cov;
        if !tree.is_removed(node_id) {
            let own = tree.node(node_id).size();
            residual.apparent += own.apparent;
            residual.disk += own.real;
            residual.dirs += 1;
        }
    }
    let root = root.expect("a non-empty dir arena has a root");

    let ctx = EvalCtx {
        tree,
        terms: &terms,
        name_bits: &name_bits,
        anc_mask: &anc_mask,
        coverage: &coverage,
        dir_verdict: &dir_verdict,
        file_verdict: &file_verdict,
        hardlinks,
        group_count,
        cap: opts.cap,
        node_count,
    };

    // Pass 2: the candidate fold, chunked by contiguous DirId ranges.
    let mut direct = vec![FilteredDirTotals::default(); dir_count];
    let threads = opts.threads.clamp(1, dir_count);
    let mut partial = if threads == 1 {
        fold_range(&ctx, 0, &mut direct)
    } else {
        let chunk = dir_count.div_ceil(threads);
        std::thread::scope(|scope| {
            let handles: Vec<_> = direct
                .chunks_mut(chunk)
                .enumerate()
                .map(|(i, slice)| {
                    let ctx = &ctx;
                    scope.spawn(move || fold_range(ctx, i * chunk, slice))
                })
                .collect();
            // Merge in fixed (thread-index) order; see Partial::merge.
            let mut merged: Option<Partial> = None;
            for handle in handles {
                let part = match handle.join() {
                    Ok(part) => part,
                    Err(panic) => std::panic::resume_unwind(panic),
                };
                match &mut merged {
                    Some(m) => m.merge(part),
                    None => merged = Some(part),
                }
            }
            merged.expect("at least one fold chunk")
        })
    };

    // Pass 2b: pulled hardlink canonicals (sequential, deterministic).
    absorb_pulled(&ctx, &mut partial, &mut direct);

    // Pass 3: reverse-topological sweep — direct totals become filtered
    // subtree totals.
    let mut dir_totals = direct;
    for index in (0..dir_count).rev() {
        if let Some(parent) = tree.dir(DirId::from_raw(index as u32)).parent {
            let totals = dir_totals[index];
            dir_totals[parent.index()].add(totals);
        }
    }

    let matched = dir_totals[root.index()];
    // Partition invariant (flat D1 transplanted): the group buckets plus
    // rest sum to the matched totals, always.
    #[cfg(debug_assertions)]
    {
        let mut sum = partial.rest;
        for bucket in &partial.groups {
            sum.add(*bucket);
        }
        debug_assert_eq!(
            (sum.apparent, sum.disk, sum.entries),
            (matched.apparent, matched.disk, matched.entries),
            "filtered breakdown does not partition the match set"
        );
    }

    let groups = partial
        .groups
        .iter()
        .enumerate()
        .map(|(i, bucket)| {
            let (label, kind) = patterns.group(i as u16).expect("group index in range");
            GroupTotal {
                label: label.to_owned(),
                kind,
                apparent: bucket.apparent,
                disk: bucket.disk,
                entries: bucket.entries,
            }
        })
        .collect();

    let result = FilterResult {
        dir_totals,
        matched: partial.bits,
        matched_apparent: matched.apparent,
        matched_disk: matched.disk,
        matched_entries: matched.entries,
        matched_extra_links: partial.extra_links,
        residual,
        groups,
        rest: RestTotal {
            apparent: partial.rest.apparent,
            disk: partial.rest.disk,
            entries: partial.rest.entries,
        },
        top_files: partial.top.to_sorted(),
        truncated: partial.top.truncated(),
        query_hash,
        epoch: opts.epoch,
    };
    debug!(
        terms = query.terms.len(),
        threads,
        matched_entries = result.matched_entries,
        matched_disk = result.matched_disk,
        extra_links = result.matched_extra_links,
        elapsed_us = started.elapsed().as_micros() as u64,
        "filter fold complete"
    );
    result
}

/// Assign the next name-term slot and register the predicate.
fn name_slot(name_preds: &mut Vec<(u32, NamePred)>, pred: NamePred) -> TermEval {
    let slot = name_preds.len() as u32;
    name_preds.push((slot, pred));
    TermEval::NameBit(slot)
}

/// `now - secs`, clamped (a huge duration means "older than everything").
fn age_cutoff(now: i64, secs: u64) -> i64 {
    i64::try_from(i128::from(now) - i128::from(secs)).unwrap_or(i64::MIN)
}

/// Zeroed group totals in pattern order (the empty-tree result shape).
fn empty_groups(patterns: &PatternSet) -> Vec<GroupTotal> {
    (0..patterns.len())
        .map(|i| {
            let (label, kind) = patterns.group(i as u16).expect("group index in range");
            GroupTotal {
                label: label.to_owned(),
                kind,
                apparent: 0,
                disk: 0,
                entries: 0,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScanOutcome;
    use crate::tree::ChildRun;
    use std::path::PathBuf;
    use std::time::Duration;

    // ---- tokenizer / parser ----

    fn one_term(input: &str) -> Term {
        let parsed = parse(input);
        assert!(
            parsed.errors.is_empty(),
            "unexpected errors: {:?}",
            parsed.errors
        );
        assert_eq!(
            parsed.query.terms().len(),
            1,
            "expected one term for {input:?}"
        );
        parsed.query.terms()[0].clone()
    }

    fn one_error(input: &str) -> ParseError {
        let parsed = parse(input);
        assert!(
            parsed.query.terms().is_empty(),
            "expected no terms for {input:?}: {:?}",
            parsed.query.terms()
        );
        assert_eq!(parsed.errors.len(), 1, "expected one error for {input:?}");
        parsed.errors[0].clone()
    }

    #[test]
    fn empty_input_is_the_empty_query() {
        let parsed = parse("   \t ");
        assert!(parsed.query.is_empty());
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn bare_terms_are_smartcase_substrings() {
        assert_eq!(
            one_term("report").pred,
            Pred::Substring {
                needle: b"report".to_vec(),
                case_insensitive: true
            }
        );
        assert_eq!(
            one_term("Report").pred,
            Pred::Substring {
                needle: b"Report".to_vec(),
                case_insensitive: false
            }
        );
    }

    #[test]
    fn glob_terms_detected_by_star_or_question() {
        assert_eq!(
            one_term("*.log").pred,
            Pred::Glob {
                pattern: b"*.log".to_vec()
            }
        );
        assert_eq!(
            one_term("data?").pred,
            Pred::Glob {
                pattern: b"data?".to_vec()
            }
        );
    }

    #[test]
    fn trailing_slash_is_an_ancestor_constraint() {
        assert_eq!(
            one_term("node_modules/").pred,
            Pred::Ancestor {
                pattern: b"node_modules".to_vec()
            }
        );
        assert_eq!(
            one_term("src*/").pred,
            Pred::Ancestor {
                pattern: b"src*".to_vec()
            }
        );
        let err = one_error("/");
        assert_eq!(err.kind, ParseErrorKind::EmptyTerm);
    }

    #[test]
    fn size_sugar_requires_a_digit() {
        assert_eq!(one_term(">100M").pred, Pred::SizeOver(100 << 20));
        assert_eq!(one_term("<1G").pred, Pred::SizeUnder(1 << 30));
        assert_eq!(one_term(">1.5GiB").pred, Pred::SizeOver(3 << 29));
        // Attack A finding 8: `>readme` is a literal substring.
        assert_eq!(
            one_term(">readme").pred,
            Pred::Substring {
                needle: b">readme".to_vec(),
                case_insensitive: true
            }
        );
        let err = one_error(">100X");
        assert_eq!(err.kind, ParseErrorKind::InvalidValue);
        assert!(err.message.contains("size"), "{}", err.message);
    }

    #[test]
    fn age_terms_parse_the_duration_dialect() {
        assert_eq!(
            one_term("older:6mo").pred,
            Pred::OlderThan((6.0 * MONTH_SECS) as u64)
        );
        assert_eq!(
            one_term("newer:2w").pred,
            Pred::NewerThan((2.0 * WEEK_SECS) as u64)
        );
        assert_eq!(
            one_term("older:36h").pred,
            Pred::OlderThan((36.0 * HOUR_SECS) as u64)
        );
        assert_eq!(
            one_term("older:1.5y").pred,
            Pred::OlderThan((1.5 * YEAR_SECS).round() as u64)
        );
        assert_eq!(one_term("newer:90d").pred, Pred::NewerThan(90 * 86_400));

        let err = one_error("older:");
        assert_eq!(err.kind, ParseErrorKind::EmptyTerm);
        let err = one_error("older:6m");
        assert!(err.message.contains("mo"), "{}", err.message);
        let err = one_error("older:6parsecs");
        assert_eq!(err.kind, ParseErrorKind::InvalidValue);
        let err = one_error("newer:x");
        assert_eq!(err.kind, ParseErrorKind::InvalidValue);
    }

    #[test]
    fn kind_ext_is_qualifiers() {
        assert_eq!(one_term("kind:file").pred, Pred::Kind(KindTerm::File));
        assert_eq!(one_term("kind:dir").pred, Pred::Kind(KindTerm::Dir));
        assert_eq!(one_term("kind:symlink").pred, Pred::Kind(KindTerm::Symlink));
        let err = one_error("kind:sock");
        assert!(
            err.message.contains("file, dir or symlink"),
            "{}",
            err.message
        );

        assert_eq!(
            one_term("ext:log").pred,
            Pred::Ext {
                ext: b"log".to_vec()
            }
        );
        assert_eq!(
            one_term("ext:.log").pred,
            Pred::Ext {
                ext: b"log".to_vec()
            }
        );
        assert_eq!(
            one_term("ext:tar.gz").pred,
            Pred::Ext {
                ext: b"tar.gz".to_vec()
            }
        );
        assert_eq!(one_error("ext:").kind, ParseErrorKind::EmptyTerm);
        assert_eq!(one_error("ext:l*g").kind, ParseErrorKind::InvalidValue);

        assert_eq!(one_term("is:hardlink").pred, Pred::Is(IsTerm::Hardlink));
        assert_eq!(one_term("is:error").pred, Pred::Is(IsTerm::Error));
        assert_eq!(one_term("is:excluded").pred, Pred::Is(IsTerm::Excluded));
        let err = one_error("is:banana");
        assert!(
            err.message.contains("hardlink, error or excluded"),
            "{}",
            err.message
        );
    }

    #[test]
    fn negation_applies_to_any_term() {
        let term = one_term("!*.o");
        assert!(term.negated);
        assert_eq!(
            term.pred,
            Pred::Glob {
                pattern: b"*.o".to_vec()
            }
        );
        assert!(one_term("!older:1y").negated);
        assert!(one_term("!node_modules/").negated);
        assert!(one_term("!>100M").negated);
        assert!(one_term("!\"q(1)\"").negated);

        assert_eq!(one_error("!").kind, ParseErrorKind::EmptyTerm);
        assert_eq!(one_error("!!x").kind, ParseErrorKind::DoubleNegation);
    }

    #[test]
    fn quoted_terms_are_literal_with_two_escapes() {
        assert_eq!(
            one_term("\"q(1).log\"").pred,
            Pred::Literal {
                needle: b"q(1).log".to_vec()
            }
        );
        assert_eq!(
            one_term(r#""a\"b\\c""#).pred,
            Pred::Literal {
                needle: b"a\"b\\c".to_vec()
            }
        );
        // Whitespace lives inside quotes.
        assert_eq!(
            one_term("\"a b\"").pred,
            Pred::Literal {
                needle: b"a b".to_vec()
            }
        );
        // Reserved sigils are inert inside quotes.
        assert_eq!(
            one_term("\"a|b;c\"").pred,
            Pred::Literal {
                needle: b"a|b;c".to_vec()
            }
        );

        assert_eq!(one_error("\"abc").kind, ParseErrorKind::Quote);
        assert_eq!(one_error("\"a\"x").kind, ParseErrorKind::Quote);
        assert_eq!(one_error("\"\"").kind, ParseErrorKind::EmptyTerm);
        let err = one_error(r#""a\n""#);
        assert_eq!(err.kind, ParseErrorKind::Quote);
        assert!(err.message.contains("escape"), "{}", err.message);
    }

    #[test]
    fn reserved_sigils_name_the_future_feature() {
        let err = one_error("(x");
        assert_eq!(err.kind, ParseErrorKind::ReservedSigil);
        assert!(err.message.contains("grouping"), "{}", err.message);
        assert_eq!(err.span, Span { start: 0, end: 1 });

        let err = one_error("a|b");
        assert!(err.message.contains("OR"), "{}", err.message);
        assert_eq!(err.span, Span { start: 1, end: 2 });

        let err = one_error("ext:log;tmp");
        assert!(err.message.contains("value lists"), "{}", err.message);
        assert_eq!(err.span, Span { start: 7, end: 8 });

        let err = one_error("a)b");
        assert!(err.message.contains("grouping"), "{}", err.message);
    }

    #[test]
    fn reserved_sigil_span_points_into_a_longer_input() {
        let parsed = parse("foo (bar");
        assert_eq!(parsed.query.terms().len(), 1, "foo still parses");
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].span, Span { start: 4, end: 5 });
    }

    #[test]
    fn user_and_group_error_with_the_d7_wording() {
        for input in ["user:root", "group:www-data"] {
            let err = one_error(input);
            assert_eq!(err.kind, ParseErrorKind::OwnerNotRetained);
            assert!(
                err.message.contains("not retained by this scan"),
                "{}",
                err.message
            );
            assert!(err.message.contains("uid/gid"), "{}", err.message);
        }
    }

    #[test]
    fn unknown_qualifier_is_inert_with_a_quote_hint() {
        let err = one_error("sixe:10");
        assert_eq!(err.kind, ParseErrorKind::UnknownQualifier);
        assert!(err.message.contains("sixe"), "{}", err.message);
        assert!(err.message.contains("quote"), "{}", err.message);
        // A colon after a non-alphabetic prefix is just a substring.
        assert_eq!(
            one_term("foo.bar:x").pred,
            Pred::Substring {
                needle: b"foo.bar:x".to_vec(),
                case_insensitive: true
            }
        );
    }

    #[test]
    fn path_patterns_are_a_named_future_feature() {
        for input in ["src/x", "a//", "src/**/*.c"] {
            let err = one_error(input);
            assert_eq!(err.kind, ParseErrorKind::PathPattern, "{input}");
            assert!(err.message.contains("ancestor"), "{}", err.message);
        }
    }

    #[test]
    fn broken_terms_are_inert_but_the_rest_still_parses() {
        let parsed = parse("*.log user:root >100M sixe:10");
        assert_eq!(parsed.query.terms().len(), 2);
        assert_eq!(parsed.errors.len(), 2);
        assert_eq!(
            parsed.query.terms()[1].pred,
            Pred::SizeOver(100 << 20),
            "later terms unaffected by earlier errors"
        );
    }

    #[test]
    fn too_many_terms_is_reported_and_truncated() {
        let input = vec!["a"; MAX_TERMS + 3].join(" ");
        let parsed = parse(&input);
        assert_eq!(parsed.query.terms().len(), MAX_TERMS);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].kind, ParseErrorKind::TooManyTerms);
    }

    #[test]
    fn fingerprint_ignores_whitespace_but_not_semantics() {
        let a = parse("*.log  >100M").query;
        let b = parse(" *.log >100M ").query;
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), parse("*.log >200M").query.fingerprint());
        assert_ne!(a.fingerprint(), parse("!*.log >100M").query.fingerprint());
        assert_ne!(
            parse("").query.fingerprint(),
            parse("a").query.fingerprint()
        );
    }

    #[test]
    fn parse_never_panics_on_garbage() {
        let charset: Vec<char> = "abzXYZ019 \t!\"\\()|;:<>/*?.-_~%éλ🦀".chars().collect();
        let mut state = 0x243F_6A88_85A3_08D3_u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        for _ in 0..3000 {
            let len = (next() % 48) as usize;
            let input: String = (0..len)
                .map(|_| charset[next() as usize % charset.len()])
                .collect();
            let parsed = parse(&input);
            for err in &parsed.errors {
                assert!(err.span.start <= err.span.end, "span order in {input:?}");
                assert!(err.span.end <= input.len(), "span bounds in {input:?}");
            }
            let _ = parsed.query.fingerprint();
        }
    }

    #[test]
    fn final_component_of_root_names() {
        assert_eq!(final_component(b"/home/theo/projects"), b"projects");
        assert_eq!(final_component(b"/home/theo/projects/"), b"projects");
        assert_eq!(final_component(b"projects"), b"projects");
        assert_eq!(final_component(b"."), b".");
        assert_eq!(final_component(b"/"), b"/");
    }

    // ---- filtered fold over a hand-built fixture ----

    /// A fixed "now" so age predicates are reproducible.
    const NOW: i64 = 1_000_000_000;

    fn dir_size() -> Size {
        Size::new(4096, 8)
    }

    struct Fixture {
        outcome: ScanOutcome,
        root: DirId,
        src: DirId,
        nm: DirId,
        big: NodeId,
        db: NodeId,
        zz: NodeId,
        mnt: NodeId,
        broken: NodeId,
        main_rs: NodeId,
        app_log: NodeId,
        q1_log: NodeId,
        x_js: NodeId,
    }

    /// Build (aggregates maintained exactly as the owner does):
    /// ```text
    /// /home/theo/projects/           (root; stored name = full path)
    ///   src/
    ///     main.rs      3000 B /  4096 disk, mtime NOW-2y
    ///     app.log      5000 B /  8192 disk, mtime NOW-1d
    ///     q(1).log      100 B /   512 disk, mtime NOW-1d
    ///   node_modules/
    ///     x.js         1000 B /  1024 disk, mtime NOW-3d
    ///   big.bin        1 MiB / 1 MiB disk,  mtime NOW-1h
    ///   data.db        2000 B /  4096 disk, mtime NOW-10d  (canonical hardlink)
    ///   zz.bak         2000 B /  4096 disk, mtime NOW-10d  (HARDLINK_EXTRA of data.db)
    ///   mnt/           EXCLUDED leaf dir (no DirMeta), mtime 0
    ///   broken         ERROR file, 0 B, mtime 0
    /// ```
    fn fixture() -> Fixture {
        const DAY: i64 = 86_400;
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/home/theo/projects", dir_size(), NOW - 30 * DAY);
        let root = tree.add_dir(root_node, None, 1);

        let src_node = tree.push_node(
            b"src",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            dir_size(),
            NOW - 30 * DAY,
        );
        let nm_node = tree.push_node(
            b"node_modules",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            dir_size(),
            NOW - 30 * DAY,
        );
        let big = tree.push_node(
            b"big.bin",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(1 << 20, 2048),
            NOW - 3600,
        );
        let db = tree.push_node(
            b"data.db",
            Kind::File,
            NodeFlags::default(),
            root_node,
            Size::new(2000, 8),
            NOW - 10 * DAY,
        );
        let zz = tree.push_node(
            b"zz.bak",
            Kind::File,
            NodeFlags::HARDLINK_EXTRA,
            root_node,
            Size::new(2000, 8),
            NOW - 10 * DAY,
        );
        let mnt = tree.push_node(
            b"mnt",
            Kind::Dir,
            NodeFlags::EXCLUDED,
            root_node,
            dir_size(),
            0,
        );
        tree.set_excluded(mnt, crate::tree::ExcludedReason::OtherFs);
        let broken = tree.push_node(
            b"broken",
            Kind::File,
            NodeFlags::ERROR,
            root_node,
            Size::default(),
            0,
        );
        tree.push_run(
            root,
            ChildRun {
                start: src_node.index() as u32,
                len: 7,
            },
        );
        let src = tree.add_dir(src_node, Some(root), 1);
        let nm = tree.add_dir(nm_node, Some(root), 1);
        tree.mark_hardlink_first(db);
        // Root section delta: src + nm own inodes, big, data.db, mnt,
        // broken (zz is an extra: contributes 0).
        tree.apply_delta(
            root,
            4096 + 4096 + (1 << 20) + 2000 + 4096,
            4096 + 4096 + (1 << 20) + 4096 + 4096,
            6,
            1,
        );

        let main_rs = tree.push_node(
            b"main.rs",
            Kind::File,
            NodeFlags::default(),
            src_node,
            Size::new(3000, 8),
            NOW - 2 * 31_557_600 - 10,
        );
        let app_log = tree.push_node(
            b"app.log",
            Kind::File,
            NodeFlags::default(),
            src_node,
            Size::new(5000, 16),
            NOW - DAY,
        );
        let q1_log = tree.push_node(
            b"q(1).log",
            Kind::File,
            NodeFlags::default(),
            src_node,
            Size::new(100, 1),
            NOW - DAY,
        );
        tree.push_run(
            src,
            ChildRun {
                start: main_rs.index() as u32,
                len: 3,
            },
        );
        tree.apply_delta(src, 3000 + 5000 + 100, 4096 + 8192 + 512, 3, 0);

        let x_js = tree.push_node(
            b"x.js",
            Kind::File,
            NodeFlags::default(),
            nm_node,
            Size::new(1000, 2),
            NOW - 3 * DAY,
        );
        tree.push_run(
            nm,
            ChildRun {
                start: x_js.index() as u32,
                len: 1,
            },
        );
        tree.apply_delta(nm, 1000, 1024, 1, 0);

        let links = vec![
            crate::scan::HardlinkLink {
                node: db,
                dev: 1,
                ino: 42,
                nlink: 2,
            },
            crate::scan::HardlinkLink {
                node: zz,
                dev: 1,
                ino: 42,
                nlink: 2,
            },
        ];
        let mut outcome = ScanOutcome::from_tree(
            tree,
            root,
            PathBuf::from("/home/theo/projects"),
            links,
            1,
            1,
            0,
            Duration::from_millis(1),
        );
        // Canonical attribution: `data.db` < `zz.bak`, so the counted link
        // already is the canonical one — a no-op move, but it flips the
        // finalized flag the index build requires.
        outcome.finalize_hardlinks();
        Fixture {
            outcome,
            root,
            src,
            nm,
            big,
            db,
            zz,
            mnt,
            broken,
            main_rs,
            app_log,
            q1_log,
            x_js,
        }
    }

    fn opts(threads: usize) -> ApplyOptions {
        ApplyOptions {
            cap: 1000,
            epoch: 0,
            now_unix: NOW,
            threads,
        }
    }

    fn run(fixture: &Fixture, input: &str) -> FilterResult {
        let parsed = parse(input);
        assert!(
            parsed.errors.is_empty(),
            "query {input:?}: {:?}",
            parsed.errors
        );
        let index = HardlinkIndex::build(&fixture.outcome, 0);
        apply(
            fixture.outcome.tree(),
            &parsed.query,
            &PatternSet::presets(),
            &index,
            &opts(1),
        )
    }

    #[test]
    fn empty_query_matches_everything_but_dir_inodes() {
        let f = fixture();
        let result = run(&f, "");
        let root_meta = f.outcome.tree().dir(f.root);

        // Residual: root + src + node_modules own inodes (mnt has no
        // DirMeta: it is a candidate, not structure).
        assert_eq!(result.residual.dirs, 3);
        assert_eq!(result.residual.disk, 3 * 4096);
        assert_eq!(result.residual.apparent, 3 * 4096);

        // The D4 invariant: empty query ⇒ matched == aggregates − residual.
        assert_eq!(result.matched_disk, root_meta.td - result.residual.disk);
        assert_eq!(
            result.matched_apparent,
            root_meta.ta - result.residual.apparent
        );
        assert_eq!(result.matched_entries, root_meta.tn - result.residual.dirs);
        assert_eq!(result.matched_entries, 8);

        // zz.bak (extra) is present as a 0-byte row.
        assert!(result.matched.contains(f.zz));
        assert_eq!(result.matched_extra_links, 1);
        assert_eq!(result.matched.count(), 9, "8 counted + 1 extra row");

        // Files only in top-N (mnt is a dir-kind candidate, never ranked).
        assert_eq!(result.top_files.len(), 7);
        assert_eq!(result.top_files[0].node, f.big);
        assert!(!result.truncated);
        assert_eq!(result.epoch, 0);
        assert_eq!(result.query_hash, parse("").query.fingerprint());
    }

    #[test]
    fn glob_refilters_directory_totals() {
        let f = fixture();
        let result = run(&f, "*.log");

        // app.log + q(1).log, both under src.
        let src = result.dir_total(f.src);
        assert_eq!((src.apparent, src.disk, src.entries), (5100, 8704, 2));
        let root = result.dir_total(f.root);
        assert_eq!((root.apparent, root.disk, root.entries), (5100, 8704, 2));
        assert_eq!(result.dir_total(f.nm), FilteredDirTotals::default());

        assert_eq!(result.matched_disk, 8704);
        assert_eq!(result.matched_entries, 2);
        assert!(result.matched.contains(f.app_log));
        assert!(result.matched.contains(f.q1_log));
        assert!(!result.matched.contains(f.main_rs));

        // Top-N over the match set only.
        let tops: Vec<NodeId> = result.top_files.iter().map(|t| t.node).collect();
        assert_eq!(tops, [f.app_log, f.q1_log]);

        // Breakdown under the filter: both files land in the *.log group.
        let log_group = result.groups.iter().find(|g| g.label == "*.log").unwrap();
        assert_eq!((log_group.disk, log_group.entries), (8704, 2));
        assert_eq!(result.rest.entries, 0);
    }

    #[test]
    fn hardlink_membership_by_any_path_counts_bytes_once() {
        let f = fixture();
        // `*.bak` names only the extra link; D3 pulls the canonical in.
        let result = run(&f, "*.bak");

        assert!(result.matched.contains(f.zz), "extra row present (0 bytes)");
        assert!(result.matched.contains(f.db), "canonical pulled in");
        assert_eq!(result.matched_extra_links, 1);
        // Bytes once, at the canonical.
        assert_eq!(result.matched_disk, 4096);
        assert_eq!(result.matched_apparent, 2000);
        assert_eq!(result.matched_entries, 1);
        let root = result.dir_total(f.root);
        assert_eq!((root.disk, root.entries), (4096, 1));
        // The canonical ranks in top-N with the hardlink badge.
        assert_eq!(result.top_files.len(), 1);
        assert_eq!(result.top_files[0].node, f.db);
        assert!(result.top_files[0].hardlink);
    }

    #[test]
    fn hardlink_query_matching_both_links_still_counts_once() {
        let f = fixture();
        let result = run(&f, "is:hardlink");
        assert!(result.matched.contains(f.db));
        assert!(result.matched.contains(f.zz));
        assert_eq!(result.matched_entries, 1);
        assert_eq!(result.matched_disk, 4096);
        assert_eq!(result.matched_extra_links, 1);
    }

    #[test]
    fn canonical_name_match_does_not_drag_extra_rows_in() {
        let f = fixture();
        let result = run(&f, "data*");
        assert!(result.matched.contains(f.db));
        assert!(
            !result.matched.contains(f.zz),
            "zz.bak's own path does not match"
        );
        assert_eq!(result.matched_entries, 1);
        assert_eq!(result.matched_extra_links, 0);
    }

    #[test]
    fn ancestor_terms_scope_to_subtrees() {
        let f = fixture();
        let result = run(&f, "src/");
        assert_eq!(result.matched_entries, 3);
        assert_eq!(result.matched_disk, 4096 + 8192 + 512);
        assert!(result.matched.contains(f.main_rs));
        assert!(!result.matched.contains(f.big));

        let negated = run(&f, "!src/");
        assert_eq!(negated.matched_entries, 8 - 3);
        assert!(negated.matched.contains(f.big));
        assert!(!negated.matched.contains(f.main_rs));
    }

    #[test]
    fn ancestor_terms_reach_the_scan_root_by_final_component() {
        let f = fixture();
        // Root name is the full path `/home/theo/projects`; the fix makes
        // `projects/` an ancestor of every candidate.
        let via_root = run(&f, "projects/");
        let all = run(&f, "");
        assert_eq!(via_root.matched_entries, all.matched_entries);
        assert_eq!(via_root.matched_disk, all.matched_disk);

        // Components above the scan root are not scanned content.
        let above = run(&f, "theo/");
        assert_eq!(above.matched_entries, 0);
        assert_eq!(above.matched_disk, 0);
    }

    #[test]
    fn kind_is_and_error_flags() {
        let f = fixture();

        let excluded = run(&f, "is:excluded");
        assert_eq!(excluded.matched_entries, 1);
        assert_eq!(excluded.matched_disk, 4096);
        assert!(excluded.matched.contains(f.mnt));

        // `kind:dir` matches only not-descended dir entries (the excluded
        // mount) — scanned directories are never candidates.
        let dirs = run(&f, "kind:dir");
        assert_eq!(dirs.matched_entries, 1);
        assert!(dirs.matched.contains(f.mnt));

        let errors = run(&f, "is:error");
        assert_eq!(errors.matched_entries, 1);
        assert_eq!(errors.matched_disk, 0);
        assert!(errors.matched.contains(f.broken));

        let files = run(&f, "kind:file");
        assert_eq!(files.matched_entries, 7, "all counted files, not mnt");
        assert!(!files.matched.contains(f.mnt));
    }

    #[test]
    fn age_terms_are_mtime_cutoffs() {
        let f = fixture();
        let old = run(&f, "older:1y");
        assert_eq!(old.matched_entries, 3, "main.rs + the two mtime-0 stubs");
        assert!(old.matched.contains(f.main_rs));
        assert!(old.matched.contains(f.mnt));
        assert!(old.matched.contains(f.broken));

        let recent = run(&f, "newer:1w");
        assert_eq!(recent.matched_entries, 4);
        for id in [f.big, f.app_log, f.q1_log, f.x_js] {
            assert!(recent.matched.contains(id));
        }
        assert!(!recent.matched.contains(f.db), "NOW-10d is older than 1w");
    }

    #[test]
    fn size_terms_compare_disk_bytes() {
        let f = fixture();
        let big = run(&f, ">500K");
        assert_eq!(big.matched_entries, 1);
        assert!(big.matched.contains(f.big));

        let small = run(&f, "<2K");
        assert_eq!(
            small.matched_entries, 3,
            "x.js (1024) + q(1).log (512) + broken (0)"
        );
        for id in [f.x_js, f.q1_log, f.broken] {
            assert!(small.matched.contains(id));
        }
    }

    #[test]
    fn smartcase_and_literal_terms() {
        let f = fixture();
        assert_eq!(run(&f, "app").matched_entries, 1);
        assert_eq!(run(&f, "APP").matched_entries, 0, "capital ⇒ exact");
        assert_eq!(run(&f, "BIG").matched_entries, 0);
        assert_eq!(run(&f, "big").matched_entries, 1);

        // Literal quoting reaches names full of syntax characters.
        let quoted = run(&f, "\"q(1)\"");
        assert_eq!(quoted.matched_entries, 1);
        assert!(quoted.matched.contains(f.q1_log));
    }

    #[test]
    fn ext_sugar_is_a_byte_exact_suffix() {
        let f = fixture();
        let logs = run(&f, "ext:log");
        assert_eq!(logs.matched_entries, 2);
        assert!(logs.matched.contains(f.app_log));
        assert!(logs.matched.contains(f.q1_log));
        assert_eq!(run(&f, "ext:js").matched_entries, 1);
    }

    #[test]
    fn conjunction_and_negation_compose() {
        let f = fixture();
        let result = run(&f, "src/ !*.log kind:file");
        assert_eq!(result.matched_entries, 1);
        assert!(result.matched.contains(f.main_rs));

        let none = run(&f, "*.log >1G");
        assert_eq!(none.matched_entries, 0);
        assert_eq!(none.matched.count(), 0);
        assert!(none.top_files.is_empty());
    }

    #[test]
    fn breakdown_buckets_cover_only_the_match_set() {
        let f = fixture();
        let result = run(&f, "newer:1w");
        // Matches: big.bin (rest), app.log + q(1).log (*.log group),
        // x.js (node_modules coverage).
        let by_label = |label: &str| result.groups.iter().find(|g| g.label == label).unwrap();
        let nm = by_label("node_modules");
        assert_eq!((nm.disk, nm.entries), (1024, 1));
        let log = by_label("*.log");
        assert_eq!((log.disk, log.entries), (8192 + 512, 2));
        assert_eq!(result.rest.entries, 1);
        assert_eq!(result.rest.disk, 1 << 20);

        // Partition invariant over the match set.
        let group_sum: u64 = result.groups.iter().map(|g| g.disk).sum();
        assert_eq!(group_sum + result.rest.disk, result.matched_disk);
        let entries_sum: u64 = result.groups.iter().map(|g| g.entries).sum();
        assert_eq!(entries_sum + result.rest.entries, result.matched_entries);
    }

    #[test]
    fn result_is_stamped_with_query_hash_and_epoch() {
        let f = fixture();
        let parsed = parse("*.log");
        let index = HardlinkIndex::build(&f.outcome, 7);
        assert_eq!(index.epoch(), 7);
        let result = apply(
            f.outcome.tree(),
            &parsed.query,
            &PatternSet::presets(),
            &index,
            &ApplyOptions {
                cap: 10,
                epoch: 7,
                now_unix: NOW,
                threads: 1,
            },
        );
        assert_eq!(result.epoch, 7);
        assert_eq!(result.query_hash, parsed.query.fingerprint());
    }

    #[test]
    fn deletion_epoch_invalidates_the_hardlink_pull() {
        let mut f = fixture();
        let stale = HardlinkIndex::build(&f.outcome, 0);
        assert_eq!(stale.canonical_of(f.zz), Some(f.db));

        // Delete the canonical link; the deletion epoch moves to 1.
        f.outcome.apply_removal(f.db).expect("remove data.db");

        // Even with the stale index, a matching extra no longer resurrects
        // the tombstoned canonical (bytes were already subtracted).
        let parsed = parse("*.bak");
        let result = apply(
            f.outcome.tree(),
            &parsed.query,
            &PatternSet::presets(),
            &stale,
            &ApplyOptions {
                cap: 10,
                epoch: 1,
                now_unix: NOW,
                threads: 1,
            },
        );
        assert!(
            result.matched.contains(f.zz),
            "the extra row is still honest"
        );
        assert!(!result.matched.contains(f.db));
        assert_eq!(result.matched_entries, 0);
        assert_eq!(result.matched_disk, 0);
        assert_eq!(result.matched_extra_links, 1);
        assert_eq!(result.epoch, 1);

        // The rebuilt index drops the mapping entirely.
        let rebuilt = HardlinkIndex::build(&f.outcome, 1);
        assert_eq!(rebuilt.canonical_of(f.zz), None);
        assert_eq!(rebuilt.epoch(), 1);
    }

    #[test]
    fn removed_subtrees_leave_the_match_set() {
        let mut f = fixture();
        let src_node = f.outcome.tree().dir(f.src).node;
        f.outcome.apply_removal(src_node).expect("remove src/");
        let index = HardlinkIndex::build(&f.outcome, 1);
        let parsed = parse("*.log");
        let result = apply(
            f.outcome.tree(),
            &parsed.query,
            &PatternSet::presets(),
            &index,
            &ApplyOptions {
                cap: 10,
                epoch: 1,
                now_unix: NOW,
                threads: 1,
            },
        );
        assert_eq!(result.matched_entries, 0, "tombstoned rows are invisible");
        assert_eq!(result.residual.dirs, 2, "src's own inode left the residual");
    }

    #[test]
    fn empty_tree_yields_an_empty_result() {
        let tree = Tree::new();
        let parsed = parse("*.log");
        let result = apply(
            &tree,
            &parsed.query,
            &PatternSet::presets(),
            &HardlinkIndex::empty(0),
            &opts(4),
        );
        assert_eq!(result.matched_entries, 0);
        assert_eq!(result.groups.len(), PatternSet::presets().len());
        assert!(result.top_files.is_empty());
    }

    // ---- determinism across thread counts ----

    /// Deterministic synthetic tree: `dirs` directories under the root,
    /// `files_per_dir` files each, names/sizes/mtimes from an LCG. Every
    /// 7th file of every 3rd directory is a HARDLINK_EXTRA whose canonical
    /// is the first file of the tree.
    fn synthetic_tree(dirs: usize, files_per_dir: usize) -> (Tree, HardlinkIndex) {
        const EXTS: [&str; 5] = ["log", "rs", "tmp", "dat", "bin"];
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        let mut tree = Tree::new();
        let root_node = tree.push_root_node(b"/bench/root", dir_size(), NOW);
        let root = tree.add_dir(root_node, None, 1);
        let mut dir_nodes = Vec::with_capacity(dirs);
        let first_dir_node = tree.push_node(
            b"dir-head",
            Kind::Dir,
            NodeFlags::default(),
            root_node,
            dir_size(),
            NOW,
        );
        dir_nodes.push(first_dir_node);
        for i in 1..dirs {
            let name = match i % 5 {
                0 => "node_modules".to_owned(),
                1 => format!("src-{i}"),
                _ => format!("dir-{}", i % 97),
            };
            dir_nodes.push(tree.push_node(
                name.as_bytes(),
                Kind::Dir,
                NodeFlags::default(),
                root_node,
                dir_size(),
                NOW,
            ));
        }
        tree.push_run(
            root,
            ChildRun {
                start: first_dir_node.index() as u32,
                len: dirs as u32,
            },
        );
        let mut canonical: Option<NodeId> = None;
        let mut extras: Vec<NodeId> = Vec::new();
        for (i, &dir_node) in dir_nodes.iter().enumerate() {
            let dir = tree.add_dir(dir_node, Some(root), 1);
            let mut first: Option<NodeId> = None;
            for f in 0..files_per_dir {
                let name = format!("f{:03}.{}", next() % 500, EXTS[(next() % 5) as usize]);
                let blocks = next() % 4096;
                let mtime = NOW - (next() % (3 * 31_557_600)) as i64;
                let extra = i % 3 == 0 && f % 7 == 3 && canonical.is_some();
                let flags = if extra {
                    NodeFlags::HARDLINK_EXTRA
                } else {
                    NodeFlags::default()
                };
                let id = tree.push_node(
                    name.as_bytes(),
                    Kind::File,
                    flags,
                    dir_node,
                    Size::new(blocks * 512, blocks),
                    mtime,
                );
                if extra {
                    extras.push(id);
                }
                if canonical.is_none() {
                    canonical = Some(id);
                    tree.mark_hardlink_first(id);
                }
                first.get_or_insert(id);
            }
            tree.push_run(
                dir,
                ChildRun {
                    start: first.expect("files pushed").index() as u32,
                    len: files_per_dir as u32,
                },
            );
        }
        let canonical = canonical.expect("at least one file");
        let index = HardlinkIndex {
            extra_to_canonical: extras.into_iter().map(|e| (e, canonical)).collect(),
            epoch: 0,
        };
        (tree, index)
    }

    #[test]
    fn identical_results_for_any_thread_count() {
        let (tree, index) = synthetic_tree(150, 40);
        let patterns = PatternSet::presets();
        for input in [
            "",
            "*.log",
            "*.log >100K",
            "!*.tmp older:1y",
            "node_modules/ *.log",
            "f0 newer:2y",
            "is:hardlink",
        ] {
            let parsed = parse(input);
            assert!(parsed.errors.is_empty());
            let baseline = apply(&tree, &parsed.query, &patterns, &index, &opts(1));
            for threads in [2, 4, 8, 13] {
                let result = apply(&tree, &parsed.query, &patterns, &index, &opts(threads));
                assert_eq!(result, baseline, "query {input:?} with {threads} threads");
            }
        }
    }

    /// Engine bench (D5): run with
    /// `cargo test -p camembert-core --release -- --ignored bench_filter_fold`.
    #[test]
    #[ignore = "bench: 1M-node synthetic tree, run with --release -- --ignored"]
    fn bench_filter_fold_1m_nodes() {
        let (tree, index) = synthetic_tree(5000, 200);
        assert!(tree.node_count() > 1_000_000);
        let patterns = PatternSet::presets();
        let parsed = parse("*.log >100K older:6mo");
        assert!(parsed.errors.is_empty());
        let mut baseline: Option<FilterResult> = None;
        for threads in [1usize, 2, 4, 8] {
            let options = opts(threads);
            let mut best = Duration::MAX;
            let mut result = None;
            for _ in 0..5 {
                let start = Instant::now();
                let r = apply(&tree, &parsed.query, &patterns, &index, &options);
                best = best.min(start.elapsed());
                result = Some(r);
            }
            let result = result.expect("ran at least once");
            println!(
                "bench_filter_fold: nodes={} threads={threads} best={:?} matched={} disk={}",
                tree.node_count(),
                best,
                result.matched_entries,
                result.matched_disk,
            );
            match &baseline {
                Some(b) => assert_eq!(&result, b, "thread count changed the result"),
                None => baseline = Some(result),
            }
        }
    }
}
