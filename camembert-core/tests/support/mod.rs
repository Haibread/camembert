//! Shared cross-platform fixtures for `camembert-core` integration tests.
//!
//! A handful of test setups (a symlink, an unreadable directory, a name
//! that is not valid UTF-8) need a genuinely different mechanism per
//! platform to mean the same thing to the scanner. Centralizing them here
//! keeps that platform split out of the test bodies themselves.

#![allow(dead_code)] // not every test file uses every helper.

use std::ffi::OsString;
use std::path::Path;

/// A file name in the platform's "not valid UTF-8" corner: a raw invalid
/// UTF-8 byte on Unix (`OsStr::from_bytes`, e.g. a lone `0xE9`), an
/// unpaired UTF-16 surrogate on Windows (`OsString::from_wide`). Different
/// mechanisms, same property — `str::from_utf8` rejects the encoded form
/// either way (WTF-8 encodes a lone surrogate as a 3-byte sequence in the
/// `ED A0 80`..=`ED BF BF` range, which is specifically excluded from valid
/// UTF-8) — so both exercise the scanner's non-UTF-8-name path: the raw
/// interned bytes must survive end to end (scan, dump, diff) without ever
/// being assumed to be `str`.
#[cfg(unix)]
pub fn non_utf8_name() -> OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(b"caf\xe9").to_os_string()
}

#[cfg(windows)]
pub fn non_utf8_name() -> OsString {
    use std::os::windows::ffi::OsStringExt;
    // "caf" + an unpaired low surrogate (0xDC00): not valid UTF-16, and NTFS
    // does not validate surrogate pairing in file names, so this round-trips
    // through `CreateFileW` fine — it is a legal Windows name.
    let mut wide: Vec<u16> = "caf".encode_utf16().collect();
    wide.push(0xDC00);
    OsString::from_wide(&wide)
}

/// Create a symlink to a file. Returns whether it succeeded — on Windows
/// this needs Developer Mode or an elevated process
/// (`SeCreateSymbolicLinkPrivilege`), which a CI runner may not have.
/// Callers must skip the test with a clear message when this returns
/// `false` rather than failing.
#[cfg(unix)]
pub fn symlink_file(target: impl AsRef<Path>, link: impl AsRef<Path>) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(windows)]
pub fn symlink_file(target: impl AsRef<Path>, link: impl AsRef<Path>) -> bool {
    std::os::windows::fs::symlink_file(target, link).is_ok()
}

/// Create a symlink to a directory. See [`symlink_file`] for the Windows
/// privilege caveat.
#[cfg(unix)]
pub fn symlink_dir(target: impl AsRef<Path>, link: impl AsRef<Path>) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(windows)]
pub fn symlink_dir(target: impl AsRef<Path>, link: impl AsRef<Path>) -> bool {
    std::os::windows::fs::symlink_dir(target, link).is_ok()
}

/// Create a hard link to a file. Returns whether it succeeded, so callers
/// can skip with a clear message rather than fail.
///
/// `std::fs::hard_link` is `link(2)` on Unix and `CreateHardLinkW` on
/// Windows — the same call `mklink /H` makes, and like it, it needs no
/// privilege. It does need a filesystem that has the concept: NTFS and
/// ReFS do, FAT/exFAT (a USB stick as `TMPDIR`) and most network
/// filesystems do not, and that is a skip, not a failure.
pub fn hard_link(target: impl AsRef<Path>, link: impl AsRef<Path>) -> bool {
    std::fs::hard_link(target, link).is_ok()
}

/// Make a directory unreadable to the current process and return whether
/// it actually ended up unreadable — chmod 000 does not stop root, and a
/// missing/broken `icacls` should not silently pass a test.
///
/// Unix: `chmod 000`. Windows: std exposes no ACL API, so this shells out
/// to `icacls` to add an explicit deny-read/list ACE for the current user
/// (`/inheritance:r` first, so no inherited ACE re-grants access).
#[cfg(unix)]
pub fn make_unreadable(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000));
    std::fs::read_dir(dir).is_err()
}

#[cfg(windows)]
pub fn make_unreadable(dir: &Path) -> bool {
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    let _ = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/inheritance:r")
        .output();
    let deny = format!("{user}:(OI)(CI)(RX,RD)");
    let _ = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/deny")
        .arg(&deny)
        .output();
    std::fs::read_dir(dir).is_err()
}

/// Undo [`make_unreadable`], restoring normal read access. Best-effort:
/// callers use this for cleanup (e.g. so `TempDir` can remove the fixture
/// afterward), not as a correctness assertion.
#[cfg(unix)]
pub fn restore_readable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
}

#[cfg(windows)]
pub fn restore_readable(dir: &Path) {
    let _ = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/reset")
        .output();
}
