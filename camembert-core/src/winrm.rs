//! "Who has this file open?", asked the way Windows installers ask it.
//!
//! Linux answers this by sweeping `/proc/[pid]/fd` ([`crate::freeable`]),
//! which on a desktop can read ~28 % of processes unprivileged. Windows has
//! no `/proc`, and the substitute is **better**: the Restart Manager
//! (`RmStartSession`/`RmRegisterResources`/`RmGetList`/`RmEndSession`)
//! answers from a medium-integrity, non-elevated process and *sees SYSTEM
//! services* — the exact blind spot the `/proc` sweep's coverage line exists
//! to apologise for. `C:\Windows\System32\svchost.exe` enumerated **104
//! distinct services** running as SYSTEM / LOCAL SERVICE / NETWORK SERVICE
//! from an ordinary shell (`docs/design/windows-delete-dossier.md` §4.1).
//!
//! The rejected alternative is recorded there too, so nobody spends a day
//! rediscovering it: `NtQuerySystemInformation(SystemExtendedHandleInform
//! ation)` enumerates 191 065 handles unelevated, but turning them into
//! *paths* needs `DuplicateHandle` + a name query that blocks
//! indefinitely — a full sweep did not complete in 120 s. Use this; do not
//! build that.
//!
//! # What it cannot see, which is the part that must be said out loud
//!
//! - **Kernel-loaded files are invisible.**
//!   `C:\Windows\System32\drivers\ntfs.sys` reports **zero** holders while
//!   very much in use. So a clean negative here is "the Restart Manager
//!   found nobody", never "nobody has this open", and every surface that
//!   renders an empty answer must carry that distinction — an absent
//!   warning is not a promise.
//! - **Some system files refuse the query outright.**
//!   `C:\Windows\System32\ntdll.dll` registers fine and then fails
//!   `RmGetList` with win32 6 (`ERROR_INVALID_HANDLE`). That is a stated
//!   failure, not an empty result, and [`HolderQuery`] keeps the two apart.
//! - **It reports *applications*, not handles**, and it misses many real
//!   locks. Measured 2026-07-28 over a live Firefox profile (11 processes):
//!   of the 47 files that genuinely refused an open-for-`DELETE` with
//!   `ERROR_SHARING_VIOLATION`, only **13 named a holder here** — 34 came
//!   back empty. In the other direction it was perfect: **0 of 60** files
//!   that opened fine reported a holder. So this is a positive predictor,
//!   never a negative one, and any surface rendering an empty answer owes
//!   the user that distinction in words.
//!
//! # Cost
//!
//! Measured on the dev box, non-elevated, Defender on: `RmStartSession`
//! ~0.3 ms, `RmRegisterResources` 0.2–1.2 ms, and `RmGetList` **50.5 ms for
//! one file**, 143.8 ms for 100, 285.4 ms for 1000 — a fixed floor with
//! sublinear growth. The **first** `RmGetList` in a fresh process cost
//! **434.6 ms** (the `RmSvc` service warming up); every later one landed in
//! that table. Three orders of magnitude above
//! [`crate::winlink`]'s 46 µs, so this is off-thread work with a debounce,
//! never a call any interactive path may make inline — see
//! `camembert/src/ui/holders_rt.rs`.
//!
//! # It reads, and only reads
//!
//! The Restart Manager can also *shut applications down*
//! (`RmShutdown`/`RmRestart`). Those are not imported here and must not be:
//! this module answers a question and moves nothing.

use std::path::Path;

use tracing::{debug, trace};
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};

/// How many `RmGetList` retries to allow when the needed count grows
/// between the sizing call and the filling one (processes come and go).
/// Three is generous: each retry costs another ~50 ms.
const MAX_SIZING_ATTEMPTS: usize = 3;

/// Upper bound on holders kept. `svchost.exe` legitimately reports 104, and
/// nothing that renders this shows more than a handful; the count is what
/// matters past that, and it is preserved exactly.
const MAX_KEPT: usize = 16;

