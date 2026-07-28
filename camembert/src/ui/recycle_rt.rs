//! Windows: the Recycle Bin meter's UI half — one off-thread query at scan
//! end, and the exact words the gauge and the toast are allowed to use.
//!
//! [`camembert_core::recycle`] explains what the figure is. This module
//! decides how it is said, and the decisions are the point:
//!
//! - **Never the word "freeable".** Linux's gauge suffix says `· N
//!   freeable` because a `/proc` sweep found blocks that a `close(2)`
//!   really does release. Recycle Bin bytes are released by *emptying the
//!   bin*, which is the user's action and never camembert's. Borrowing the
//!   Linux wording would put a claim on screen that the tool cannot back.
//! - **Say where it is, then say what that costs.** The suffix names the
//!   Recycle Bin, and the toast spells out the consequence — "not free
//!   until you empty it" — because "in the Recycle Bin" reads to plenty of
//!   users as "gone".
//! - **No panel, no key, no flag.** There is exactly one number and one
//!   sentence about it. The `f` panel exists on Linux because a sweep
//!   produces a ledger of inodes and guilty PIDs worth scrolling; a bin
//!   query produces two integers. A modal for two integers would be
//!   ceremony, and the keymap/`?`/palette stay untouched.
//!
//! # Why it runs off the UI thread
//!
//! `SHQueryRecycleBinW` measured **16.5–23.3 ms** on this box's bin (66
//! items, 6 264 307 348 bytes — `recycle::tests::bench_query_cost`). The render loop
//! owes a frame every 33 ms, so half a frame is already too much, and a bin
//! holding tens of thousands of items is not bounded by that measurement at
//! all. So: one job thread, a one-shot channel, polled non-blockingly in
//! the event loop — the same shape as the freeable sweep on Linux
//! (`ui.rs`'s step 2.5) and the link-count runtime next door.
//!
//! # Threshold
//!
//! The toast reuses freeable D5's rule verbatim — **≥ 100 MiB *and* ≥ 1 %
//! of the volume's capacity** — so small disks are not nagged about crumbs
//! and large ones are not nagged about rounding noise. The constants are
//! restated here rather than imported because `freeable_panel` is
//! `cfg(unix)` and never compiled on this platform; [`should_toast`]'s
//! tests pin the same two boundaries its Linux twin does.
//!
//! The *suffix* has no threshold, exactly as on Linux: any non-zero figure
//! is shown, and only the interruption is rationed.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use tracing::{debug, warn};

use camembert_core::recycle::{self, BinStatus};
use camembert_core::size::HumanSize;

/// Thread name for the query job (visible in panics / a debugger).
const THREAD_NAME: &str = "camembert-recycle";

/// Minimum recycled bytes for the scan-end toast (freeable D5's figure).
pub const TOAST_MIN_BYTES: u64 = 100 * 1024 * 1024;
/// Minimum fraction of volume capacity for the toast (freeable D5).
pub const TOAST_MIN_FRACTION: f64 = 0.01;

/// Both bounds must hold, and a zero-capacity volume never divides by zero
/// and never toasts.
pub fn should_toast(recycled_bytes: u64, capacity_bytes: u64) -> bool {
    if recycled_bytes < TOAST_MIN_BYTES || capacity_bytes == 0 {
        return false;
    }
    (recycled_bytes as f64 / capacity_bytes as f64) >= TOAST_MIN_FRACTION
}

/// The disk gauge's suffix, ready to append — `None` when the bin is empty
/// and there is nothing to say.
///
/// Named, not quantified as a saving: the reader is told *where* the bytes
/// are, which is the fact no directory tree can show them.
pub fn gauge_suffix(status: &BinStatus) -> Option<String> {
    (!status.is_empty()).then(|| format!(" · {} in the Recycle Bin", HumanSize(status.bytes)))
}

/// The one-time scan-end notification, gated by [`should_toast`].
///
/// This is where the honesty lives: the bytes are named, the item count
/// says whether that is a few big things or a long tail, and the clause
/// after the dash refuses the "space you just got back" reading outright.
pub fn toast_text(status: &BinStatus) -> String {
    let items = if status.items == 1 { "item" } else { "items" };
    format!(
        "Recycle Bin: {} in {} {items} — not free until you empty it",
        HumanSize(status.bytes),
        status.items,
    )
}

