//! Name percent-encoding (spec §4) and JSON line assembly (spec §5).
//!
//! Filenames are raw bytes; to form a valid JSON string, bytes that are
//! not part of valid UTF-8 are emitted as `%XX` (uppercase hex) and a
//! literal `%` as `%25`. **The encoding is not the sort key** — ordering
//! is always defined on the decoded raw bytes (decision D3).
//!
//! Number policy (decision D4): `ino`/`dev` are always JSON strings; any
//! other u64 (or i64) field is a JSON number below 2^53 and a decimal
//! string at or above it, because `JSON.parse`/jq arithmetic silently
//! corrupts larger integers.

use std::fmt::Write as _;

/// Largest integer magnitude exactly representable in an IEEE-754 double:
/// values at or above this are emitted as strings (D4).
const JSON_SAFE_LIMIT: u64 = 1 << 53;

/// Encode raw filename bytes into a valid JSON-string payload (spec §4):
/// non-UTF-8 bytes become `%XX` (uppercase), `%` becomes `%25`, everything
/// else passes through. Injective and reversible ([`decode_name`]).
pub fn encode_name(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                push_percent_escaped(valid, &mut out);
                break;
            }
            Err(err) => {
                let (valid, invalid) = rest.split_at(err.valid_up_to());
                push_percent_escaped(
                    std::str::from_utf8(valid).expect("split at valid_up_to"),
                    &mut out,
                );
                // error_len is None only at end-of-input truncation.
                let bad = err.error_len().unwrap_or(invalid.len());
                for &b in &invalid[..bad] {
                    write!(out, "%{b:02X}").expect("write to String");
                }
                rest = &invalid[bad..];
            }
        }
    }
    out
}

fn push_percent_escaped(s: &str, out: &mut String) {
    for ch in s.chars() {
        if ch == '%' {
            out.push_str("%25");
        } else {
            out.push(ch);
        }
    }
}

/// Reverse of [`encode_name`]: `%XX` pairs back to raw bytes. A `%` not
/// followed by two hex digits (never produced by the encoder) passes
/// through literally.
pub fn decode_name(encoded: &str) -> Vec<u8> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| {
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                Some((hi * 16 + lo) as u8)
            })
            .flatten();
        match hex {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// One JSON Lines record under construction. Key order is the caller's
/// insertion order; string values are escaped through `serde_json`.
pub(crate) struct JsonLine {
    buf: String,
}

impl JsonLine {
    pub(crate) fn new() -> Self {
        Self {
            buf: String::from("{"),
        }
    }

    fn key(&mut self, key: &str) {
        if self.buf.len() > 1 {
            self.buf.push(',');
        }
        // Keys are fixed ASCII identifiers; no escaping needed.
        write!(self.buf, "\"{key}\":").expect("write to String");
    }

    pub(crate) fn str(&mut self, key: &str, value: &str) -> &mut Self {
        self.key(key);
        self.buf
            .push_str(&serde_json::to_string(value).expect("string serialization is infallible"));
        self
    }

    /// u64 under the D4 policy: number below 2^53, decimal string above.
    pub(crate) fn u64(&mut self, key: &str, value: u64) -> &mut Self {
        self.key(key);
        if value < JSON_SAFE_LIMIT {
            write!(self.buf, "{value}").expect("write to String");
        } else {
            write!(self.buf, "\"{value}\"").expect("write to String");
        }
        self
    }

    /// u64 always emitted as a decimal string (`ino`/`dev`, D4).
    pub(crate) fn u64_string(&mut self, key: &str, value: u64) -> &mut Self {
        self.key(key);
        write!(self.buf, "\"{value}\"").expect("write to String");
        self
    }

    /// i64 under the same magnitude policy as [`JsonLine::u64`].
    pub(crate) fn i64(&mut self, key: &str, value: i64) -> &mut Self {
        self.key(key);
        if value.unsigned_abs() < JSON_SAFE_LIMIT {
            write!(self.buf, "{value}").expect("write to String");
        } else {
            write!(self.buf, "\"{value}\"").expect("write to String");
        }
        self
    }

    pub(crate) fn bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.key(key);
        write!(self.buf, "{value}").expect("write to String");
        self
    }

    /// Seconds with millisecond precision (the `e` line's `elapsed`).
    pub(crate) fn seconds(&mut self, key: &str, value: f64) -> &mut Self {
        self.key(key);
        write!(self.buf, "{value:.3}").expect("write to String");
        self
    }

    /// Close the object and return the newline-terminated line.
    pub(crate) fn finish(mut self) -> String {
        self.buf.push_str("}\n");
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_passes_utf8_through() {
        assert_eq!(encode_name(b"access.log"), "access.log");
        assert_eq!(encode_name("caf\u{e9} bl\u{e9}".as_bytes()), "café blé");
    }

    #[test]
    fn encode_escapes_non_utf8_uppercase_and_percent() {
        assert_eq!(encode_name(b"caf\xe9.log"), "caf%E9.log");
        assert_eq!(encode_name(b"\xab\xcd"), "%AB%CD");
        assert_eq!(encode_name(b"100%.txt"), "100%25.txt");
        assert_eq!(encode_name(b"%\xff%"), "%25%FF%25");
    }

    #[test]
    fn encode_decode_round_trips() {
        let cases: &[&[u8]] = &[
            b"plain",
            b"caf\xe9.log",
            b"100%.txt",
            b"%25 already encoded-looking",
            b"\xff\xfe\xfd",
            b"mixed \xf0\x9f\xa7\x80 emoji and \xff bad",
            b"trailing truncated \xf0\x9f",
            b"",
        ];
        for &case in cases {
            assert_eq!(
                decode_name(&encode_name(case)),
                case,
                "round trip of {case:?}"
            );
        }
    }

    #[test]
    fn sort_key_is_raw_bytes_not_encoded_form() {
        // Raw bytes: b"~" (0x7E) < b"\xFF". Encoded forms would invert:
        // "%FF" ('%' = 0x25) < "~". The comparator must use raw bytes.
        let (a, b): (&[u8], &[u8]) = (b"~", b"\xff");
        assert!(a < b, "raw-byte order");
        assert!(
            encode_name(a) > encode_name(b),
            "encoded order disagrees — proving the encoded form must not be the key"
        );
    }

    #[test]
    fn u64_boundary_at_2_pow_53() {
        let line = |v: u64| {
            let mut l = JsonLine::new();
            l.u64("v", v);
            l.finish()
        };
        assert_eq!(line((1 << 53) - 1), "{\"v\":9007199254740991}\n");
        assert_eq!(line(1 << 53), "{\"v\":\"9007199254740992\"}\n");
        assert_eq!(line(u64::MAX), format!("{{\"v\":\"{}\"}}\n", u64::MAX));
    }

    #[test]
    fn ino_dev_are_always_strings() {
        let mut l = JsonLine::new();
        l.u64_string("i", 7);
        assert_eq!(l.finish(), "{\"i\":\"7\"}\n");
    }

    #[test]
    fn i64_negative_and_boundary() {
        let line = |v: i64| {
            let mut l = JsonLine::new();
            l.i64("m", v);
            l.finish()
        };
        assert_eq!(line(-12345), "{\"m\":-12345}\n");
        assert_eq!(line(-(1 << 53)), "{\"m\":\"-9007199254740992\"}\n");
    }

    #[test]
    fn json_string_escaping_is_delegated() {
        let mut l = JsonLine::new();
        l.str("n", "with \"quotes\" and \\ backslash");
        let line = l.finish();
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["n"], "with \"quotes\" and \\ backslash");
    }
}
