//! Pty-driven smoke tests for the interactive TUI event loop
//! (`camembert::ui::run`/`event_loop`): the one piece of the UI with zero
//! automated coverage before this file, because it needs a real terminal
//! (raw mode, the alternate screen, real crossterm event polling) that a
//! plain `Command` + pipes can't provide — pipes make `stdout` non-a-tty,
//! which routes the binary straight to `--no-ui` mode (already covered by
//! `tests/cli.rs`) instead of exercising this loop at all.
//!
//! Scope is deliberately shallow: these are not pixel/layout assertions
//! (that's what the unit-tested `draw_*`/`handle_key` functions in
//! `src/ui.rs` are for). The goal is proving the glue survives real
//! terminal I/O — starts, renders, takes keyboard input, and always
//! restores the terminal on the way out — across the two quit paths
//! (`q`, Ctrl-C) and a basic navigation keypress.
//!
//! Every test drives the *built binary* through a real pty
//! ([`portable_pty`], preferred over `expectrl` here: it's the
//! actively-maintained wezterm crate, has no dependency on an external
//! `expect`-like DSL, and its `MasterPty`/`Child` split lets a watchdog
//! thread hold an independent kill handle while the test thread blocks on
//! `wait()` — exactly the shape needed for the timeout guard below).
//! `portable-pty` is a dev-dependency of this crate only (see
//! `camembert/Cargo.toml`); no production code changed to make this
//! possible.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

/// Environment variables the binary reads config from (mirrors
/// `tests/cli.rs`'s `bin()`): stripped so a developer's shell environment
/// never bleeds into the pty session and makes a run non-deterministic.
const CONFIG_ENV_VARS: &[&str] = &[
    "SCAN_PATH",
    "THREADS",
    "ONE_FILESYSTEM",
    "STATX_ENGINE",
    "TOP",
    "NO_UI",
    "OUTPUT",
    "COLOR",
    "THEME",
    "NO_MOTION",
    "NO_PROC_SWEEP",
    "NO_FIEMAP",
    "FILTER",
    "LOG_FILTER",
    "LOG_FILE",
];

/// Realistic terminal size: wide enough (>= `MIN_WHEEL_TERMINAL_WIDTH` in
/// `src/ui.rs`, 100 cols) that the dashboard renders in its full layout —
/// table, wheel and all — rather than collapsing to the responsive
/// mini-donut header.
const PTY_SIZE: PtySize = PtySize {
    rows: 40,
    cols: 120,
    pixel_width: 0,
    pixel_height: 0,
};

/// How long a test is willing to wait for a pattern to show up in the
/// pty's output. Generous on purpose: CI machines under load can be a
/// lot slower than a laptop, and a spurious failure here is worse than a
/// slow one.
const RENDER_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a test waits for the child to actually exit after sending a
/// quit key.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the watchdog thread gives the whole session (spawn through
/// exit) before it force-kills the child — the backstop against a hung
/// event loop wedging the test suite instead of failing it.
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval for the read-until-pattern loops below. Short enough
/// that tests resolve quickly once the pattern appears, long enough to
/// not busy-spin.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A distinctive fixture directory name, asserted for verbatim in the
/// rendered header/path — distinguishing "the dashboard actually
/// rendered this scan" from any other string that might transiently
/// appear (e.g. the binary's own name).
const FIXTURE_DIR_NAME: &str = "cmbt-tui-smoke-fixture";

/// The exact sequence crossterm's `LeaveAlternateScreen` writes
/// (verified against `crossterm-0.29.0/src/terminal.rs`: `csi!("?1049l")`)
/// — `ratatui::restore()` (called unconditionally at the end of
/// `ui::run`) emits this to leave the alternate screen and hand the real
/// scrollback back to the terminal.
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// The corresponding `EnterAlternateScreen` sequence, emitted by
/// `ratatui::init()` at the top of `ui::run`.
const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";

/// Build a small fixture tree to scan: a couple of files and a
/// subdirectory are enough to give the dashboard something to draw
/// (metric cards, a non-empty table, a non-empty donut) without paying
/// for a real synthetic-tree-sized scan in every test.
fn build_fixture(root: &Path) -> std::path::PathBuf {
    let tree = root.join(FIXTURE_DIR_NAME);
    std::fs::create_dir_all(tree.join("subdir")).expect("create fixture subdir");
    std::fs::write(tree.join("subdir/a.txt"), vec![b'a'; 4096]).expect("write fixture file");
    std::fs::write(tree.join("top.txt"), vec![b'b'; 8192]).expect("write fixture file");
    tree
}

/// A live pty session driving the camembert binary: the master side (to
/// resize/read/write) plus the child handle, a background reader thread
/// continuously draining the pty into a shared buffer (ptys apply
/// backpressure to the child if nobody reads — the child must never be
/// left writing into a full pipe while the test thread is doing something
/// else), and a watchdog that kills the child if the whole session runs
/// long enough to suggest the event loop is hung.
struct PtySession {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    eof: Arc<AtomicBool>,
    /// Sending on this cancels the watchdog once the session finished
    /// normally; if the session hangs, the watchdog's `recv_timeout`
    /// simply expires and it kills `child` itself instead.
    watchdog_done: mpsc::Sender<()>,
}

