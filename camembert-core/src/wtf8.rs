//! WTF-8 → UTF-16, exactly or not at all.
//!
//! Windows filenames are sequences of 16-bit units that are *usually* valid
//! UTF-16 and are not required to be: an unpaired surrogate is a legal
//! NTFS name. Rust's `OsStr` stores those as
//! [WTF-8](https://simonsapin.github.io/wtf-8/) — UTF-8 generalised to
//! admit surrogate code points — and camembert interns
//! `OsStr::as_encoded_bytes` in the name arena, so a scanned name reaches
//! the tree as WTF-8 bytes.
//!
//! Getting *back* out is where std stops helping. It offers only
//! `OsStr::from_encoded_bytes_unchecked`, whose contract camembert cannot
//! honour (interned bytes may have come from a dump written on Linux,
//! where any byte string is a legal name), so
//! [`crate::tree::os_name_from_bytes`] used to fall back to
//! `String::from_utf8_lossy` — which turns an unpaired surrogate into
//! U+FFFD and hands the caller **a different name**. That is fine for a
//! label and wrong for anything that touches the filesystem again:
//! revealing, copying, or one day deleting the wrong entry.
//!
//! This module is the missing half. It is pure, portable, `unsafe`-free
//! and testable on every platform; the Windows-only step that follows it
//! (`OsString::from_wide`) lives at the call site.
//!
//! # Refuse, never guess
//!
//! [`wtf8_to_utf16`] returns [`None`] for anything that is not well-formed
//! WTF-8, rather than substituting a replacement character. A caller that
//! wants a lossy *display* string can still ask for one explicitly; a
//! caller that wants to name a file again gets a checkable "these bytes
//! did not come from this platform" instead of a plausible-looking lie.
//!
//! What is refused, and why each is not merely pedantry:
//!
//! - **Invalid lead/continuation bytes, truncated sequences.** Not UTF-8,
//!   not WTF-8, cannot be a Windows name.
//! - **Overlong encodings** (`C0 AF`, `E0 80 AF`, `F0 80 80 AF`). Two byte
//!   strings decoding to one name is exactly the ambiguity that makes a
//!   round-trip untrustworthy.
//! - **Scalar values above U+10FFFF** (`F5`..`FF`, or `F4 90 …`). No
//!   UTF-16 representation exists.
//! - **A surrogate pair written as two three-byte surrogates**
//!   (`ED A0 80 ED B0 80`). This is the one rule that separates WTF-8 from
//!   "UTF-8 with surrogates allowed": WTF-8 requires the four-byte form
//!   for a paired surrogate, so admitting the split form would again give
//!   one name two encodings. `OsString::from_wide` never produces it.
//!
//! An unpaired surrogate on its own — a high surrogate not followed by a
//! low one, or a low surrogate not preceded by a high one — is **accepted
//! and preserved**, which is the entire point of the module.

/// Decode well-formed WTF-8 into the UTF-16 units it denotes.
///
/// Returns `None` if `bytes` is not well-formed WTF-8; see the module docs
/// for the exact list and the reasoning. Valid UTF-8 is a subset and always
/// succeeds.
///
/// The result is what `OsStr::encode_wide` would have produced for the name
/// these bytes came from, so on Windows
/// `OsString::from_wide(&wtf8_to_utf16(name.as_encoded_bytes())?)` is the
/// identity, unpaired surrogates included.
pub fn wtf8_to_utf16(bytes: &[u8]) -> Option<Vec<u16>> {
    // One unit per byte is the worst case (all-ASCII); a four-byte
    // sequence yields two units, which is still ≤ its byte length.
    let mut out: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        // The scalar value (or surrogate code point) this sequence denotes,
        // plus how many bytes it consumed.
        let (code, width) = match b0 {
            0x00..=0x7F => (u32::from(b0), 1),
            // `C0`/`C1` can only ever encode an overlong ASCII byte.
            0xC2..=0xDF => {
                let b1 = continuation(bytes, i + 1)?;
                (((u32::from(b0) & 0x1F) << 6) | b1, 2)
            }
            0xE0..=0xEF => {
                let b1 = bytes.get(i + 1).copied()?;
                // `E0 80..9F` is overlong (it would re-encode U+0000..U+07FF).
                // Every other lead accepts the full continuation range —
                // including `ED A0..BF`, which is where surrogates live and
                // where plain UTF-8 would refuse.
                let low = if b0 == 0xE0 { 0xA0 } else { 0x80 };
                if !(low..=0xBF).contains(&b1) {
                    return None;
                }
                let b2 = continuation(bytes, i + 2)?;
                (
                    ((u32::from(b0) & 0x0F) << 12) | ((u32::from(b1) & 0x3F) << 6) | b2,
                    3,
                )
            }
            0xF0..=0xF4 => {
                let b1 = bytes.get(i + 1).copied()?;
                // `F0 80..8F` is overlong; `F4 90..BF` is above U+10FFFF.
                let (low, high) = match b0 {
                    0xF0 => (0x90, 0xBF),
                    0xF4 => (0x80, 0x8F),
                    _ => (0x80, 0xBF),
                };
                if !(low..=high).contains(&b1) {
                    return None;
                }
                let b2 = continuation(bytes, i + 2)?;
                let b3 = continuation(bytes, i + 3)?;
                (
                    ((u32::from(b0) & 0x07) << 18)
                        | ((u32::from(b1) & 0x3F) << 12)
                        | (b2 << 6)
                        | b3,
                    4,
                )
            }
            _ => return None,
        };
        i += width;
        if code < 0x1_0000 {
            let unit = code as u16;
            // WTF-8 well-formedness: a high surrogate immediately followed
            // by a low one must have been written as the four-byte form.
            if is_high_surrogate(unit) && bytes.get(i..i + 3).is_some_and(is_low_surrogate_seq) {
                return None;
            }
            out.push(unit);
        } else {
            let offset = code - 0x1_0000;
            out.push(0xD800 + (offset >> 10) as u16);
            out.push(0xDC00 + (offset & 0x3FF) as u16);
        }
    }
    Some(out)
}

