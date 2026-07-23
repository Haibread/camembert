//! OSC 11 background-color query (design slice 6, design §Color and
//! capabilities): asks the terminal for its current background color so
//! `--theme`/`THEME`/`camembert.toml` can be left unset and still get a
//! sensible `light` theme on a light terminal.
//!
//! The reply arrives on stdin exactly like a keystroke would — the
//! terminal has no other channel to answer on — so this is bounded by a
//! short [`TIMEOUT`] and any terminal that never answers (most of them:
//! OSC 11 support is real but not universal) is treated as dark, the
//! same as today's default. The query only ever runs before
//! `ratatui::init` touches the terminal ([`super::resolve_theme_name`]),
//! and only when [`should_query`] says the terminal looks capable of
//! answering at all.
//!
//! In canonical (cooked) mode the reply would never satisfy a read at
//! all — the tty line-discipline buffers until a newline the terminal
//! never sends — and echo would print the raw escape bytes to the
//! screen. So [`read_reply_bounded`] puts stdin into a minimal raw
//! window itself (`ICANON`/`ECHO` off, `VMIN=0`/`VTIME=1` so each read
//! attempt returns within ~100ms whether or not a byte showed up) on the
//! calling thread — no detached reader, nothing left running past this
//! function that could steal a keystroke — and [`TermiosGuard`]
//! restores the original settings on every exit path.
//!
//! The testable seams are [`parse_reply`] (byte parsing),
//! [`relative_luminance`]/[`is_light`] (the threshold), and
//! [`read_until_terminator`] (the deadline/terminator loop, exercised
//! against a scripted [`Read`] impl); the actual terminal round-trip in
//! [`query_terminal_background`] (including the termios save/restore)
//! is a thin, deliberately untested wrapper around them — there is no
//! tty in a test process to query.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use rustix::termios::{self, LocalModes, OptionalActions, SpecialCodeIndex};
use tracing::debug;

/// How long to wait for a reply before assuming the terminal will not
/// answer. Long enough for a real terminal's round-trip (local ptys
/// answer in well under 10ms; even an ssh hop rarely exceeds this),
/// short enough that a silent terminal never makes startup feel stuck.
const TIMEOUT: Duration = Duration::from_millis(150);

/// Hard cap on the reply buffer: the longest valid reply
/// (`\x1b]11;rgb:ffff/ffff/ffff\x1b\\`) is 26 bytes. Anything longer
/// without a terminator is garbage — stop reading rather than buffer
/// unboundedly from a misbehaving terminal (or a user typing).
const MAX_REPLY_LEN: usize = 64;

/// Whether attempting the query is even worth it: a pipe (not a tty on
/// either end) would never answer and a read against it can behave
/// unpredictably, and `TERM=dumb`/unset terminals are not expected to
/// implement any OSC sequence.
pub fn should_query(term: Option<&str>, stdin_is_tty: bool, stdout_is_tty: bool) -> bool {
    stdin_is_tty && stdout_is_tty && !matches!(term, None | Some("dumb"))
}

/// Relative luminance (ITU-R BT.709 coefficients on linearized sRGB
/// channels, the standard WCAG formula) — `> 0.5` reads as a light
/// background to the eye.
pub fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    let channel = |c: u8| -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// Whether an RGB background reads as light (design: "relative luminance
/// of the reported color > 0.5").
pub fn is_light(r: u8, g: u8, b: u8) -> bool {
    relative_luminance(r, g, b) > 0.5
}

/// Parse an OSC 11 reply's body — the bytes between `\x1b]11;` and the
/// BEL/ST terminator, both already stripped by [`extract_body`] — into
/// 8-bit RGB. Accepts both reply formats terminals use in practice:
/// `rgb:RRRR/GGGG/BBBB` (16 bit per channel) and `rgb:RR/GG/BB` (8 bit);
/// anything else (a different color model, wrong channel count, non-hex
/// digits, an empty channel) is rejected rather than guessed at.
pub fn parse_reply(body: &str) -> Option<(u8, u8, u8)> {
    let rest = body.strip_prefix("rgb:")?;
    let mut parts = rest.split('/');
    let r = parts.next()?;
    let g = parts.next()?;
    let b = parts.next()?;
    if parts.next().is_some() {
        return None; // more than three channels: not a color we understand
    }
    let channel = |s: &str| -> Option<u8> {
        if s.is_empty() || s.len() > 4 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(s, 16).ok()?;
        let max = (1u32 << (4 * s.len())) - 1;
        Some(((value * 255) / max) as u8)
    };
    Some((channel(r)?, channel(g)?, channel(b)?))
}