impl PtySession {
    /// Spawn `camembert <tree> <extra_args>` inside a fresh pty of
    /// [`PTY_SIZE`]. `tree` is passed as the scan-path *argument*
    /// (absolute), not via the process's working directory — `ScanArgs`
    /// defaults `path` to `.` and the header prints exactly whatever
    /// string was given (`snapshot.path.display()`, see `draw_header` in
    /// `src/ui.rs`), so a bare cwd change would leave the header showing
    /// the literal `.` instead of anything identifying the fixture.
    /// Returns `Err` (with a reason, per the task's headless-skip
    /// requirement) only if the pty itself could not be created —
    /// spawning the binary or the pty working at all is expected to
    /// succeed in every real environment, headless included, since
    /// `portable-pty` talks to `/dev/ptmx` directly and needs no attached
    /// display or session leader.
    fn spawn(tree: &Path, extra_args: &[&str]) -> Result<Self, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PTY_SIZE)
            .map_err(|err| format!("openpty failed (no pty support in this sandbox?): {err}"))?;

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_camembert"));
        for var in CONFIG_ENV_VARS {
            cmd.env_remove(var);
        }
        // Deterministic capability ladder: truecolor + unicode glyphs, so
        // the header signature/table render their non-ASCII form
        // consistently regardless of the invoking shell's real TERM.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.arg(tree);
        for arg in extra_args {
            cmd.arg(arg);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| format!("failed to spawn camembert in the pty: {err}"))?;
        // Critical: drop our copy of the slave fd. The master's reader
        // only sees EOF once every slave-side fd is closed; holding onto
        // this one (even unused) after spawning would keep the reader
        // blocked forever after the child exits.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| format!("failed to clone the pty reader: {err}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| format!("failed to take the pty writer: {err}"))?;

        let output = Arc::new(Mutex::new(Vec::new()));
        let eof = Arc::new(AtomicBool::new(false));
        {
            let output = Arc::clone(&output);
            let eof = Arc::clone(&eof);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => output.lock().unwrap().extend_from_slice(&buf[..n]),
                    }
                }
                eof.store(true, Ordering::SeqCst);
            });
        }
        // Keep the master side alive for the whole session: dropping it
        // early can tear the pty down from under the reader thread. It's
        // a handful of pointers, reclaimed at process exit like the
        // reader thread's own stack — leaking it is the simplest way to
        // give it an unbounded lifetime without adding a field nothing
        // else in this struct needs to read again.
        Box::leak(pair.master);

        let (watchdog_done, watchdog_rx) = mpsc::channel::<()>();
        let mut killer = child.clone_killer();
        thread::spawn(move || {
            if watchdog_rx.recv_timeout(WATCHDOG_TIMEOUT).is_err() {
                eprintln!(
                    "tui_smoke watchdog: session exceeded {WATCHDOG_TIMEOUT:?}; killing the child"
                );
                let _ = killer.kill();
            }
        });

        Ok(Self {
            child,
            writer,
            output,
            eof,
            watchdog_done,
        })
    }

    /// Block (with bounded polling, never a fixed sleep as the only
    /// synchronization) until `pattern` appears anywhere in the output
    /// collected so far, or panic with the transcript-so-far once
    /// `timeout` elapses.
    fn wait_for(&self, pattern: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let text = String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned();
            if text.contains(pattern) {
                return text;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out after {timeout:?} waiting for {pattern:?} in the pty output; \
                     output so far ({} bytes):\n{text}",
                    text.len()
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Block until the reader thread observes EOF (every slave-side fd
    /// closed, i.e. the child fully exited) or `timeout` elapses.
    fn wait_eof(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.eof.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                let text = String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned();
                panic!(
                    "timed out after {timeout:?} waiting for the pty to reach EOF; \
                     output so far:\n{text}"
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().expect("flush pty writer");
    }

    /// Poll (bounded, never blocking indefinitely) for the child to
    /// finish, returning its exit status.
    fn wait_exit(&mut self, timeout: Duration) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) => {}
                Err(err) => panic!("error waiting for the child: {err}"),
            }
            if Instant::now() >= deadline {
                let text = String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned();
                panic!(
                    "camembert did not exit within {timeout:?} after the quit key; \
                     output so far:\n{text}"
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Cancel the watchdog now that the session is winding down normally.
    /// A failed send (the watchdog already fired and dropped its
    /// receiver) is not this method's problem to report — the exit-status
    /// assertion right after every call site already fails loudly in that
    /// case.
    fn disarm_watchdog(&self) {
        let _ = self.watchdog_done.send(());
    }
}

/// Skip (with a printed reason, never a silent pass) rather than fail
/// outright when the sandbox genuinely has no pty support — expected to
/// never actually trigger in CI or any real Linux environment, since
/// `native_pty_system()` here just needs `/dev/ptmx`, but the task calls
/// for graceful degradation over a hard failure if it ever does.
macro_rules! session_or_skip {
    ($session:expr, $test_name:literal) => {
        match $session {
            Ok(session) => session,
            Err(reason) => {
                eprintln!("skipping {}: {reason}", $test_name);
                return;
            }
        }
    };
}

#[test]
fn startup_then_clean_quit_restores_the_terminal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = build_fixture(dir.path());
    let mut session = session_or_skip!(
        PtySession::spawn(&tree, &[]),
        "startup_then_clean_quit_restores_the_terminal"
    );

    // Evidence the dashboard actually rendered: the fixture's own
    // directory name, which only ever appears in the header's path span
    // (`draw_header` in src/ui.rs) once a snapshot has been drawn.
    session.wait_for(FIXTURE_DIR_NAME, RENDER_TIMEOUT);
    // Alternate screen entered before anything is drawn (ratatui::init at
    // the top of ui::run) — sanity-check the pty transcript agrees before
    // we go looking for its counterpart at teardown.
    {
        let text = String::from_utf8_lossy(&session.output.lock().unwrap()).into_owned();
        assert!(
            text.contains(ENTER_ALT_SCREEN),
            "expected the alternate screen to have been entered by now: {text}"
        );
    }

    session.send(b"q");
    let status = session.wait_exit(EXIT_TIMEOUT);
    session.disarm_watchdog();
    assert!(
        status.success(),
        "camembert should exit 0 on a clean `q` quit, got {status:?}"
    );

    session.wait_eof(EXIT_TIMEOUT);
    let text = String::from_utf8_lossy(&session.output.lock().unwrap()).into_owned();
    assert!(
        text.contains(LEAVE_ALT_SCREEN),
        "terminal should be restored (alternate screen left) on quit: {text}"
    );
    // The restore must come after the initial entry, not just appear
    // somewhere in the stream by coincidence.
    assert!(
        text.rfind(LEAVE_ALT_SCREEN) > text.find(ENTER_ALT_SCREEN),
        "leave-alt-screen should follow enter-alt-screen: {text}"
    );
}

#[test]
fn ctrl_c_quits_cleanly_like_q() {
    // keymap.rs documents `q, Ctrl-C` as the two quit keys, and
    // ui::handle_key's normal-mode match confirms both return
    // `Action::Quit` identically (`KeyCode::Char('c')` +
    // `KeyModifiers::CONTROL` alongside plain `KeyCode::Char('q')`) — this
    // test drives the Ctrl-C path specifically, since it's the one a
    // real user reaches for out of habit and the one most likely to be
    // accidentally special-cased (e.g. treated as SIGINT instead of a key
    // event) if the raw-mode wiring ever regressed.
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = build_fixture(dir.path());
    let mut session =
        session_or_skip!(PtySession::spawn(&tree, &[]), "ctrl_c_quits_cleanly_like_q");

    session.wait_for(FIXTURE_DIR_NAME, RENDER_TIMEOUT);

    session.send(&[0x03]); // ETX / Ctrl-C
    let status = session.wait_exit(EXIT_TIMEOUT);
    session.disarm_watchdog();
    assert!(
        status.success(),
        "camembert should exit 0 on Ctrl-C, same as `q`, got {status:?}"
    );

    session.wait_eof(EXIT_TIMEOUT);
    let text = String::from_utf8_lossy(&session.output.lock().unwrap()).into_owned();
    assert!(
        text.contains(LEAVE_ALT_SCREEN),
        "terminal should be restored on a Ctrl-C quit too: {text}"
    );
}

#[test]
fn navigation_keypress_survives_and_then_quits() {
    // Not a pixel assertion: the goal is proving the loop survives a real
    // input event round-trip (read the key, mutate `UiState`, redraw)
    // rather than checking exactly what moved where — that's
    // `handle_key`'s and `UiState::move_down`'s own unit tests' job.
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = build_fixture(dir.path());
    let mut session = session_or_skip!(
        PtySession::spawn(&tree, &[]),
        "navigation_keypress_survives_and_then_quits"
    );

    session.wait_for(FIXTURE_DIR_NAME, RENDER_TIMEOUT);

    // `j` (down) is in keymap::SIMPLE, wired straight to
    // `UiState::move_down`; send a few to be sure the loop is actually
    // still alive and consuming events rather than having wedged right
    // after the first render.
    session.send(b"jjj");
    // Give the redraw a moment to happen, then confirm the dashboard is
    // still up (the fixture name still there) before quitting — proof
    // the process didn't crash on the keypress.
    session.wait_for(FIXTURE_DIR_NAME, RENDER_TIMEOUT);

    session.send(b"q");
    let status = session.wait_exit(EXIT_TIMEOUT);
    session.disarm_watchdog();
    assert!(
        status.success(),
        "camembert should exit 0 after navigating and quitting, got {status:?}"
    );

    session.wait_eof(EXIT_TIMEOUT);
    let text = String::from_utf8_lossy(&session.output.lock().unwrap()).into_owned();
    assert!(
        text.contains(LEAVE_ALT_SCREEN),
        "terminal should be restored after a navigate-then-quit session: {text}"
    );
}
