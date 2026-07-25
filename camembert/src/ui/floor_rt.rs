//! Freeable phase 2 slice 2 — the ambient **exclusive floor** pass's
//! UI-side runtime (D3, `docs/design/freeable2-decisions.md`). The core
//! whole-tree walk lives in `camembert_core::fiemap::floor`, whose module
//! doc is the honesty contract this file's wiring must never violate; the
//! shape here mirrors `ui::oracle` (spawn state + a job channel, owned by
//! the event loop exactly like `sweep_rx`/`OracleRuntime`) — but the pass
//! itself is a single whole-tree walk rather than one job per mark, and
//! the result is a whole-value **snapshot** (D3), never incrementally
//! merged.
//!
//! ## Spawn sequencing (D3: "after the phase-1 sweep; sequenced, never
//! concurrent with it")
//!
//! `ui::event_loop` owns the sequencing decision (search `spawn_full` at
//! its two call sites): the pass spawns immediately at scan end when no
//! `/proc` sweep will run at all (`--no-proc-sweep`), or the instant the
//! sweep's own result lands otherwise. This module only refuses to spawn
//! twice for the same pass ([`FloorRuntime::spawn_full`]'s `in_flight`
//! guard) — it has no opinion on *when* it is called.
//!
//! ## The deletion interlock — the one subtle part
//!
//! A confirmed deletion takes the frozen arena's *write* lock
//! (`ui::write_outcome`) for the duration of `delete::delete_nodes`. The
//! floor pass holds the arena's *read* lock for however long the
//! whole-tree walk takes — potentially minutes on a huge tree. Naively, a
//! deletion arriving mid-pass would block on the write lock until the
//! pass finished: unacceptable latency for an action the user expects to
//! be near-instant.
//!
//! The fix, in two halves the caller (`ui::execute_deletion`) must call in
//! order:
//!
//! 1. [`FloorRuntime::cancel_for_deletion`] — called **before**
//!    `ui::write_outcome` is ever acquired. It flips the in-flight pass's
//!    cancel flag; [`compute_floor`] checks that flag roughly every 256
//!    files and returns `None` promptly, releasing the read lock well
//!    before the deletion's write-lock attempt would ever have to wait
//!    for it.
//! 2. [`FloorRuntime::respawn_after_deletion`] — called **after** the
//!    write lock is dropped, **unconditionally** (regardless of whether
//!    anything was actually deleted — see below). Spawns the next pass: a
//!    cheap [`reaggregate_floor`] over the surviving tree when a
//!    completed map already existed, or a fresh full [`compute_floor`]
//!    otherwise.
//!
//! Calling `respawn_after_deletion` unconditionally (not just when
//! `report.deleted > 0`) matters: `cancel_for_deletion` fires *before* the
//! deletion's outcome is known (it has to, to avoid ever blocking on the
//! lock), so a deletion attempt that fails entirely (`report.deleted ==
//! 0`) still cancelled whatever pass was running. Without an unconditional
//! respawn, that cancelled pass — possibly the *first* pass of the
//! session, never yet landed — would simply never run again, silently
//! stalling the ambient floor for the rest of the session. A wasted
//! reaggregate/recompute in the rare all-failed-deletion case is a small
//! price for that self-healing guarantee. (`ui::execute_deletion` still
//! only calls [`super::state::UiState::clear_floor`] when something
//! actually changed — an unchanged tree's existing snapshot stays valid
//! and visible while the harmless respawn runs.)
//!
//! ## Generation, not epoch
//!
//! Each spawn (full or reaggregate) is tagged with an internal, strictly
//! increasing `generation` — **not** `UiState::flat_epoch`. A dedicated
//! counter is needed (rather than reusing the deletion epoch the way
//! `OracleRuntime`/the filter fold do) because `respawn_after_deletion`
//! can legitimately spawn a new pass **against the same epoch** (the
//! all-failed-deletion case above never bumps `flat_epoch`) — reusing the
//! epoch there would make a stale, cancelled pass's late-arriving `None`
//! indistinguishable from the fresh pass's own result. The generation
//! counter has no such ambiguity: it bumps on every single spawn, so
//! [`FloorRuntime::poll`] can always tell "this message belongs to the
//! pass I am still tracking" from "this is a superseded straggler".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use camembert_core::fiemap::{FloorMap, compute_floor, reaggregate_floor};
use camembert_core::scan::ScanOutcome;
use camembert_core::size::HumanSize;
use camembert_core::view::Row;

