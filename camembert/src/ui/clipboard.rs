//! Copy-path-to-clipboard (`y`) via OSC 52, deliberately not a clipboard
//! crate: no new dependency, and — unlike every native clipboard API —
//! it works over SSH, which is exactly where a disk-usage tool run on a
//! remote box needs it most. Windows Terminal and most modern terminal
//! emulators implement it; a terminal that does not just ignores the
//! escape sequence.
//!
//! # Why the toast can only say "attempted"
//!
//! OSC 52 is one-way: the terminal either honors it or silently drops
//! it, and there is no reply to wait for (contrast [`super::osc11`],
//! which *does* get one). So [`copy`] can only report whether the bytes
//! reached the terminal — a `write`/`flush` succeeding — never whether
//! the clipboard actually changed. The caller's toast must be worded as
//! an attempt ("path copied (OSC 52)"), not a confirmed fact.

use std::io::{self, Write};
use std::path::Path;

/// Standard base64 alphabet (RFC 4648 §4 — not the URL-safe variant):
/// what every terminal's OSC 52 handler expects.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Hand-rolled rather than a crate dependency (see the module doc): an
/// OSC 52 payload is a path's worth of bytes, not a hot path, and this is
/// the entire algorithm — no incremental/streaming case to get wrong.
fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(match b1 {
            Some(b1) => ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => ALPHABET[(b2 & 0x3f) as usize] as char,
            None => '=',
        });
    }
    out
}

/// The raw OSC 52 escape sequence (`ESC ] 52 ; c ; <base64> BEL`) that
/// sets the system clipboard (`c`) to `path`'s text. Pure — no I/O — so
/// it is unit-testable against known vectors without a terminal.
///
/// The payload is `path`'s *text*: a clipboard is inherently text, so a
/// non-UTF-8 path is decoded lossily ([`Path::to_string_lossy`]), the
/// same tradeoff every other display-only path rendering in this UI
/// already makes (`ui.rs`'s row-name formatting). This is display, not a
/// delete or a reveal argv — there is no filesystem round-trip to get
/// wrong here, only what shows up on the clipboard.
pub fn sequence(path: &Path) -> String {
    let text = path.to_string_lossy();
    let encoded = base64_encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

/// Write the OSC 52 sequence straight to the terminal. Succeeds or fails
/// on whether the bytes reached the terminal, never on whether the
/// clipboard actually changed (module doc) — the caller's toast wording
/// must reflect that.
pub fn copy(path: &Path) -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(sequence(path).as_bytes())?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn base64_matches_the_rfc_4648_textbook_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn sequence_is_the_osc_52_envelope_around_the_encoded_path() {
        let path = Path::new("/tmp/foo");
        let expected = format!("\x1b]52;c;{}\x07", base64_encode(b"/tmp/foo"));
        assert_eq!(sequence(path), expected);
        assert!(expected.starts_with("\x1b]52;c;"));
        assert!(expected.ends_with('\x07'));
    }

    #[test]
    fn sequence_handles_a_known_ascii_path() {
        // Hand-checkable: "/tmp/foo" is exactly the "foo" RFC vector
        // prefixed by "/tmp/", so this cross-checks the composition
        // against a literal, not just against `base64_encode` calling
        // itself.
        assert_eq!(
            sequence(Path::new("/tmp/foo")),
            "\x1b]52;c;L3RtcC9mb28=\x07"
        );
    }

    #[test]
    fn sequence_handles_a_non_ascii_path() {
        let path = Path::new("/home/th\u{e9}o/caf\u{e9} \u{2615}.txt");
        let expected = format!(
            "\x1b]52;c;{}\x07",
            base64_encode("/home/th\u{e9}o/caf\u{e9} \u{2615}.txt".as_bytes())
        );
        assert_eq!(sequence(path), expected);
    }

    #[cfg(unix)]
    #[test]
    fn sequence_lossily_decodes_a_non_utf8_path_rather_than_failing() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/tmp/bad-\xff-name");
        let seq = sequence(Path::new(raw));
        let expected = format!(
            "\x1b]52;c;{}\x07",
            base64_encode(Path::new(raw).to_string_lossy().as_bytes())
        );
        assert_eq!(seq, expected);
        // The invalid byte became U+FFFD (replacement character) rather
        // than the encoder choking on it or smuggling the raw byte
        // through as if it were valid text.
        assert!(Path::new(raw).to_string_lossy().contains('\u{FFFD}'));
    }
}