/// Extract the OSC 11 body from a raw reply, terminated by BEL (`\x07`)
/// or ST (`\x1b\\`). `None` for anything that does not look like an OSC
/// 11 response at all: garbage, a different OSC number, or a truncated
/// read that never reached a terminator.
pub fn extract_body(raw: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(raw).ok()?;
    let text = text.strip_prefix("\x1b]11;")?;
    text.strip_suffix('\x07')
        .or_else(|| text.strip_suffix("\x1b\\"))
}

/// Query the real terminal for its background color. Writes the OSC 11
/// query, reads a bounded reply, and combines [`extract_body`] +
/// [`parse_reply`]; `None` on any failure (raw-mode setup failed, write
/// failed, no reply within [`TIMEOUT`], or a reply that did not parse).
///
/// The read happens on the calling thread, with stdin switched to a
/// minimal raw window for the duration (see [`TermiosGuard`]) — no
/// detached thread, so nothing can outlive this function and steal a
/// later keystroke. Non-canonical mode with `VMIN=0`/`VTIME=1` makes
/// each individual `read` return within ~100ms regardless of whether a
/// byte arrived, so [`read_until_terminator`] can honor the overall
/// [`TIMEOUT`] just by checking a deadline between attempts.
pub fn query_terminal_background() -> Option<(u8, u8, u8)> {
    let raw = read_reply_bounded(TIMEOUT)?;
    let body = extract_body(&raw)?;
    parse_reply(body)
}

/// RAII guard that restores stdin's original termios settings on drop —
/// on every exit path out of [`read_reply_bounded`] (success, timeout,
/// an early `?` return, or a panic unwind) — so the raw window never
/// outlives this one query.
///
/// Holds the raw descriptor rather than a borrowed [`AsFd`] handle:
/// [`rustix::fd::BorrowedFd`] ties its lifetime to the value it
/// borrowed from, which would keep `stdin` immutably borrowed for as
/// long as the guard lives — but [`read_reply_bounded`] still needs a
/// `&mut` on stdin to read from it after the guard is constructed.
/// Stdin's descriptor is open for the whole process lifetime and this
/// guard never outlives the function that created it, so re-borrowing
/// it as needed in [`Drop::drop`] is sound.
struct TermiosGuard {
    fd: std::os::fd::RawFd,
    original: termios::Termios,
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        // Safety: `fd` is stdin's descriptor (fd 0), which stays open
        // for the process's entire life; this guard is local to
        // `read_reply_bounded` and never outlives it.
        let fd = unsafe { rustix::fd::BorrowedFd::borrow_raw(self.fd) };
        if let Err(err) = termios::tcsetattr(fd, OptionalActions::Now, &self.original) {
            debug!(?err, "OSC 11: failed to restore terminal settings");
        }
    }
}

fn read_reply_bounded(timeout: Duration) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let raw_fd = stdin.as_raw_fd();
    let fd = stdin.as_fd();

    let original = match termios::tcgetattr(fd) {
        Ok(t) => t,
        Err(err) => {
            debug!(?err, "OSC 11: tcgetattr failed; skipping query");
            return None;
        }
    };
    let mut raw = original.clone();
    raw.local_modes
        .remove(LocalModes::ICANON | LocalModes::ECHO);
    raw.special_codes[SpecialCodeIndex::VMIN] = 0;
    raw.special_codes[SpecialCodeIndex::VTIME] = 1; // ~100ms per read attempt
    if let Err(err) = termios::tcsetattr(fd, OptionalActions::Now, &raw) {
        debug!(?err, "OSC 11: tcsetattr failed; skipping query");
        return None;
    }
    // From here on, every exit (including the `?`s below) restores the
    // original settings via `TermiosGuard::drop`. `fd`'s borrow of
    // `stdin` ends with this statement (its last use), freeing `stdin`
    // up for the `&mut` read loop below.
    let _guard = TermiosGuard {
        fd: raw_fd,
        original,
    };

    let mut stdout = std::io::stdout();
    stdout.write_all(b"\x1b]11;?\x07").ok()?;
    stdout.flush().ok()?;

    let deadline = Instant::now() + timeout;
    let reply = read_until_terminator(&mut stdin, deadline);
    if reply.is_none() {
        debug!("OSC 11 query timed out or the terminal never answered");
    }
    reply
}