use super::read_outcome;

/// Thread name for every spawned pass (visible in `ps -T` / panics).
const THREAD_NAME: &str = "camembert-floor";

/// One landed pass's result, tagged with the spawn generation it ran as
/// (see the module doc's "Generation, not epoch" section). `map` is
/// `None` only for a cancelled full [`compute_floor`] — a `reaggregate_floor`
/// pass never cancels (module doc: no FIEMAP, always fast) and always
/// sends `Some`.
struct FloorMessage {
    generation: u64,
    map: Option<FloorMap>,
}

/// The floor pass's session-long runtime: spawn state and the job
/// channel. Lives in the event loop exactly like
/// `sweep_rx`/`oracle::OracleRuntime` — [`super::state::UiState`] stays
/// terminal/IO-free and only ever holds the landed, already-resolved
/// [`FloorMap`] snapshot (`UiState::floor`).
pub struct FloorRuntime {
    /// `!no_fiemap && shared_bit_reliable()` (D3: "ambient floor is gated
    /// on kernel >= 6.1 ... the oracle is NOT gated" — this gate is
    /// strictly this module's own). Computed once by the caller
    /// (`ui::event_loop`) and injected here so tests never depend on the
    /// host kernel.
    enabled: bool,
    tx: Sender<FloorMessage>,
    rx: Receiver<FloorMessage>,
    /// Cancel flag of whichever pass is currently in flight, if any.
    cancel: Option<Arc<AtomicBool>>,
    /// Files-processed counter of the in-flight pass, for the "mapping
    /// extents… N files" status line.
    progress: Option<Arc<AtomicU64>>,
    /// Strictly increasing per-spawn identity (module doc).
    generation: u64,
    /// Whether a pass is currently running (spawned, not yet landed).
    in_flight: bool,
}