/// One application the Restart Manager says is holding the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Process id.
    pub pid: u32,
    /// `strAppName` — an executable name, a service display name, or an
    /// explorer window title, depending on `ApplicationType`.
    pub name: String,
    /// `strServiceShortName`, non-empty only for a service.
    pub service: Option<String>,
}

/// What one lookup returned. As in [`crate::winlink::LinkCount`], a refusal
/// and an empty answer are different facts and are never collapsed: "the
/// Restart Manager found nobody" and "the Restart Manager would not say"
/// mean opposite things to a user deciding whether to touch a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderQuery {
    /// The Restart Manager answered. An empty vector is a real answer —
    /// and a *bounded* one: see the module docs on kernel-held files.
    Known {
        /// Holders, capped at [`MAX_KEPT`].
        holders: Vec<Holder>,
        /// How many the Restart Manager actually reported, which may be
        /// larger than `holders.len()`.
        total: usize,
    },
    /// The query failed; `code` is the Win32 error the API returned.
    Failed(RmFailure),
}

/// Why a lookup failed, in terms a user can act on. Deliberately not routed
/// through [`crate::errno`] — that taxonomy answers "what went wrong with
/// the scan", a different question with a different severity vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RmFailure {
    /// No session could be started — the Restart Manager service is
    /// unavailable, or too many sessions are open machine-wide.
    NoSession,
    /// The file could not be registered (a bad path, or a name this build
    /// cannot encode).
    NotRegistered,
    /// Registration worked and the listing did not. `ntdll.dll` answers
    /// win32 6 here; this is the *measured* shape of a system file saying
    /// no.
    Refused,
}

impl RmFailure {
    /// One short clause, meant to be appended after "unknown".
    pub fn label(self) -> &'static str {
        match self {
            Self::NoSession => "the Restart Manager is unavailable",
            Self::NotRegistered => "this path could not be registered",
            Self::Refused => "the Restart Manager refused this file",
        }
    }
}

/// Ask which applications hold `path` open.
///
/// Blocking for ~50 ms (much more on the first call in a process), so every
/// caller runs it off the UI thread.
pub fn holders_of(path: &Path) -> HolderQuery {
    let Some(session) = Session::start() else {
        return HolderQuery::Failed(RmFailure::NoSession);
    };
    if !session.register(path) {
        return HolderQuery::Failed(RmFailure::NotRegistered);
    }
    session.list()
}

/// An `RmStartSession` handle that ends its session on drop.
///
/// The Restart Manager caps concurrent sessions machine-wide, so leaking one
/// degrades the whole box, not just camembert — which is why this is a
/// guard type and not a pair of calls.
struct Session {
    handle: u32,
}

impl Session {
    fn start() -> Option<Self> {
        let mut handle: u32 = 0;
        // `CCH_RM_SESSION_KEY` is the length without the terminator; the
        // API writes a GUID string plus NUL into this buffer.
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        // SAFETY: both out-parameters are live, correctly sized locals —
        // the key buffer is exactly the length the API documents — and the
        // call is synchronous, so neither pointer outlives this frame.
        let rc = unsafe { RmStartSession(&raw mut handle, 0, key.as_mut_ptr()) };
        if rc != ERROR_SUCCESS {
            debug!(
                rc,
                "RmStartSession refused; no open-file advisory this time"
            );
            return None;
        }
        Some(Self { handle })
    }