/// Start the query for the volume containing `scan_root`.
///
/// `None` when the job thread could not be spawned — the meter then simply
/// never appears, which is the same outcome as a volume with no bin and
/// needs no separate story.
pub fn spawn(scan_root: PathBuf) -> Option<Receiver<BinStatus>> {
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            if let Some(status) = recycle::query(&scan_root) {
                debug!(
                    volume = %status.volume.display(),
                    bytes = status.bytes,
                    items = status.items,
                    "recycle bin measured"
                );
                // The receiver may already be gone (the session quit); a
                // failed send just means nobody is listening anymore.
                let _ = tx.send(status);
            }
        });
    match spawned {
        Ok(_handle) => Some(rx),
        Err(err) => {
            warn!(%err, "failed to spawn the recycle-bin query thread; no meter this session");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(bytes: u64, items: u64) -> BinStatus {
        BinStatus {
            volume: PathBuf::from(r"C:\"),
            bytes,
            items,
        }
    }

    // ---- wording (the honesty contract) ----------------------------------

    /// The one word this figure may never be given: `freeable` means "a
    /// `close(2)` away" on the very surface next to it, and these bytes are
    /// not. Saying they are *not* free is exactly what the toast is for, so
    /// the ban is on the claim, not on the substring.
    #[test]
    fn nothing_here_ever_calls_the_bin_freeable() {
        let full = status(6_264_307_348, 66);
        let suffix = gauge_suffix(&full).expect("a non-empty bin has a suffix");
        let toast = toast_text(&full);
        for text in [&suffix, &toast] {
            assert!(
                !text.to_lowercase().contains("freeable"),
                "{text:?} claims the bytes are freeable"
            );
        }
        assert!(
            toast.contains("not free until you empty it"),
            "the toast must refuse the reading outright: {toast:?}"
        );
    }

    /// The measured state of the machine this was written on: the suffix
    /// names the place, the toast names the consequence.
    #[test]
    fn the_measured_bin_reads_as_recoverable_not_as_reclaimed() {
        let full = status(6_264_307_348, 66);
        assert_eq!(
            gauge_suffix(&full).as_deref(),
            Some(" · 5.8 GiB in the Recycle Bin")
        );
        assert_eq!(
            toast_text(&full),
            "Recycle Bin: 5.8 GiB in 66 items — not free until you empty it"
        );
    }

    #[test]
    fn an_empty_bin_says_nothing_at_all() {
        assert_eq!(gauge_suffix(&status(0, 0)), None);
    }

    #[test]
    fn one_item_is_not_pluralised() {
        assert!(toast_text(&status(200 * 1024 * 1024, 1)).contains("in 1 item —"));
    }

    // ---- threshold (freeable D5's rule, restated) -------------------------

    #[test]
    fn the_toast_requires_both_bounds() {
        let capacity = 1_000 * 1024 * 1024 * 1024; // 1000 GiB
        // Big enough absolutely, far too small relatively.
        assert!(!should_toast(200 * 1024 * 1024, capacity));
        // Big enough relatively, too small absolutely.
        assert!(!should_toast(50 * 1024 * 1024, 1024 * 1024 * 1024));
        // Both.
        assert!(should_toast(20 * 1024 * 1024 * 1024, capacity));
    }

    #[test]
    fn an_unknown_capacity_never_toasts_and_never_divides_by_zero() {
        assert!(!should_toast(u64::MAX, 0));
    }

    #[test]
    fn the_absolute_bound_is_inclusive_and_the_relative_one_too() {
        let capacity = TOAST_MIN_BYTES * 100; // exactly 1 % at the floor
        assert!(should_toast(TOAST_MIN_BYTES, capacity));
        assert!(!should_toast(TOAST_MIN_BYTES - 1, capacity));
        assert!(!should_toast(TOAST_MIN_BYTES, capacity + 1));
    }

    /// The real path this session would take, end to end, off-thread.
    #[test]
    fn a_spawned_query_answers_for_this_volume() {
        let rx = spawn(std::env::current_dir().expect("cwd")).expect("spawn the query");
        let status = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the query answers");
        assert!(status.volume.to_string_lossy().ends_with('\\'));
    }
}