impl FloorRuntime {
    pub fn new(enabled: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            enabled,
            tx,
            rx,
            cancel: None,
            progress: None,
            generation: 0,
            in_flight: false,
        }
    }

    /// Whether a pass is currently running — drives
    /// `ui::needs_frequent_polling` (the progress line must tick) and
    /// guards [`Self::spawn_full`] against a duplicate spawn.
    pub fn has_pending(&self) -> bool {
        self.in_flight
    }

    /// Files processed so far by the in-flight pass, for the "mapping
    /// extents… N files" status line. `None` while nothing is running —
    /// the caller shows nothing at all in that case (idle or done).
    pub fn progress(&self) -> Option<u64> {
        self.progress.as_ref().map(|p| p.load(Ordering::Relaxed))
    }

    /// Spawn the full whole-tree pass (D3 sequencing — see the module doc
    /// for the two call sites in `ui::event_loop`). A no-op when disabled
    /// (`--no-fiemap`/old kernel) or a pass is already running (the two
    /// call sites are mutually exclusive by construction — `--no-proc-sweep`
    /// is either on or off for the whole session — so this guard should
    /// never actually fire; kept as a defensive invariant, not a load-bearing
    /// one).
    pub fn spawn_full(&mut self, outcome: Arc<RwLock<ScanOutcome>>) {
        if !self.enabled || self.in_flight {
            return;
        }
        self.spawn_compute(outcome);
    }

    /// The deletion interlock's first half (module doc): call **before**
    /// taking the arena's write lock. Cancels whichever pass is in flight
    /// so it releases its read lock within a bounded number of files
    /// instead of running to completion. A no-op when nothing is running.
    pub fn cancel_for_deletion(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// The deletion interlock's second half (module doc): call
    /// **unconditionally** after the deletion completed and the write
    /// lock dropped. Supersedes whatever was in flight — its cancelled
    /// result, if it lands late, carries the *old* generation and is
    /// discarded by [`Self::poll`] — with a pass against a fresh
    /// generation: a cheap [`reaggregate_floor`] when a completed map
    /// already existed (`prev`), else a fresh full [`compute_floor`].
    pub fn respawn_after_deletion(
        &mut self,
        outcome: Arc<RwLock<ScanOutcome>>,
        prev: Option<Arc<FloorMap>>,
    ) {
        if !self.enabled {
            return;
        }
        match prev {
            Some(prev) => self.spawn_reaggregate(outcome, prev),
            None => self.spawn_compute(outcome),
        }
    }

    /// Non-blocking poll of the job channel (the `spawn_freeable_sweep`/
    /// `OracleRuntime::poll` `try_recv` idiom). Returns the landed map
    /// once the pass this runtime is *currently* tracking completes
    /// (`Some(None)` from a cancelled pass is folded into the outer
    /// `None` — a cancellation never has anything honest to show, so it
    /// is indistinguishable here from "nothing landed yet"). Any message
    /// from a superseded generation is silently dropped.
    pub fn poll(&mut self) -> Option<FloorMap> {
        let mut result = None;
        while let Ok(msg) = self.rx.try_recv() {
            if msg.generation != self.generation {
                debug!(
                    generation = msg.generation,
                    current = self.generation,
                    "dropped a stale floor pass result"
                );
                continue;
            }
            self.finish();
            result = msg.map;
        }
        result
    }

    // ---- spawn plumbing --------------------------------------------------

    fn spawn_compute(&mut self, outcome: Arc<RwLock<ScanOutcome>>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let generation = self.start(Arc::clone(&cancel), Arc::clone(&progress));
        let tx = self.tx.clone();
        let spawned = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || {
                let map = {
                    let guard = read_outcome(&outcome);
                    compute_floor(&guard, &cancel, &progress)
                };
                let _ = tx.send(FloorMessage { generation, map });
            });
        if let Err(err) = spawned {
            warn!(%err, "failed to spawn the floor pass thread; ambient floor skipped this pass");
            self.finish();
        }
    }

    fn spawn_reaggregate(&mut self, outcome: Arc<RwLock<ScanOutcome>>, prev: Arc<FloorMap>) {
        // `reaggregate_floor` never calls FIEMAP and never checks a cancel
        // flag (module doc: pure re-aggregation, "fast enough for a
        // synchronous post-deletion refresh") — the cancel/progress
        // handles below exist only so `FloorRuntime`'s in-flight shape
        // stays uniform across both spawn kinds.
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let generation = self.start(cancel, progress);
        let tx = self.tx.clone();
        let spawned = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || {
                let map = {
                    let guard = read_outcome(&outcome);
                    reaggregate_floor(&guard, &prev)
                };
                let _ = tx.send(FloorMessage {
                    generation,
                    map: Some(map),
                });
            });
        if let Err(err) = spawned {
            warn!(%err, "failed to spawn the floor reaggregate thread; ambient floor stays as-is");
            self.finish();
        }
    }

    /// Track a fresh spawn: bump the generation, adopt its cancel/progress
    /// handles, mark in-flight. Returns the generation the caller's
    /// spawned closure must tag its message with.
    fn start(&mut self, cancel: Arc<AtomicBool>, progress: Arc<AtomicU64>) -> u64 {
        self.generation += 1;
        self.cancel = Some(cancel);
        self.progress = Some(progress);
        self.in_flight = true;
        self.generation
    }

    /// Clear in-flight bookkeeping once the tracked generation's result
    /// has landed (or its spawn failed outright).
    fn finish(&mut self) {
        self.cancel = None;
        self.progress = None;
        self.in_flight = false;
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested independent of the runtime/threads above)
// ---------------------------------------------------------------------------

/// The row's ambient exclusive floor: a scanned directory reads its
/// aggregate ([`FloorMap::dir_floor`]); a file — or an excluded,
/// non-descended mount point, where `row.dir` is also `None` — reads its
/// own node figure ([`FloorMap::node_floor`]). Shared by the bar split and
/// the selection card figure so the two surfaces can never disagree about
/// which number a given row means.
pub fn row_floor(floor: &FloorMap, row: &Row) -> Option<u64> {
    match row.dir {
        Some(dir) => floor.dir_floor(dir),
        None => floor.node_floor(row.node),
    }
}

