//! io_uring-batched `statx` (HANDOFF §3: "the lever almost nobody uses").
//!
//! One ring per worker (workers are independent — no sharing, no locking).
//! After `getdents64` yields a burst of names, the worker submits one
//! `IORING_OP_STATX` SQE per name and reaps *all* completions before the
//! entries go anywhere. That reap-to-zero discipline is what keeps the
//! owner's completion invariant intact (see `owner.rs`): a section is
//! only ever sent with every stat result already inside it, exactly like
//! the sync path, so `is_last_section` remains a sufficient release
//! condition. The in-flight count is tracked explicitly and asserted zero
//! on every return — the async path accounts for what the sync path
//! guaranteed implicitly.
//!
//! Availability is probed once per scan ([`probe`]): `io_uring_setup`
//! is denied by default-seccomp Docker, gVisor, and the
//! `kernel.io_uring_disabled` sysctl, and the STATX opcode needs kernel
//! ≥ 5.6. Any denial falls back to the sync path (`stat_at`), which is
//! byte-identical in behavior and supported forever — a scan never fails
//! because io_uring is unavailable.

use std::ffi::CString;
use std::io;
use std::mem;
use std::time::Duration;

use io_uring::{IoUring, opcode, types};
use rustix::fd::{AsRawFd, BorrowedFd};
use rustix::fs::{AtFlags, Statx};
use rustix::io::Errno;
use tracing::warn;

use super::worker::{EntryStat, STATX_MASK};

/// SQEs per burst == SQ ring size, and the in-flight cap per worker.
///
/// Sized to one `getdents64` burst: the 32 KiB dirent buffer
/// (`worker::DIRENT_BUF`) holds at most ~1365 minimum-size entries and
/// typically 600–1100 with real-world name lengths, so 1024 (power of
/// two, as `io_uring_setup` requires) lets a typical full burst be
/// stat'ed with a single `io_uring_enter` while bounding pinned memory:
/// 1024 × 256 B statx out-buffers = 256 KiB per worker, plus the owned
/// name storage. Directories smaller than a burst — the overwhelming
/// majority — cost exactly one submit syscall for all their stats.
pub(crate) const STAT_BURST: usize = 1024;

/// `struct statx` is 256 bytes in the uapi; the slab math above and the
/// cast in `stat_burst` both lean on rustix's `Statx` being exactly that.
const _: () = assert!(mem::size_of::<Statx>() == 256);

/// Probe io_uring availability for this process: ring creation (minimal
/// params) + `IORING_REGISTER_PROBE` for the STATX opcode. Returns the
/// human-readable denial reason otherwise (`EPERM`, `ENOSYS`, …).
pub(crate) fn probe() -> Result<(), String> {
    // Smallest power-of-two ring: this instance only serves the probe.
    let ring = IoUring::new(2).map_err(|err| errno_name(&err))?;
    let mut probe = io_uring::register::Probe::new();
    ring.submitter()
        .register_probe(&mut probe)
        .map_err(|err| format!("IORING_REGISTER_PROBE failed: {}", errno_name(&err)))?;
    if !probe.is_supported(opcode::Statx::CODE) {
        return Err("STATX opcode unsupported (kernel < 5.6)".to_string());
    }
    Ok(())
}

/// The symbolic errno name (`EPERM`) when we know it, the io::Error text
/// otherwise — for the one-line engine-selection log.
fn errno_name(err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(raw) => format!("{:?}", Errno::from_raw_os_error(raw)),
        None => err.to_string(),
    }
}

/// Per-worker statx batcher: one ring plus the pinned out-buffer slab.
///
/// Buffer ownership: SQE `i` of a burst writes into `bufs[i]`, a slot of
/// a boxed slice owned by this struct that is never reallocated, and
/// reads the name behind `names[i]`, a `CString` owned by the caller for
/// the whole call. Both stay alive across submission because
/// [`StatxBatcher::stat_burst`] does not return until every submitted
/// SQE's CQE has been reaped (`user_data` = slot index).
pub(crate) struct StatxBatcher {
    ring: IoUring,
    /// statx out-buffers, indexed by SQE `user_data`. Boxed slice: the
    /// allocation address is stable for the batcher's whole life.
    bufs: Box<[Statx]>,
    /// A submit-path error marked the ring unusable: every subsequent
    /// burst reports `None` for all entries (per-entry sync fallback).
    broken: bool,
}

impl StatxBatcher {
    pub(crate) fn new() -> io::Result<Self> {
        let ring = IoUring::new(STAT_BURST as u32)?;
        // SAFETY: `Statx` is a repr(C) struct of plain integers and
        // integer bitflags; the all-zero bit pattern is a valid value.
        let zeroed = unsafe { mem::zeroed::<Statx>() };
        Ok(Self {
            ring,
            bufs: vec![zeroed; STAT_BURST].into_boxed_slice(),
            broken: false,
        })
    }