/// The low six bits of `bytes[at]`, if that byte exists and is a
/// continuation byte (`10xxxxxx`).
fn continuation(bytes: &[u8], at: usize) -> Option<u32> {
    let byte = bytes.get(at).copied()?;
    (byte & 0xC0 == 0x80).then(|| u32::from(byte & 0x3F))
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

/// Whether `seq` is the three-byte encoding of a low surrogate
/// (`ED B0 80` .. `ED BF BF`).
fn is_low_surrogate_seq(seq: &[u8]) -> bool {
    seq.len() == 3 && seq[0] == 0xED && (0xB0..=0xBF).contains(&seq[1]) && seq[2] & 0xC0 == 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- what must be accepted ------------------------------------------

    #[test]
    fn ascii_and_multibyte_utf8_decode_as_utf16() {
        assert_eq!(wtf8_to_utf16(b""), Some(vec![]));
        assert_eq!(wtf8_to_utf16(b"ab"), Some(vec![0x61, 0x62]));
        // U+00E9 é, U+20AC €, U+1F9C0 🧀 (a surrogate pair).
        assert_eq!(
            wtf8_to_utf16("é€🧀".as_bytes()),
            Some(vec![0x00E9, 0x20AC, 0xD83E, 0xDDC0])
        );
    }

    /// The whole reason this module exists: an unpaired surrogate is a
    /// legal Windows filename and must survive the trip unchanged.
    #[test]
    fn lone_surrogates_survive_in_both_halves_of_the_range() {
        assert_eq!(wtf8_to_utf16(&[0xED, 0xA0, 0x80]), Some(vec![0xD800]));
        assert_eq!(wtf8_to_utf16(&[0xED, 0xAF, 0xBF]), Some(vec![0xDBFF]));
        assert_eq!(wtf8_to_utf16(&[0xED, 0xB0, 0x80]), Some(vec![0xDC00]));
        assert_eq!(wtf8_to_utf16(&[0xED, 0xBF, 0xBF]), Some(vec![0xDFFF]));
        // Embedded in an otherwise ordinary name.
        assert_eq!(
            wtf8_to_utf16(&[b'a', 0xED, 0xA0, 0x80, b'b']),
            Some(vec![0x61, 0xD800, 0x62])
        );
    }

    /// A high surrogate followed by something that is *not* a low
    /// surrogate stays a lone high surrogate — the refusal below is
    /// narrowly about the paired case.
    #[test]
    fn a_high_surrogate_followed_by_anything_else_is_fine() {
        assert_eq!(
            wtf8_to_utf16(&[0xED, 0xA0, 0x80, 0xED, 0xA0, 0x81]),
            Some(vec![0xD800, 0xD801]),
            "two high surrogates in a row are unpaired, not a pair"
        );
        assert_eq!(
            wtf8_to_utf16(&[0xED, 0xA0, 0x80, b'x']),
            Some(vec![0xD800, 0x78])
        );
        assert_eq!(
            wtf8_to_utf16(&[0xED, 0xB0, 0x80, 0xED, 0xA0, 0x80]),
            Some(vec![0xDC00, 0xD800]),
            "low then high is not a pair either"
        );
    }

    #[test]
    fn the_boundaries_of_each_sequence_length_decode() {
        assert_eq!(wtf8_to_utf16(&[0xC2, 0x80]), Some(vec![0x0080]));
        assert_eq!(wtf8_to_utf16(&[0xDF, 0xBF]), Some(vec![0x07FF]));
        assert_eq!(wtf8_to_utf16(&[0xE0, 0xA0, 0x80]), Some(vec![0x0800]));
        assert_eq!(wtf8_to_utf16(&[0xEF, 0xBF, 0xBF]), Some(vec![0xFFFF]));
        assert_eq!(
            wtf8_to_utf16(&[0xF0, 0x90, 0x80, 0x80]),
            Some(vec![0xD800, 0xDC00]),
            "U+10000, the first supplementary scalar"
        );
        assert_eq!(
            wtf8_to_utf16(&[0xF4, 0x8F, 0xBF, 0xBF]),
            Some(vec![0xDBFF, 0xDFFF]),
            "U+10FFFF, the last one"
        );
    }

    // ---- what must be refused -------------------------------------------

    /// The §2.8 cases from `docs/design/windows-delete-dossier.md`, each
    /// refused rather than mangled.
    #[test]
    fn non_wtf8_input_is_refused_not_guessed_at() {
        assert_eq!(wtf8_to_utf16(&[0xFF]), None, "no lead byte is 0xFF");
        assert_eq!(wtf8_to_utf16(&[0xC3]), None, "truncated two-byte lead");
        assert_eq!(wtf8_to_utf16(&[0xC0, 0xAF]), None, "overlong '/'");
        assert_eq!(wtf8_to_utf16(&[0x80]), None, "bare continuation byte");
    }

    #[test]
    fn every_overlong_form_is_refused() {
        assert_eq!(wtf8_to_utf16(&[0xC1, 0xBF]), None);
        assert_eq!(wtf8_to_utf16(&[0xE0, 0x80, 0xAF]), None);
        assert_eq!(wtf8_to_utf16(&[0xE0, 0x9F, 0xBF]), None);
        assert_eq!(wtf8_to_utf16(&[0xF0, 0x80, 0x80, 0xAF]), None);
        assert_eq!(wtf8_to_utf16(&[0xF0, 0x8F, 0xBF, 0xBF]), None);
    }

    #[test]
    fn scalars_above_the_unicode_range_have_no_utf16_form_and_are_refused() {
        assert_eq!(wtf8_to_utf16(&[0xF4, 0x90, 0x80, 0x80]), None);
        assert_eq!(wtf8_to_utf16(&[0xF5, 0x80, 0x80, 0x80]), None);
        assert_eq!(wtf8_to_utf16(&[0xF8, 0x88, 0x80, 0x80, 0x80]), None);
    }

    #[test]
    fn truncated_and_malformed_sequences_are_refused() {
        assert_eq!(wtf8_to_utf16(&[0xE0, 0xA0]), None);
        assert_eq!(wtf8_to_utf16(&[0xF0, 0x90, 0x80]), None);
        assert_eq!(
            wtf8_to_utf16(&[0xE0, 0xA0, 0x41]),
            None,
            "not a continuation"
        );
        assert_eq!(wtf8_to_utf16(&[b'a', 0xC2]), None, "truncated at the end");
    }

    /// The rule that makes the encoding unambiguous: a *paired* surrogate
    /// written as two three-byte surrogates is ill-formed WTF-8, because
    /// the four-byte form already denotes it.
    #[test]
    fn a_split_surrogate_pair_is_refused_so_one_name_has_one_encoding() {
        assert_eq!(wtf8_to_utf16(&[0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]), None);
        assert_eq!(wtf8_to_utf16(&[0xED, 0xAF, 0xBF, 0xED, 0xBF, 0xBF]), None);
        // The same code points via the four-byte form are the accepted way
        // to write exactly this name.
        assert_eq!(
            wtf8_to_utf16(&[0xF0, 0x90, 0x80, 0x80]),
            Some(vec![0xD800, 0xDC00])
        );
    }

    // ---- against std's own encoder ---------------------------------------

    /// The property the whole module claims, checked against the real
    /// encoder rather than a hand-written one: for *any* UTF-16 sequence,
    /// unpaired surrogates included, `OsString::from_wide` →
    /// `as_encoded_bytes` → [`wtf8_to_utf16`] is the identity.
    ///
    /// Deterministic pseudo-random (xorshift64, fixed seed) so a failure
    /// is reproducible rather than a one-off CI ghost.
    #[cfg(windows)]
    #[test]
    fn random_utf16_round_trips_through_std_wtf8() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let mut seed: u64 = 0x2026_0728_C0FF_EE01;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let len = (next() % 12) as usize;
            let units: Vec<u16> = (0..len)
                .map(|_| {
                    let r = next();
                    // Heavily biased towards the surrogate range: uniform
                    // u16 would make the interesting case ~3 % of draws.
                    if r % 3 == 0 {
                        0xD800 + (r >> 8) as u16 % 0x0800
                    } else {
                        (r >> 16) as u16
                    }
                })
                .collect();
            let name = OsString::from_wide(&units);
            let bytes = name.as_encoded_bytes();
            assert_eq!(
                wtf8_to_utf16(bytes).as_deref(),
                Some(&units[..]),
                "round-trip failed for {units:04X?} (bytes {bytes:02X?})"
            );
        }
    }
}