/// Split a rendered proportion bar into a bright ("floor-exclusive")
/// prefix and the rest (tui-design.md reservation 2). `bar` is
/// `wheel::proportion_bar`'s already-eased output for this row — reusing
/// its own filled-cell count (rather than recomputing `frac * width`
/// separately) means the split can never disagree with what the bar
/// itself is actually showing, sub-cell rounding included.
///
/// `None` (no map, or the row has no figure) -> `0` (bar unchanged,
/// entirely "remainder"). `Some(0)` (D2 "fully shared") -> `0` — an
/// all-remainder-colored bar *is* the signal; the "fully shared" wording
/// itself lives in the selection card, not the bar. `Some(f) if f >=
/// row_disk` -> the whole filled portion (clamped). Returns the char
/// index in `bar` where the bright prefix ends.
pub fn bright_split(bar: &str, floor_bytes: Option<u64>, row_disk: u64) -> usize {
    let Some(floor) = floor_bytes else {
        return 0;
    };
    if floor == 0 || row_disk == 0 {
        return 0;
    }
    let filled = bar.chars().filter(|&c| c != ' ').count();
    if filled == 0 {
        return 0;
    }
    let floor_frac = (floor as f64 / row_disk as f64).min(1.0);
    ((filled as f64 * floor_frac).round() as usize).min(filled)
}

/// [`humanize_age`](super::fmt::humanize_age)'s coarse-unit bucketing,
/// applied to a plain elapsed [`Duration`] instead of two wall-clock
/// timestamps: treating the duration itself as the "delta" reproduces the
/// exact same decomposition (`delta = now - mtime` there, `delta =
/// elapsed` here), so this reuses that helper instead of duplicating its
/// unit ladder.
fn humanize_elapsed(elapsed: Duration) -> String {
    super::fmt::humanize_age(elapsed.as_secs() as i64, 0)
}

