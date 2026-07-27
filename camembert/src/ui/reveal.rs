//! Reveal-in-file-manager (`o`): the bridge from "found the entry eating
//! disk" to "act on it outside camembert" — camembert never deletes
//! anything itself, on any platform, and Windows has no delete path at
//! all (see `ui.rs`'s module docs on the reclaim subsystem). `o` and `y`
//! (`super::clipboard`) are how a user gets from "found it" to "did
//! something about it" regardless.
//!
//! # Why not a shell
//!
//! The path comes straight from the scan — a directory a hostile or
//! merely creative filer named `& del *` or `; rm -rf ~` must be inert.
//! [`command_for`] never builds a shell line: `Command::new(program)` +
//! real argv entries only, so the OS's own argv boundary (never string
//! parsing) is what protects the caller. [`spawn`] never passes through
//! `cmd /c`, `sh -c`, or anything that re-interprets the path as text.
//!
//! # Platform arms
//!
//! - **Windows**: `explorer.exe /select,<path>` — selects the entry
//!   inside its parent folder. No space after the comma: that space is
//!   significant to Explorer's own command-line parsing (`/select, C:\x`
//!   opens `C:\x`'s parent instead of selecting `C:\x` inside it).
//! - **macOS**: `open -R <path>` — Finder's own reveal-and-select.
//! - **Every other Unix (Linux, the *BSDs)**: no portable "reveal and
//!   select" exists, so this opens the *parent* directory with
//!   `xdg-open` instead — the honest equivalent, not a workaround (see
//!   `ui.rs`'s module docs and the README's Keys table).
//!
//! [`command_for`] is a pure function (program + argv, no I/O) so the
//! exact command each platform builds is unit-testable without spawning
//! anything; [`spawn`] is the thin, deliberately untested wrapper that
//! actually launches it, detached from camembert's own stdio and never
//! waited on — a slow or wedged file manager must never block a frame.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// The program and argv `Command::new(program).args(args)` needs to
/// reveal `path` in the platform's file manager. See the module docs for
/// what each arm does and why.
#[cfg(windows)]
pub fn command_for(path: &Path) -> (OsString, Vec<OsString>) {
    // Built by pushing onto the `OsString` directly (never through a
    // `String`/`Display` round-trip) so a non-UTF-8 component survives
    // exactly, and no space is ever introduced after the comma.
    let mut arg = OsString::from("/select,");
    arg.push(path.as_os_str());
    (OsString::from("explorer.exe"), vec![arg])
}

/// macOS: `open -R` is Finder's own reveal-and-select — the entry itself,
/// not its parent.
#[cfg(target_os = "macos")]
pub fn command_for(path: &Path) -> (OsString, Vec<OsString>) {
    (
        OsString::from("open"),
        vec![OsString::from("-R"), path.as_os_str().to_owned()],
    )
}

/// Every other Unix: no portable "reveal and select" exists, so this
/// opens the *parent* directory instead — the honest equivalent (module
/// docs). Falls back to `path` itself when it has no parent (e.g. `/`)
/// rather than failing outright.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn command_for(path: &Path) -> (OsString, Vec<OsString>) {
    let target = path.parent().unwrap_or(path);
    (
        OsString::from("xdg-open"),
        vec![target.as_os_str().to_owned()],
    )
}

/// Spawn the platform's reveal command, fully detached: no inherited
/// stdin/stdout/stderr, and the child is never waited on. Only the spawn
/// call itself (`fork`/`exec`, or `CreateProcess`, failing) can surface an
/// error here — the caller reports it as a toast, never a panic and never
/// a silently swallowed failure.
pub fn spawn(path: &Path) -> io::Result<()> {
    let (program, args) = command_for(path);
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_selects_the_entry_inside_its_parent() {
        let (program, args) = command_for(Path::new(r"C:\Users\demo\big file.bin"));
        assert_eq!(program, OsString::from("explorer.exe"));
        assert_eq!(
            args,
            vec![OsString::from(r"/select,C:\Users\demo\big file.bin")]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_arg_has_no_space_after_the_comma() {
        let (_, args) = command_for(Path::new(r"C:\x"));
        let arg = args[0].to_string_lossy();
        assert!(arg.starts_with("/select,C:"), "{arg}");
        assert!(!arg.starts_with("/select, "), "no space after the comma");
    }

    #[cfg(windows)]
    #[test]
    fn windows_preserves_non_ascii_names_exactly() {
        // A non-ASCII (but valid Unicode, hence valid UTF-8/WTF-8) name —
        // the argv must carry it byte-for-byte, not a lossily "corrected"
        // substitute that could select the wrong file.
        let (_, args) = command_for(Path::new("C:\\r\u{e9}sum\u{e9}\\日本語.txt"));
        assert_eq!(
            args,
            vec![OsString::from("/select,C:\\r\u{e9}sum\u{e9}\\日本語.txt")]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reveals_the_entry_itself() {
        let (program, args) = command_for(Path::new("/Users/demo/file.txt"));
        assert_eq!(program, OsString::from("open"));
        assert_eq!(
            args,
            vec![OsString::from("-R"), OsString::from("/Users/demo/file.txt")]
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_opens_the_parent_directory_not_the_entry() {
        let (program, args) = command_for(Path::new("/home/demo/big-dir/file.bin"));
        assert_eq!(program, OsString::from("xdg-open"));
        assert_eq!(args, vec![OsString::from("/home/demo/big-dir")]);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_falls_back_to_the_path_itself_when_it_has_no_parent() {
        let (_, args) = command_for(Path::new("/"));
        assert_eq!(args, vec![OsString::from("/")]);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_preserves_non_utf8_bytes_exactly() {
        use std::os::unix::ffi::OsStrExt;
        // 0xFF is never valid UTF-8, but it is a perfectly legal byte in a
        // real Linux filename — the argv must carry it unchanged.
        let raw = std::ffi::OsStr::from_bytes(b"/home/demo/bad-\xffname");
        let (_, args) = command_for(Path::new(raw));
        assert_eq!(
            args,
            vec![OsString::from(std::ffi::OsStr::from_bytes(b"/home/demo"))]
        );
    }
}