/// Read bytes one at a time from `reader` until a BEL/ST terminator,
/// [`MAX_REPLY_LEN`], or `deadline` passes, whichever comes first.
///
/// `reader` is expected to return `Ok(0)` for "no byte available on
/// this attempt, try again" rather than treating that as EOF — exactly
/// what a raw-mode fd with `VMIN=0`/`VTIME=1` yields when a `read`
/// attempt times out — so this loop spins on that at negligible cost
/// until `deadline`.
fn read_until_terminator<R: Read>(reader: &mut R, deadline: Instant) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(32);
    let mut byte = [0u8; 1];
    while Instant::now() < deadline {
        match reader.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\x07") || buf.ends_with(b"\x1b\\") || buf.len() >= MAX_REPLY_LEN
                {
                    return Some(buf);
                }
            }
            Ok(_) => continue, // VMIN=0/VTIME=1 timeout: no byte this attempt
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_query_requires_a_real_terminal_on_both_ends() {
        assert!(should_query(Some("xterm-256color"), true, true));
        assert!(
            !should_query(Some("xterm-256color"), false, true),
            "stdin not a tty"
        );
        assert!(
            !should_query(Some("xterm-256color"), true, false),
            "stdout not a tty"
        );
        assert!(!should_query(Some("dumb"), true, true), "TERM=dumb");
        assert!(!should_query(None, true, true), "TERM unset");
    }

    #[test]
    fn luminance_known_values() {
        assert!((relative_luminance(255, 255, 255) - 1.0).abs() < 1e-9);
        assert!((relative_luminance(0, 0, 0) - 0.0).abs() < 1e-9);
        assert!(!is_light(0, 0, 0), "black is dark");
        assert!(is_light(255, 255, 255), "white is light");
    }

    #[test]
    fn luminance_threshold_matches_common_backgrounds() {
        // Tokyo Night's own background (#1a1b26): clearly dark.
        assert!(!is_light(0x1a, 0x1b, 0x26));
        // A typical light-terminal background (#e1e2e7, tokyonight-day):
        // clearly light.
        assert!(is_light(0xe1, 0xe2, 0xe7));
        // A mid grey right at the boundary should not panic and should
        // land on one side deterministically.
        let _ = is_light(0x80, 0x80, 0x80);
    }

    #[test]
    fn parse_reply_16_bit_channels() {
        assert_eq!(parse_reply("rgb:1a1a/1b1b/2626"), Some((0x1a, 0x1b, 0x26)));
        assert_eq!(parse_reply("rgb:ffff/ffff/ffff"), Some((0xff, 0xff, 0xff)));
        assert_eq!(parse_reply("rgb:0000/0000/0000"), Some((0, 0, 0)));
    }

    #[test]
    fn parse_reply_8_bit_channels() {
        assert_eq!(parse_reply("rgb:e1/e2/e7"), Some((0xe1, 0xe2, 0xe7)));
        assert_eq!(parse_reply("rgb:ff/ff/ff"), Some((0xff, 0xff, 0xff)));
    }

    #[test]
    fn parse_reply_rejects_garbage() {
        assert_eq!(parse_reply(""), None);
        assert_eq!(parse_reply("not-a-color"), None);
        assert_eq!(parse_reply("rgba:11/22/33"), None, "wrong prefix");
        assert_eq!(parse_reply("rgb:11/22"), None, "too few channels");
        assert_eq!(parse_reply("rgb:11/22/33/44"), None, "too many channels");
        assert_eq!(parse_reply("rgb:zz/22/33"), None, "non-hex digit");
        assert_eq!(parse_reply("rgb:/22/33"), None, "empty channel");
        assert_eq!(parse_reply("rgb:11111/22/33"), None, "channel too wide");
    }

    #[test]
    fn extract_body_handles_both_terminators() {
        assert_eq!(
            extract_body(b"\x1b]11;rgb:1a1a/1b1b/2626\x07"),
            Some("rgb:1a1a/1b1b/2626")
        );
        assert_eq!(
            extract_body(b"\x1b]11;rgb:1a1a/1b1b/2626\x1b\\"),
            Some("rgb:1a1a/1b1b/2626")
        );
    }

    #[test]
    fn extract_body_rejects_wrong_shape() {
        assert_eq!(extract_body(b""), None, "empty");
        assert_eq!(extract_body(b"garbage"), None, "not an escape sequence");
        assert_eq!(
            extract_body(b"\x1b]10;rgb:11/22/33\x07"),
            None,
            "OSC 10 (foreground), not 11"
        );
        assert_eq!(
            extract_body(b"\x1b]11;rgb:1a1a/1b1b/2626"),
            None,
            "truncated: no terminator"
        );
        assert_eq!(extract_body(&[0xff, 0xfe]), None, "invalid UTF-8");
    }

    #[test]
    fn full_pipeline_parses_a_realistic_reply() {
        let raw = b"\x1b]11;rgb:1a1a/1b1b/2626\x07";
        let body = extract_body(raw).expect("valid reply shape");
        let (r, g, b) = parse_reply(body).expect("valid color");
        assert!(!is_light(r, g, b));
    }

    /// A scripted [`Read`] standing in for a raw-mode fd with
    /// `VMIN=0`/`VTIME=1`: yields the queued bytes one at a time, then
    /// `Ok(0)` forever after (a "no byte this attempt" timeout) rather
    /// than `Ok(0)` meaning EOF, or optionally an error.
    struct ScriptedReader {
        bytes: std::collections::VecDeque<u8>,
        then_error: bool,
    }

    impl ScriptedReader {
        fn bytes(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.iter().copied().collect(),
                then_error: false,
            }
        }

        fn erroring_after(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.iter().copied().collect(),
                then_error: true,
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.bytes.pop_front() {
                Some(byte) => {
                    buf[0] = byte;
                    Ok(1)
                }
                None if self.then_error => Err(std::io::Error::other("scripted read error")),
                None => Ok(0), // VMIN=0/VTIME=1 timeout, not EOF
            }
        }
    }

    /// A generous deadline for scenarios that are expected to return
    /// before it ever matters, so the test does not depend on timing.
    fn far_future_deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn read_until_terminator_stops_at_bel() {
        let mut reader = ScriptedReader::bytes(b"\x1b]11;rgb:1a1a/1b1b/2626\x07");
        let result = read_until_terminator(&mut reader, far_future_deadline());
        assert_eq!(result, Some(b"\x1b]11;rgb:1a1a/1b1b/2626\x07".to_vec()));
    }

    #[test]
    fn read_until_terminator_stops_at_st() {
        let mut reader = ScriptedReader::bytes(b"\x1b]11;rgb:1a1a/1b1b/2626\x1b\\");
        let result = read_until_terminator(&mut reader, far_future_deadline());
        assert_eq!(result, Some(b"\x1b]11;rgb:1a1a/1b1b/2626\x1b\\".to_vec()));
    }

    #[test]
    fn read_until_terminator_stops_at_max_reply_len_without_terminator() {
        let garbage = vec![b'x'; MAX_REPLY_LEN + 16];
        let mut reader = ScriptedReader::bytes(&garbage);
        let result = read_until_terminator(&mut reader, far_future_deadline());
        assert_eq!(result, Some(vec![b'x'; MAX_REPLY_LEN]));
    }

    #[test]
    fn read_until_terminator_gives_up_at_the_deadline_when_silent() {
        // Every read reports "no byte this attempt" (VMIN=0/VTIME=1
        // timeout), simulating a terminal that never answers at all.
        let mut reader = ScriptedReader::bytes(b"");
        let deadline = Instant::now() + Duration::from_millis(20);
        let result = read_until_terminator(&mut reader, deadline);
        assert_eq!(result, None);
    }

    #[test]
    fn read_until_terminator_stops_immediately_on_read_error() {
        let mut reader = ScriptedReader::erroring_after(b"\x1b]11;rgb:");
        // A far-future deadline would make a timeout-based failure take
        // forever to observe in a test; the error path must return
        // immediately instead of waiting for it.
        let result = read_until_terminator(&mut reader, far_future_deadline());
        assert_eq!(result, None);
    }
}