    /// Batched `statx` for `names` (≤ [`STAT_BURST`]) relative to
    /// `dirfd`, `AT_SYMLINK_NOFOLLOW`, [`STATX_MASK`] — the exact
    /// semantics of the sync `stat_at` path.
    ///
    /// `results[i]` is `Some(Ok(_))`/`Some(Err(errno))` per the entry's
    /// own CQE, or `None` when io_uring could not run that entry at all
    /// (broken ring, `ENOSYS`/`EINVAL` completion) — the caller then
    /// stats that entry synchronously, so the per-entry error taxonomy
    /// only ever comes from a real statx answer.
    ///
    /// Completion invariant: on return, in-flight == 0 — every submitted
    /// SQE has been reaped. Sections built from these results are
    /// therefore complete when sent (see the module docs and `owner.rs`).
    pub(crate) fn stat_burst(
        &mut self,
        dirfd: BorrowedFd<'_>,
        names: &[CString],
        results: &mut Vec<Option<Result<EntryStat, Errno>>>,
    ) {
        assert!(names.len() <= STAT_BURST, "burst exceeds ring size");
        results.clear();
        results.resize_with(names.len(), || None);
        if self.broken {
            return;
        }

        // Push phase. The SQ is empty here (every previous burst reaped to
        // zero) and a burst never exceeds the ring size, so `push` cannot
        // fail — but handle SQ-full anyway by flushing and continuing.
        // `pushed` counts SQEs written to the SQ; `submitted` counts SQEs
        // the kernel actually consumed (`io_uring_enter` return values) —
        // only those ever produce a CQE, and only those pin memory.
        let mut pushed: usize = 0;
        let mut submitted: usize = 0;
        for (i, name) in names.iter().enumerate() {
            let sqe = opcode::Statx::new(
                types::Fd(dirfd.as_raw_fd()),
                name.as_ptr(),
                (&raw mut self.bufs[i]).cast::<types::statx>(),
            )
            .flags(AtFlags::SYMLINK_NOFOLLOW.bits() as i32)
            .mask(STATX_MASK.bits())
            .build()
            .user_data(i as u64);
            loop {
                // SAFETY: the kernel reads `name` (heap buffer of a
                // CString the caller keeps alive for the whole call;
                // CString storage never moves) and writes `bufs[i]`
                // (owned slab, never reallocated). Both outlive the
                // drain loop below, which does not let this function
                // return until this SQE's CQE has been consumed.
                match unsafe { self.ring.submission().push(&sqe) } {
                    Ok(()) => break,
                    Err(_) => {
                        // SQ full: submit what is queued and continue.
                        match self.ring.submit() {
                            Ok(n) => submitted += n,
                            Err(err) if retryable(&err) => {}
                            Err(err) => {
                                warn!(%err, "io_uring submit failed; ring disabled");
                                self.broken = true;
                                self.drain(pushed, submitted, results);
                                return;
                            }
                        }
                    }
                }
            }
            pushed += 1;
        }

        self.drain(pushed, submitted, results);
    }

    /// Submit whatever is still in the SQ and reap until every
    /// kernel-consumed SQE has completed. Entries never consumed by the
    /// kernel (broken ring) keep their `None` result — the caller stats
    /// them synchronously; their inert SQ entries die with the ring.
    ///
    /// CQEs land in the shared-memory CQ ring even without a syscall, so
    /// the last-ditch path (a fatally failing `io_uring_enter` with ops
    /// still in flight) degrades to polling the ring memory — this
    /// function never returns while the kernel owns a buffer, whatever
    /// the syscall layer does.
    fn drain(
        &mut self,
        pushed: usize,
        mut submitted: usize,
        results: &mut [Option<Result<EntryStat, Errno>>],
    ) {
        let mut reaped: usize = 0;
        loop {
            for cqe in self.ring.completion() {
                let i = cqe.user_data() as usize;
                let res = cqe.result();
                results[i] = if res < 0 {
                    let errno = Errno::from_raw_os_error(-res);
                    if errno == Errno::NOSYS || errno == Errno::INVAL {
                        // The op itself was refused (should have been
                        // caught by the probe): let the caller stat this
                        // entry synchronously instead of recording a
                        // phantom per-entry error.
                        None
                    } else {
                        Some(Err(errno))
                    }
                } else {
                    Some(Ok(EntryStat::from_statx(&self.bufs[i])))
                };
                reaped += 1;
            }
            // Done when everything the kernel consumed is reaped and
            // nothing more will be submitted.
            if reaped >= submitted && (submitted >= pushed || self.broken) {
                break;
            }
            if self.broken {
                // Ops already submitted still owe CQEs; poll the ring
                // memory until they land.
                std::thread::sleep(Duration::from_micros(100));
                continue;
            }
            // Submits any SQ leftovers, then waits. `io_uring_enter`
            // errors only when it consumed nothing, so `submitted` stays
            // exact on the error paths.
            match self.ring.submit_and_wait(pushed - reaped) {
                Ok(n) => submitted += n,
                Err(err) if retryable(&err) => {}
                Err(err) => {
                    warn!(%err, "io_uring wait failed; ring disabled");
                    self.broken = true;
                }
            }
        }
        // Completion invariant: nothing in flight past this point.
        debug_assert!(reaped >= submitted, "io_uring burst left SQEs in flight");
    }
}

/// Transient `io_uring_enter` errors worth retrying.
fn retryable(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(raw)
            if raw == Errno::INTR.raw_os_error()
                || raw == Errno::AGAIN.raw_os_error()
                || raw == Errno::BUSY.raw_os_error()
    )
}
