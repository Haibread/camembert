//! The Recycle Bin meter — Windows' answer to "space you think you freed".
//!
//! Linux's [`crate::freeable`] sweep answers that question with
//! deleted-but-still-open files: `df` counts their blocks, no directory
//! tree can show them. Windows has no such sweep and does not need one for
//! this purpose, because it has a *bigger* and completely visible version
//! of the same gap: **the Recycle Bin**. A user who deleted 5.83 GiB in
//! Explorer believes that space came back; the free-space figure the disk
//! gauge draws says otherwise, and no scan of any directory tree explains
//! the difference (`C:\$Recycle.Bin` is hidden, per-SID and ACL'd).
//!
//! `SHQueryRecycleBinW` answers it in one call: read-only, unelevated, on
//! every fixed volume including FAT32 (measured — see
//! `docs/design/windows-delete-dossier.md` §2.5h/§4.3). It moves nothing,
//! opens nothing, and cannot empty anything. Neither can camembert: this
//! module has no write path and is not a step towards one.
//!
//! # What the figure is, and what it is not
//!
//! It is **recoverable by the user**, not free space, and every surface
//! that renders it says so. Nothing here is called "freeable": the bytes
//! are not released until the user empties the bin, which is an action
//! camembert never takes and never offers. Calling it freeable would be
//! the exact class of claim the project exists to refuse.
//!
//! # Scope
//!
//! One volume — the one containing the scan root, resolved with
//! `GetVolumePathNameW`, which is the same volume `ui::disk_space`'s
//! `GetDiskFreeSpaceExW` gauge describes. A scan spanning several volumes
//! therefore under-reports the machine's total recycled bytes, exactly as
//! the Linux gauge's freeable figure is scoped to the root filesystem
//! (freeable D2). Reporting the sum across every volume would put bytes
//! from one disk onto another disk's gauge.
//!
//! # Degradation
//!
//! Every failure returns [`None`] with a `tracing` debug line and no error
//! for the caller to handle (freeable D7's contract). A volume with no
//! Recycle Bin at all — a network share, a removable stick with the bin
//! disabled — is one of those failures, and reads as "nothing to say"
//! rather than "zero bytes recycled". Note the converse, which is why this
//! is a *size* oracle and not an *availability* one: the call also returns
//! `S_OK` with zeros on a volume whose bin is merely empty, and those two
//! are indistinguishable from here.

use std::path::{Path, PathBuf};

use tracing::debug;
use windows_sys::Win32::Foundation::S_OK;
use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
use windows_sys::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};

/// `MAX_PATH` + room for the NUL, which is what `GetVolumePathNameW`
/// documents as always sufficient for a mount-point path.
const VOLUME_BUF: usize = 261;

/// What one volume's Recycle Bin holds.
///
/// Plain data, no handles, no lifetimes — it crosses a thread boundary to
/// the UI exactly as [`crate::freeable::Ledger`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinStatus {
    /// The volume mount point queried, e.g. `C:\`. Kept so a surface can
    /// name it when a session spans more than one volume.
    pub volume: PathBuf,
    /// Bytes the bin holds. **Not free space**: still allocated, still
    /// counted as used by `GetDiskFreeSpaceExW`, until the user empties
    /// the bin.
    pub bytes: u64,
    /// How many items those bytes are spread over. A recycled *directory*
    /// counts as one item however many files it contains.
    pub items: u64,
}

impl BinStatus {
    /// Whether there is anything at all to say. A bin reporting zero bytes
    /// is real information ("nothing recycled here") but not information
    /// worth spending a gauge suffix on.
    pub fn is_empty(&self) -> bool {
        self.bytes == 0 && self.items == 0
    }
}

/// Query the Recycle Bin of the volume containing `scan_root`.
///
/// Returns `None` on any failure, including a volume that has no bin.
/// Blocking: the call enumerates the bin's metadata, so callers run it off
/// the UI thread (see `camembert/src/ui/recycle_rt.rs`).
pub fn query(scan_root: &Path) -> Option<BinStatus> {
    let volume = volume_root(scan_root)?;
    let mut wide: Vec<u16> = shell_path(&volume);
    wide.push(0);
    let mut info = SHQUERYRBINFO {
        cbSize: u32::try_from(size_of::<SHQUERYRBINFO>()).expect("SHQUERYRBINFO fits"),
        ..Default::default()
    };
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the
    // synchronous call, and `info` is a live, fully initialised structure
    // whose `cbSize` states its own size — the only contract the API
    // documents. The call is read-only: it reports the bin's contents and
    // has no disposition, deletion or restore behaviour of any kind.
    let hr = unsafe { SHQueryRecycleBinW(wide.as_ptr(), &raw mut info) };
    if hr != S_OK {
        debug!(
            volume = %volume.display(),
            hresult = format!("{hr:#010x}"),
            "recycle bin query refused; no figure will be shown"
        );
        return None;
    }
    Some(BinStatus {
        volume,
        bytes: u64::try_from(info.i64Size).unwrap_or(0),
        items: u64::try_from(info.i64NumItems).unwrap_or(0),
    })
}