    fn register(&self, path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return false;
        }
        wide.push(0);
        let files = [wide.as_ptr()];
        // SAFETY: `files` points at one NUL-terminated buffer that outlives
        // the synchronous call; the application and service arrays are
        // empty, which the API documents as a null pointer with a zero
        // count. Nothing is written back.
        let rc = unsafe {
            RmRegisterResources(
                self.handle,
                1,
                files.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            )
        };
        if rc != ERROR_SUCCESS {
            debug!(rc, path = %path.display(), "RmRegisterResources refused");
            return false;
        }
        true
    }

    /// The documented two-step: ask with a zero-length buffer to learn the
    /// needed count, then ask again with one that size. The count can grow
    /// in between (processes start), so this retries rather than truncating
    /// silently.
    fn list(&self) -> HolderQuery {
        let mut buffer: Vec<RM_PROCESS_INFO> = Vec::new();
        for _ in 0..MAX_SIZING_ATTEMPTS {
            // **Capacity, not length.** The buffer is grown with
            // `with_capacity` and stays empty until the API fills it, so
            // reading `len()` here would tell the API it has no room, get
            // `ERROR_MORE_DATA` again, and never make progress.
            let capacity = buffer.capacity();
            let mut needed: u32 = 0;
            let mut have: u32 = u32::try_from(capacity).unwrap_or(u32::MAX);
            // `RmRebootReasonNone`, as the `u32` out-parameter the binding
            // declares. camembert never acts on a reboot reason; the
            // pointer is mandatory, not optional.
            let mut reasons: u32 = 0;
            // SAFETY: `buffer` really has `capacity` elements allocated, and
            // `have` says exactly that, so the API writes at most that many;
            // it reports through `needed` how many it wanted. A null pointer
            // with `have == 0` is the documented sizing call.
            let rc = unsafe {
                RmGetList(
                    self.handle,
                    &raw mut needed,
                    &raw mut have,
                    if capacity == 0 {
                        std::ptr::null_mut()
                    } else {
                        buffer.as_mut_ptr()
                    },
                    &raw mut reasons,
                )
            };
            match rc {
                ERROR_SUCCESS => {
                    // Clamped at the allocation regardless of what the API
                    // claims: `set_len` past the capacity is UB, and this
                    // module will not stake that on a driver's arithmetic.
                    let written = (have as usize).min(capacity);
                    // SAFETY: the call initialised `written` contiguous
                    // `RM_PROCESS_INFO`s at the start of the allocation, and
                    // `written <= capacity` by the clamp above.
                    unsafe { buffer.set_len(written) };
                    let total = buffer.len();
                    let holders = buffer.iter().take(MAX_KEPT).map(holder_of).collect();
                    trace!(total, "restart manager listed holders");
                    return HolderQuery::Known { holders, total };
                }
                ERROR_MORE_DATA => {
                    // Grow and retry — but only if that is actually growth.
                    // A `needed` that does not exceed what we just offered
                    // would spin the loop for `MAX_SIZING_ATTEMPTS`
                    // 50 ms calls and answer nothing.
                    if needed as usize <= capacity {
                        debug!(needed, capacity, "RmGetList wants no more room than it had");
                        return HolderQuery::Failed(RmFailure::Refused);
                    }
                    buffer = Vec::with_capacity(needed as usize);
                }
                other => {
                    // The `ntdll.dll` case: registration succeeded, the
                    // listing did not.
                    debug!(rc = other, "RmGetList refused this file");
                    return HolderQuery::Failed(RmFailure::Refused);
                }
            }
        }
        debug!("RmGetList kept asking for more room; giving up rather than looping");
        HolderQuery::Failed(RmFailure::Refused)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: `self.handle` came from a successful `RmStartSession` and
        // is ended exactly once, when this guard is dropped.
        let rc = unsafe { RmEndSession(self.handle) };
        if rc != ERROR_SUCCESS {
            debug!(rc, "RmEndSession refused; the session may linger");
        }
    }
}

fn holder_of(info: &RM_PROCESS_INFO) -> Holder {
    Holder {
        pid: info.Process.dwProcessId,
        name: wide_field(&info.strAppName),
        service: Some(wide_field(&info.strServiceShortName)).filter(|s| !s.is_empty()),
    }
}