/// The selection card's floor figure lines (D2: `excl ≥ …`/"fully shared",
/// plus at most one caveat) — or no lines at all when there is nothing
/// honest to say (no map yet, the row has no figure, or a zero floor on
/// an empty row). `now`/`computed_at` are both injected for testability;
/// the caller passes `Instant::now()` and `UiState::floor`'s stamp.
pub fn card_lines(floor: &FloorMap, row: &Row, now: Instant, computed_at: Instant) -> Vec<String> {
    let Some(f) = row_floor(floor, row) else {
        return Vec::new();
    };
    let rel = humanize_elapsed(now.saturating_duration_since(computed_at));
    let mut lines = Vec::with_capacity(2);
    if f > 0 {
        lines.push(format!(
            "excl \u{2265} {} \u{b7} mapped {rel}",
            HumanSize(f)
        ));
    } else if row.disk > 0 {
        lines.push(format!("fully shared \u{b7} mapped {rel}"));
    } else {
        // f == 0 and row.disk == 0: neither wording fits an empty row.
        return Vec::new();
    }
    if floor.any_compressed() {
        lines.push("logical bytes — compressed mount: physical reclaim may be smaller".to_owned());
    } else {
        let (_, unmapped) = floor.coverage();
        if unmapped > 0 {
            lines.push(format!("{unmapped} file(s) unmapped — floor understates"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use camembert_core::scan::{ScanOptions, Scanner};
    use camembert_core::tree::NodeId;
    use std::path::Path;

    // --- real-tree helpers (compute_floor needs an actual ScanOutcome;
    // Owner/Batch are `pub(crate)` to camembert-core and not reachable
    // from this crate, so a tiny tempdir scan is the honest way to get
    // one — same idiom `ui.rs`'s own tests already use) ---------------

    fn scan_dir(path: &Path) -> ScanOutcome {
        let mut outcome = Scanner::new(ScanOptions {
            threads: 2,
            ..ScanOptions::default()
        })
        .scan(path)
        .expect("scan of a tempdir never fails");
        outcome.finalize_hardlinks();
        outcome
    }

    fn one_file_outcome() -> (ScanOutcome, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a"), vec![0u8; 8192]).expect("write fixture");
        (scan_dir(tmp.path()), tmp)
    }

    /// Guard: skip (returning `true`) when the fixture filesystem is the
    /// one tier that produces no floor figure at all. The extent tiers
    /// map through FIEMAP and the hardlink-exact tier takes `st_blocks`,
    /// so both land a figure; ZFS has no per-file sharing API and lands
    /// none — a test asserting on the figure has nothing to assert
    /// against. Same runtime-probe style as `camembert-core/tests/fiemap.rs`
    /// (found running this suite on a real ZFS pool, 2026-07-25).
    fn skip_unless_floor_produces_figures(dir: &Path) -> bool {
        if matches!(
            camembert_core::fiemap::tier_of(dir),
            camembert_core::fiemap::FsTier::Zfs
        ) {
            eprintln!("skipping: {} is on ZFS (no per-file figure)", dir.display());
            return true;
        }
        false
    }

    fn compute(outcome: &ScanOutcome) -> FloorMap {
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        compute_floor(outcome, &cancel, &progress).expect("not pre-cancelled")
    }

    fn row(node: NodeId, dir: Option<camembert_core::tree::DirId>, disk: u64) -> Row {
        Row {
            name: b"x".to_vec().into_boxed_slice(),
            node,
            dir,
            is_dir: dir.is_some(),
            apparent: disk,
            disk,
            items: 1,
            errors: 0,
            error_reason: None,
            state: camembert_core::view::RowState::File,
            mtime: 0,
        }
    }

    // ---- gating -------------------------------------------------------

    #[test]
    fn disabled_runtime_never_spawns() {
        let (outcome, _tmp) = one_file_outcome();
        let mut rt = FloorRuntime::new(false);
        rt.spawn_full(Arc::new(RwLock::new(outcome)));
        assert!(!rt.has_pending(), "disabled: no thread, no spawn");
        assert!(rt.poll().is_none());
    }

    #[test]
    fn disabled_runtime_respawn_after_deletion_is_also_a_noop() {
        let (outcome, _tmp) = one_file_outcome();
        let mut rt = FloorRuntime::new(false);
        rt.respawn_after_deletion(Arc::new(RwLock::new(outcome)), None);
        assert!(!rt.has_pending());
    }

    // ---- spawn/poll round trip (real thread) ---------------------------

    #[test]
    fn spawn_full_lands_a_real_floor_map() {
        let (outcome, _tmp) = one_file_outcome();
        if skip_unless_floor_produces_figures(_tmp.path()) {
            return;
        }
        let mut rt = FloorRuntime::new(true);
        rt.spawn_full(Arc::new(RwLock::new(outcome)));
        assert!(rt.has_pending());

        let mut landed = None;
        for _ in 0..200 {
            if let Some(map) = rt.poll() {
                landed = Some(map);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let map = landed.expect("a one-file scan's pass lands quickly");
        assert!(!rt.has_pending(), "poll cleared in-flight on landing");
        assert!(map.coverage().0 >= 1, "the one file got mapped");
    }

    #[test]
    fn spawn_full_guards_against_a_duplicate_spawn() {
        let (outcome, _tmp) = one_file_outcome();
        let mut rt = FloorRuntime::new(true);
        let lock = Arc::new(RwLock::new(outcome));
        rt.spawn_full(Arc::clone(&lock));
        let generation_after_first = rt.generation;
        rt.spawn_full(lock); // already in flight: must not bump generation
        assert_eq!(rt.generation, generation_after_first);
    }

    // ---- generation guard (mirrors oracle.rs's stale/superseded tests) --

    #[test]
    fn poll_discards_a_stale_generation_result() {
        let mut rt = FloorRuntime::new(true);
        // Simulate an in-flight pass at generation 1 without spawning a
        // real thread — direct field access, same idiom `floor.rs`'s own
        // test module uses for its private types.
        rt.cancel = Some(Arc::new(AtomicBool::new(false)));
        rt.progress = Some(Arc::new(AtomicU64::new(0)));
        rt.generation = 1;
        rt.in_flight = true;

        // A message from a superseded generation 0 lands late.
        let _ = rt.tx.send(FloorMessage {
            generation: 0,
            map: None,
        });
        assert!(rt.poll().is_none(), "stale message resolves nothing");
        assert!(rt.has_pending(), "the current pass is still tracked");
    }

    #[test]
    fn poll_resolves_the_current_generation_and_clears_in_flight() {
        let (outcome, _tmp) = one_file_outcome();
        let real_map = compute(&outcome);
        let mut rt = FloorRuntime::new(true);
        rt.cancel = Some(Arc::new(AtomicBool::new(false)));
        rt.progress = Some(Arc::new(AtomicU64::new(0)));
        rt.generation = 5;
        rt.in_flight = true;

        let _ = rt.tx.send(FloorMessage {
            generation: 5,
            map: Some(real_map),
        });
        assert!(rt.poll().is_some());
        assert!(!rt.has_pending());
    }

    #[test]
    fn poll_folds_a_same_generation_cancel_into_none() {
        let mut rt = FloorRuntime::new(true);
        rt.cancel = Some(Arc::new(AtomicBool::new(false)));
        rt.progress = Some(Arc::new(AtomicU64::new(0)));
        rt.generation = 2;
        rt.in_flight = true;

        let _ = rt.tx.send(FloorMessage {
            generation: 2,
            map: None,
        });
        assert!(rt.poll().is_none());
        assert!(
            !rt.has_pending(),
            "a resolved cancel still clears in-flight"
        );
    }

    // ---- the deletion interlock ----------------------------------------

    #[test]
    fn cancel_for_deletion_flips_the_in_flight_passs_flag() {
        let mut rt = FloorRuntime::new(true);
        let cancel = Arc::new(AtomicBool::new(false));
        rt.cancel = Some(Arc::clone(&cancel));
        rt.in_flight = true;

        rt.cancel_for_deletion();
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn cancel_for_deletion_is_a_noop_with_nothing_in_flight() {
        let mut rt = FloorRuntime::new(true);
        rt.cancel_for_deletion(); // must not panic
        assert!(!rt.has_pending());
    }

    #[test]
    fn respawn_after_deletion_without_a_prev_map_spawns_a_full_compute() {
        let (outcome, _tmp) = one_file_outcome();
        let mut rt = FloorRuntime::new(true);
        rt.respawn_after_deletion(Arc::new(RwLock::new(outcome)), None);
        assert!(rt.has_pending());
        assert_eq!(rt.generation, 1);
    }

    #[test]
    fn respawn_after_deletion_with_a_prev_map_spawns_and_lands_a_reaggregate() {
        let (outcome, _tmp) = one_file_outcome();
        let prev = Arc::new(compute(&outcome));
        let mut rt = FloorRuntime::new(true);
        rt.respawn_after_deletion(Arc::new(RwLock::new(outcome)), Some(prev));
        assert!(rt.has_pending());

        let mut landed = None;
        for _ in 0..200 {
            if let Some(map) = rt.poll() {
                landed = Some(map);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(landed.is_some(), "reaggregate over an unchanged tree lands");
        assert!(!rt.has_pending());
    }

    #[test]
    fn respawn_after_deletion_always_bumps_generation_even_with_nothing_changed() {
        // The all-failed-deletion self-healing case (module doc): a
        // respawn happens even when the tree never changed, so a pass
        // cancelled for nothing is never stuck forever.
        let (outcome, _tmp) = one_file_outcome();
        let mut rt = FloorRuntime::new(true);
        let lock = Arc::new(RwLock::new(outcome));
        rt.spawn_full(Arc::clone(&lock));
        let cancel = rt.cancel.clone().expect("in flight");
        rt.cancel_for_deletion();
        assert!(cancel.load(Ordering::Relaxed));

        let before = rt.generation;
        rt.respawn_after_deletion(lock, None);
        assert!(
            rt.generation > before,
            "unconditional respawn always supersedes the cancelled pass"
        );
    }

    // ---- pure helpers ----------------------------------------------------

    #[test]
    fn row_floor_reads_dir_floor_for_a_dir_row_and_node_floor_for_a_file_row() {
        let (outcome, _tmp) = one_file_outcome();
        let map = compute(&outcome);
        let root = outcome.root();
        let file_node = outcome
            .children_of(root)
            .next()
            .expect("the fixture file exists");

        let file_row = row(file_node, None, 8192);
        assert_eq!(row_floor(&map, &file_row), map.node_floor(file_node));

        let root_node = outcome.dir(root).node;
        let dir_row = row(root_node, Some(root), 8192);
        assert_eq!(row_floor(&map, &dir_row), map.dir_floor(root));
    }

    #[test]
    fn bright_split_cases() {
        // No figure at all: bar unchanged.
        assert_eq!(bright_split("######", None, 1000), 0);
        // Fully shared (Some(0)): zero bright cells, never negative/odd.
        assert_eq!(bright_split("######", Some(0), 1000), 0);
        // 50% floor over a 6-cell filled bar: 3 bright.
        assert_eq!(bright_split("######", Some(500), 1000), 3);
        // 100% floor (f == row_disk): the whole filled portion.
        assert_eq!(bright_split("######", Some(1000), 1000), 6);
        // f > row_disk clamps to the whole filled portion.
        assert_eq!(bright_split("######", Some(5000), 1000), 6);
        // Rounding at a 1-cell fill: below half stays dim, at/above
        // rounds up to the one available cell.
        assert_eq!(bright_split("#", Some(300), 1000), 0);
        assert_eq!(bright_split("#", Some(600), 1000), 1);
        // A bar with trailing spaces (partial fill): only filled chars
        // count toward the split.
        assert_eq!(bright_split("##  ", Some(1000), 1000), 2);
        // Degenerate denominator never panics/divides by zero.
        assert_eq!(bright_split("##", Some(10), 0), 0);
    }

    #[test]
    fn card_lines_excl_wording_and_caveat_precedence() {
        let (outcome, _tmp) = one_file_outcome();
        let map = compute(&outcome);
        let file_node = outcome
            .children_of(outcome.root())
            .next()
            .expect("fixture file");
        // Whether this host's tmp filesystem yields exclusive > 0 or a
        // (legitimate) 0 for an unshared file is tier-dependent; both
        // wordings are exercised deterministically by the dedicated
        // "fully shared" test below, so here just check whichever
        // wording actually applies is the right one, plus the shared
        // "mapped ... ago" suffix both branches carry.
        let Some(floor) = map.node_floor(file_node) else {
            return; // ZFS tmpfs: no per-file figures at all, nothing to assert
        };
        let row = row(file_node, None, 8192);
        let now = Instant::now();
        let computed_at = now - Duration::from_secs(65);

        let lines = card_lines(&map, &row, now, computed_at);
        assert!(!lines.is_empty());
        if floor > 0 {
            assert!(lines[0].starts_with("excl \u{2265} "), "{lines:?}");
        } else {
            assert!(lines[0].starts_with("fully shared"), "{lines:?}");
        }
        assert!(lines[0].contains("mapped"));
        assert!(lines[0].contains("min ago"), "{lines:?}");
    }

    #[test]
    fn card_lines_fully_shared_wording_for_a_zero_floor_on_a_nonzero_row() {
        let (outcome, _tmp) = one_file_outcome();
        if skip_unless_floor_produces_figures(_tmp.path()) {
            return;
        }
        let map = compute(&outcome); // produced_figure is true: the fixture file got mapped
        // A directory id absent from the map's aggregate reads as
        // `Some(0)` by `FloorMap::dir_floor`'s documented "a miss reads
        // as 0, since the pass did produce figures" rule (any live,
        // childless directory would hit exactly this path in practice) —
        // forging the id keeps this test independent of which tier this
        // host's temp filesystem actually is.
        let bogus_dir = camembert_core::tree::DirId::from_raw(u32::MAX - 7);
        let dir_row = row(NodeId::from_raw(u32::MAX - 7), Some(bogus_dir), 8192);
        let now = Instant::now();

        let lines = card_lines(&map, &dir_row, now, now);
        assert_eq!(lines[0], "fully shared \u{b7} mapped just now");
    }

    #[test]
    fn card_lines_absent_when_the_row_has_no_figure() {
        let (outcome, _tmp) = one_file_outcome();
        let map = compute(&outcome);
        // A node id the map never produced a figure for.
        let unknown = NodeId::from_raw(u32::MAX - 1);
        let row = row(unknown, None, 1234);
        assert!(card_lines(&map, &row, Instant::now(), Instant::now()).is_empty());
    }

    #[test]
    fn humanize_elapsed_matches_the_shared_bucketing() {
        assert_eq!(humanize_elapsed(Duration::from_secs(3)), "just now");
        assert_eq!(humanize_elapsed(Duration::from_secs(65)), "1 min ago");
        assert_eq!(humanize_elapsed(Duration::from_secs(3 * 3600)), "3 h ago");
    }
}