/// The mount point of the volume containing `path`, e.g. `C:\`.
fn volume_root(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let mut input: Vec<u16> = path.as_os_str().encode_wide().collect();
    input.push(0);
    let mut out = [0u16; VOLUME_BUF];
    // SAFETY: `input` is NUL-terminated and outlives the call; `out` is a
    // live buffer and the length passed is exactly its element count, so
    // nothing is written past it.
    let ok = unsafe {
        GetVolumePathNameW(
            input.as_ptr(),
            out.as_mut_ptr(),
            u32::try_from(out.len()).expect("VOLUME_BUF fits"),
        )
    };
    if ok == 0 {
        debug!(path = %path.display(), "could not resolve the volume for the recycle bin query");
        return None;
    }
    let len = out.iter().position(|&unit| unit == 0).unwrap_or(out.len());
    Some(PathBuf::from(OsString::from_wide(&out[..len])))
}

/// Strip a `\\?\` prefix, because shell APIs reject it.
///
/// The Windows scan backend carries `\\?\`-prefixed paths everywhere to
/// escape `MAX_PATH`, and `GetVolumePathNameW` faithfully hands the prefix
/// back on the volume root it returns. `SHQueryRecycleBinW` is a shell
/// entry point and the extended prefix is exactly what shell entry points
/// refuse (delete dossier §2.5e measured `SHCreateItemFromParsingName`
/// answering `E_INVALIDARG` to one). A volume root is at most a few
/// characters, so nothing is lost by dropping it here.
///
/// Kept as a separate, pure step so the rule is visible and testable
/// rather than hidden inside the call.
fn shell_path(volume: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let text = volume.as_os_str();
    let wide: Vec<u16> = text.encode_wide().collect();
    match strip_extended_prefix(&wide) {
        Some(rest) => rest.to_vec(),
        None => wide,
    }
}

/// `\\?\C:\` -> `C:\`; `\\?\UNC\server\share\` and anything else -> `None`
/// (a UNC volume has no Recycle Bin to query, so leaving the prefix on and
/// letting the call refuse is the honest outcome).
fn strip_extended_prefix(wide: &[u16]) -> Option<&[u16]> {
    const PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    let rest = wide.strip_prefix(&PREFIX[..])?;
    // Only a drive-letter volume root, never `UNC\…` or a volume GUID.
    let is_drive = matches!(rest, [letter, colon, ..]
        if *colon == b':' as u16
            && char::from_u32(u32::from(*letter)).is_some_and(|c| c.is_ascii_alphabetic()));
    is_drive.then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn an_extended_prefix_is_stripped_for_the_shell() {
        assert_eq!(shell_path(Path::new(r"\\?\C:\")), w(r"C:\"));
        assert_eq!(shell_path(Path::new(r"\\?\d:\")), w(r"d:\"));
    }

    #[test]
    fn an_ordinary_volume_root_is_passed_through_untouched() {
        assert_eq!(shell_path(Path::new(r"C:\")), w(r"C:\"));
        assert_eq!(
            shell_path(Path::new(r"E:\mount\point\")),
            w(r"E:\mount\point\")
        );
    }

    /// A UNC or volume-GUID path keeps its prefix: there is no drive-letter
    /// form to rewrite it to, and a refused query is a better answer than a
    /// mangled path that queries the wrong thing.
    #[test]
    fn non_drive_extended_paths_keep_their_prefix() {
        assert_eq!(
            shell_path(Path::new(r"\\?\UNC\server\share\")),
            w(r"\\?\UNC\server\share\")
        );
        assert_eq!(
            shell_path(Path::new(
                r"\\?\Volume{00000000-0000-0000-0000-000000000000}\"
            )),
            w(r"\\?\Volume{00000000-0000-0000-0000-000000000000}\")
        );
        assert!(strip_extended_prefix(&w(r"\\?\")).is_none());
    }

    #[test]
    fn an_empty_bin_has_nothing_to_say() {
        let empty = BinStatus {
            volume: PathBuf::from(r"C:\"),
            bytes: 0,
            items: 0,
        };
        assert!(empty.is_empty());
        assert!(
            !BinStatus {
                bytes: 4096,
                items: 1,
                ..empty.clone()
            }
            .is_empty()
        );
    }

    /// A real query against the volume this test is running from. It
    /// asserts only the shape — the machine's bin may legitimately be
    /// empty — and its point is that the call is reachable, unelevated,
    /// from an ordinary thread with no COM apartment initialised.
    #[test]
    fn the_running_volume_answers_without_elevation_or_com() {
        let here = std::env::current_dir().expect("cwd");
        let status = query(&here).expect("a fixed volume has a recycle bin");
        assert!(
            status.volume.to_string_lossy().ends_with('\\'),
            "a volume root ends in a separator: {status:?}"
        );
        // Items and bytes agree about emptiness on any sane bin.
        assert_eq!(status.items == 0, status.bytes == 0, "{status:?}");
    }

    /// What the call costs on this machine's bin, which is what decides
    /// whether it may run on the UI thread (it may not — see
    /// `camembert/src/ui/recycle_rt.rs`). Same shape as
    /// `freeable::tests::bench_sweep_cost`.
    ///
    /// `cargo test -p camembert-core --lib recycle::tests::bench -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn bench_query_cost() {
        let here = std::env::current_dir().expect("cwd");
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let status = query(&here);
            println!("{:?} in {:?}", status, started.elapsed());
        }
    }
}