/// A fixed-size, NUL-padded UTF-16 field as a `String`.
///
/// Lossy on purpose: these are display names chosen by whoever registered
/// the process, they never name a file, and nothing round-trips them back
/// to the OS — the one place where `from_utf16_lossy` is the right answer
/// rather than the lazy one.
fn wide_field(field: &[u16]) -> String {
    let len = field
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(field.len());
    String::from_utf16_lossy(&field[..len]).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::RestartManager::{RM_UNIQUE_PROCESS, RmUnknownApp};

    /// A zeroed `FILETIME`, for building synthetic `RM_PROCESS_INFO`s.
    fn zero_filetime() -> FILETIME {
        FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }
    }

    fn field<const N: usize>(text: &str) -> [u16; N] {
        let mut out = [0u16; N];
        for (slot, unit) in out.iter_mut().zip(text.encode_utf16()) {
            *slot = unit;
        }
        out
    }

    fn info(pid: u32, app: &str, service: &str) -> RM_PROCESS_INFO {
        RM_PROCESS_INFO {
            Process: RM_UNIQUE_PROCESS {
                dwProcessId: pid,
                ProcessStartTime: zero_filetime(),
            },
            strAppName: field(app),
            strServiceShortName: field(service),
            ApplicationType: RmUnknownApp,
            AppStatus: 0,
            TSSessionId: 0,
            bRestartable: 0,
        }
    }

    #[test]
    fn a_fixed_field_stops_at_its_nul_and_is_trimmed() {
        assert_eq!(wide_field(&field::<8>("Code")), "Code");
        assert_eq!(wide_field(&field::<8>("")), "");
        // No NUL at all: the whole field is the name.
        assert_eq!(
            wide_field(&"abcd".encode_utf16().collect::<Vec<_>>()),
            "abcd"
        );
    }

    #[test]
    fn a_service_holder_keeps_its_short_name_and_a_plain_one_does_not() {
        let service = holder_of(&info(4, "Windows Update", "wuauserv"));
        assert_eq!(service.pid, 4);
        assert_eq!(service.name, "Windows Update");
        assert_eq!(service.service.as_deref(), Some("wuauserv"));

        let app = holder_of(&info(1234, "Code.exe", ""));
        assert_eq!(
            app.service, None,
            "an empty short name is absent, not empty"
        );
    }

    /// Every failure keeps its own words: "unavailable", "unregisterable"
    /// and "refused" are three different things a user can do three
    /// different things about.
    #[test]
    fn failures_stay_distinguishable() {
        let labels = [
            RmFailure::NoSession.label(),
            RmFailure::NotRegistered.label(),
            RmFailure::Refused.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    /// A file nobody holds gives a *clean* negative — the measured
    /// `needed = 0` case, not a failure.
    #[test]
    fn a_file_nobody_holds_answers_with_an_empty_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("unheld.txt");
        std::fs::write(&path, b"x").expect("write");

        match holders_of(&path) {
            HolderQuery::Known { holders, total } => {
                assert!(holders.is_empty(), "{holders:?}");
                assert_eq!(total, 0);
            }
            other => panic!("expected a clean negative, got {other:?}"),
        }
    }

    /// A file this very process holds open must be found, with our own pid.
    /// This is the positive control for the whole module: without it, a
    /// module that always returned "nobody" would pass every other test
    /// here.
    #[test]
    fn a_file_held_by_this_process_names_this_process() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("held.txt");
        std::fs::write(&path, b"x").expect("write");
        // Shared for read only — the share mode a holder must grant for a
        // delete to even be attempted (delete dossier §3.2).
        let _handle = std::fs::File::options()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .expect("hold the file open");

        match holders_of(&path) {
            HolderQuery::Known { holders, total } => {
                assert!(total >= 1, "the holder was not found");
                let me = std::process::id();
                assert!(
                    holders.iter().any(|h| h.pid == me),
                    "this process ({me}) is missing from {holders:?}"
                );
            }
            other => panic!("expected the holder to be listed, got {other:?}"),
        }
    }

    /// A name the API cannot be handed at all is refused *before* the call
    /// rather than truncated at the NUL and asked about — which would query
    /// a different file and report the answer as this one's. "We could not
    /// ask" and "nobody has it" must never look the same.
    #[test]
    fn a_path_with_an_interior_nul_is_refused_rather_than_truncated() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let sneaky: Vec<u16> = r"C:\a"
            .encode_utf16()
            .chain(std::iter::once(0))
            .chain(r"\b".encode_utf16())
            .collect();
        assert_eq!(
            holders_of(Path::new(&OsString::from_wide(&sneaky))),
            HolderQuery::Failed(RmFailure::NotRegistered)
        );
    }
}
