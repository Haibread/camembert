//! Windows: "who has this file open?", asked where a user asks it.
//!
//! [`camembert_core::winrm`] owns the Restart Manager call and every fact
//! about what it can and cannot see. This module decides *when* to ask and
//! *what the answer is allowed to say*.
//!
//! It is the Windows counterpart of freeable phase 1's `/proc` advisory,
//! with the same contract (freeable D6): **advisory, off-thread, and never
//! silent about its own coverage**. What it is not is a pre-deletion check,
//! because there is no deletion on Windows — the value it adds today is
//! standalone: a row you were about to act on outside camembert (`o`, `y`)
//! tells you first whether something is holding it.
//!
//! # The shape, copied from `nlink_rt` and then given a brake
//!
//! Off-thread job, [`HolderState::Pending`] placeholder, update in place,
//! memoised per node, invalidated on the deletion epoch — that is
//! [`super::nlink_rt`] exactly, and for the same reason: the render loop
//! owes a frame every 33 ms.
//!
//! The brake is new and is forced by the cost. A link count is 46 µs;
//! `RmGetList` measured **50.5 ms** for one file, and **434.6 ms** for the
//! first call in a process (`RmSvc` warming up). Spawning one job per row
//! the cursor passes over would put dozens of 50 ms threads in flight for
//! rows nobody is looking at any more. So a row must be **settled under the
//! cursor for [`DEBOUNCE`]** before anything is asked — the same idea the
//! query palette's fold debounce uses, applied to a syscall instead of a
//! keystroke. Arrowing through a directory therefore costs nothing at all;
//! stopping on a row costs one query.
//!
//! While the debounce is armed the card already shows the spinner, so the
//! line does not appear and disappear and the card's height never jumps.
//!
//! # Honesty, and the measurement that set the wording
//!
//! The empty answer is the dangerous one, and the measurement says it is
//! more dangerous than the dossier assumed. Over a live Firefox profile
//! (11 processes running), the files that genuinely refused an
//! open-for-DELETE were asked about here: **13 of 47 named a holder; 34
//! did not.** In the other direction there were **no false positives at
//! all** — 0 of 60 files that opened fine reported one.
//!
//! So this is a *positive* predictor and not a negative one. When it names
//! a holder, believe it. When it names nobody, that is not a clean bill of
//! health — a fact the dossier already knew for kernel-held files
//! (`ntfs.sys` reports zero while very much in use) and which turns out to
//! generalise. The empty line therefore says **"not proof"** in as many
//! words, and the wording is pinned by a test.
//!
//! A refusal (`ntdll.dll` answers win32 6) is a third thing again, and says
//! so rather than being folded into either.
//!
//! # What is never asked
//!
//! - **Anything, under `--no-proc-sweep`/`NO_PROC_SWEEP`.** That flag means
//!   "do not go looking at what processes have open", which is precisely
//!   this; it was inert on Windows before and now is not.
//! - **Anything before the scan finishes.** The answer would be as true
//!   mid-scan, but the scan is already saturating the filter stack and this
//!   call goes through the same one.
//! - **Directories and non-files.** The Restart Manager registers files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use camembert_core::scan::ScanOutcome;
use camembert_core::tree::{Kind, NodeId};
use camembert_core::winrm::{self, Holder, HolderQuery, RmFailure};

use super::read_outcome;

/// Thread name for every spawned query job (visible in panics / a debugger).
const THREAD_NAME: &str = "camembert-holders";

/// How long a row must stay under the cursor before it is worth 50 ms of
/// Restart Manager. Long enough that scrolling costs nothing, short enough
/// that stopping to read a row feels like it answered on its own.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Resolved answers kept before the cache is dropped wholesale. Smaller
/// than `nlink_rt`'s 8192 because each entry holds a name, and because the
/// debounce means far fewer rows are ever asked about.
const MAX_CACHED: usize = 512;

/// How many holder names the card names before it starts counting.
const MAX_NAMED: usize = 2;

// ---------------------------------------------------------------------------
// Pure data
// ---------------------------------------------------------------------------

/// What the selection card knows about who holds one row open.
///
/// The three answers are deliberately three: *found nobody* (bounded — see
/// the module docs), *found these*, and *would not say*. Collapsing the
/// first and the third would turn a refusal into a clean bill of health,
/// which is the single worst thing an advisory can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderState {
    /// The query is armed or in flight.
    Pending,
    /// The Restart Manager answered.
    Known {
        /// Holder labels, pre-joined at job time so the render path clones
        /// one small string per frame instead of a vector of structs.
        /// Empty when `total` is 0.
        named: String,
        /// How many holders it reported, which may exceed the number named.
        total: usize,
    },
    /// The query failed, with its reason.
    Failed(RmFailure),
}

/// The selection-card line for `state` — exactly one, always, in the shape
/// [`super::nlink_rt::card_lines`] established: a row that cannot answer
/// says so rather than falling silent, so the card's height never depends
/// on whether a query happened to succeed.
pub fn card_lines(state: Option<&HolderState>, spinner: char) -> Vec<String> {
    let Some(state) = state else {
        return Vec::new();
    };
    vec![match state {
        HolderState::Pending => format!("{spinner} checking for open handles…"),
        // The bounded negative. The caveat is *here* and nowhere else,
        // because this is the only answer that could be misread as a
        // guarantee — and the measurement in the module docs says it would
        // be misread 34 times out of 47.
        HolderState::Known { total: 0, .. } => {
            "no holder found · not proof — many real locks stay invisible".to_owned()
        }
        HolderState::Known { named, total: 1 } => format!("open in {named}"),
        HolderState::Known { named, total } => format!("open in {total} processes · {named}"),
        HolderState::Failed(why) => format!("open handles unknown · {}", why.label()),
    }]
}

/// One holder as the card names it: whoever registered it, plus the pid so
/// two copies of the same application are distinguishable.
fn label(holder: &Holder) -> String {
    let name = holder
        .service
        .as_deref()
        .filter(|_| holder.name.is_empty())
        .unwrap_or(&holder.name);
    if name.is_empty() {
        format!("pid {}", holder.pid)
    } else {
        format!("{name} ({})", holder.pid)
    }
}

/// The first few holders, then a count for the rest — `svchost.exe` really
/// does report 104, and a card line is not the place for them.
fn summarise(holders: &[Holder], total: usize) -> String {
    let mut named: Vec<String> = holders.iter().take(MAX_NAMED).map(label).collect();
    let remaining = total.saturating_sub(named.len());
    if remaining > 0 {
        named.push(format!("+{remaining} more"));
    }
    named.join(", ")
}

/// Whether `node` is worth a query at all. See the module docs for every
/// exclusion and its reason.
fn is_candidate(outcome: &ScanOutcome, node: NodeId) -> bool {
    outcome.node(node).kind() == Kind::File
}

/// What the debounce says about asking for `node` right now.
///
/// Split out of [`HolderRuntime::state_for`] so the rule can be tested
/// against synthetic clocks instead of by sleeping, which is the same
/// reason [`super::toast::ToastQueue`] takes an injected `Instant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Brake {
    /// A different row (or the first row): start its timer.
    Rearm,
    /// The same row, not yet settled.
    Wait,
    /// Settled long enough — spend the ~50 ms.
    Fire,
}

fn brake(armed: Option<(NodeId, Instant)>, node: NodeId, now: Instant) -> Brake {
    match armed {
        Some((armed_node, since)) if armed_node == node => {
            if now.duration_since(since) < DEBOUNCE {
                Brake::Wait
            } else {
                Brake::Fire
            }
        }
        _ => Brake::Rearm,
    }
}

// ---------------------------------------------------------------------------
// The off-thread job
// ---------------------------------------------------------------------------

/// One query's result, tagged so [`HolderRuntime::poll`] can drop it when
/// it no longer applies (the same (node, serial, epoch) guard the link
/// runtime uses).
struct JobMessage {
    epoch: u64,
    node: NodeId,
    serial: u64,
    state: HolderState,
}

fn run_job(outcome: &RwLock<ScanOutcome>, node: NodeId) -> HolderState {
    // The arena lock is taken for the path and released before the ~50 ms
    // syscall runs — never held across it.
    let path: PathBuf = {
        let guard = read_outcome(outcome);
        guard.tree().path_of_node(node)
    };
    match winrm::holders_of(&path) {
        HolderQuery::Known { holders, total } => HolderState::Known {
            named: summarise(&holders, total),
            total,
        },
        HolderQuery::Failed(why) => HolderState::Failed(why),
    }
}

fn spawn_job(
    outcome: Arc<RwLock<ScanOutcome>>,
    node: NodeId,
    epoch: u64,
    serial: u64,
    tx: Sender<JobMessage>,
) -> bool {
    let spawned = thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            let state = run_job(&outcome, node);
            // The receiver may already be gone (session moved on); a failed
            // send just means nobody is listening anymore.
            let _ = tx.send(JobMessage {
                epoch,
                node,
                serial,
                state,
            });
        });
    match spawned {
        Ok(_handle) => true,
        Err(err) => {
            warn!(%err, "failed to spawn the open-handle job thread; this row's holders stay unknown");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime (owned by the event loop, mirrors `LinkRuntime`)
// ---------------------------------------------------------------------------

/// The advisory's session-long state: the job channel, the debounce arm,
/// the resolved answers, and which nodes still have a query in flight.
pub struct HolderRuntime {
    /// `false` under `--no-proc-sweep`/`NO_PROC_SWEEP`: no query ever runs
    /// and no line is ever shown (an empty line would claim a coverage the
    /// user just switched off).
    enabled: bool,
    tx: Sender<JobMessage>,
    rx: Receiver<JobMessage>,
    next_serial: u64,
    /// The row the cursor is currently resting on, and since when. Cleared
    /// once its query has been spawned.
    armed: Option<(NodeId, Instant)>,
    /// node -> serial of the currently-outstanding query.
    pending: HashMap<NodeId, u64>,
    /// Resolved answers for the current epoch.
    results: HashMap<NodeId, HolderState>,
}

impl HolderRuntime {
    pub fn new(enabled: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            enabled,
            tx,
            rx,
            next_serial: 0,
            armed: None,
            pending: HashMap::new(),
            results: HashMap::new(),
        }
    }

    /// The card's state for `node`, arming (and eventually spawning) the
    /// query. `now` is injected so the debounce is unit-testable without
    /// sleeping, exactly as [`super::toast::ToastQueue`] does with its TTL.
    ///
    /// Returns `None` for a row that must never be queried, and the card
    /// then shows no holder line at all — which is correct: saying
    /// "unknown" about a directory would be noise, not honesty.
    pub fn state_for(
        &mut self,
        lock: &Arc<RwLock<ScanOutcome>>,
        node: NodeId,
        epoch: u64,
        now: Instant,
    ) -> Option<HolderState> {
        if !self.enabled || !is_candidate(&read_outcome(lock), node) {
            self.armed = None;
            return None;
        }
        if let Some(state) = self.results.get(&node) {
            self.armed = None;
            return Some(state.clone());
        }
        if self.pending.contains_key(&node) {
            return Some(HolderState::Pending);
        }
        // The brake: a row must settle before it is worth ~50 ms.
        match brake(self.armed, node, now) {
            Brake::Rearm => {
                self.armed = Some((node, now));
                return Some(HolderState::Pending);
            }
            Brake::Wait => return Some(HolderState::Pending),
            Brake::Fire => self.armed = None,
        }
        if self.results.len() >= MAX_CACHED {
            debug!("open-handle cache full; dropping it rather than growing per visited row");
            self.results.clear();
        }
        self.next_serial += 1;
        let serial = self.next_serial;
        if !spawn_job(Arc::clone(lock), node, epoch, serial, self.tx.clone()) {
            // No thread, no answer — and saying so beats a row that sits on
            // "checking…" forever.
            let state = HolderState::Failed(RmFailure::NoSession);
            self.results.insert(node, state.clone());
            return Some(state);
        }
        self.pending.insert(node, serial);
        Some(HolderState::Pending)
    }

    /// Drives the render loop's frame cadence: a query is armed or in
    /// flight, so the card owes an update soon.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() || self.armed.is_some()
    }

    /// Non-blocking poll of the job channel. Returns whether anything
    /// landed.
    pub fn poll(&mut self, current_epoch: u64) -> bool {
        let mut landed = false;
        while let Ok(msg) = self.rx.try_recv() {
            landed = true;
            if msg.epoch != current_epoch {
                debug!(node = ?msg.node, "dropped a stale open-handle result (epoch moved)");
                continue;
            }
            if self.pending.get(&msg.node) != Some(&msg.serial) {
                debug!(node = ?msg.node, "dropped a superseded open-handle result");
                continue;
            }
            self.pending.remove(&msg.node);
            self.results.insert(msg.node, msg.state);
        }
        landed
    }

    /// A deletion just changed the frozen arena: every cached answer
    /// described a tree that no longer exists.
    ///
    /// Unreachable today (the Windows TUI has no deletion), and deliberately
    /// present anyway — the epoch is the interlock and a future deletion
    /// path must inherit it rather than rediscover the need for it.
    #[allow(dead_code)]
    pub fn on_deletion(&mut self) {
        self.armed = None;
        self.pending.clear();
        self.results.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(raw: u32) -> NodeId {
        NodeId::from_raw(raw)
    }

    fn holder(pid: u32, name: &str) -> Holder {
        Holder {
            pid,
            name: name.to_owned(),
            service: None,
        }
    }

    // ---- wording (the honesty contract) ----------------------------------

    #[test]
    fn no_state_renders_no_line() {
        assert!(card_lines(None, '⠋').is_empty());
    }

    /// The load-bearing line. Measured over a live Firefox profile, 34 of
    /// the 47 files that genuinely refused an open-for-DELETE reported no
    /// holder here — so an empty answer read as "nothing holds this" would
    /// be wrong most of the time it mattered. It must say "not proof".
    #[test]
    fn an_empty_answer_refuses_to_be_read_as_a_guarantee() {
        let line = card_lines(
            Some(&HolderState::Known {
                named: String::new(),
                total: 0,
            }),
            '⠋',
        );
        assert_eq!(
            line,
            vec!["no holder found · not proof — many real locks stay invisible".to_owned()]
        );
        assert!(line[0].contains("not proof"));
    }

    #[test]
    fn a_single_holder_is_named_without_a_count() {
        let named = summarise(&[holder(1234, "Code.exe")], 1);
        assert_eq!(named, "Code.exe (1234)");
        assert_eq!(
            card_lines(Some(&HolderState::Known { named, total: 1 }), '⠋'),
            vec!["open in Code.exe (1234)".to_owned()]
        );
    }

    /// `svchost.exe` reported 104 services from an unelevated process. The
    /// card names a couple and counts the rest rather than either lying
    /// about the total or trying to list them.
    #[test]
    fn a_crowd_is_counted_exactly_and_named_partially() {
        let holders: Vec<Holder> = (0..16)
            .map(|i| holder(900 + i, &format!("svc{i}")))
            .collect();
        let named = summarise(&holders, 104);
        assert_eq!(named, "svc0 (900), svc1 (901), +102 more");
        assert_eq!(
            card_lines(Some(&HolderState::Known { named, total: 104 }), '⠋'),
            vec!["open in 104 processes · svc0 (900), svc1 (901), +102 more".to_owned()]
        );
    }

    /// A holder whose registrant gave no application name still gets an
    /// identity, and a service falls back to its short name.
    #[test]
    fn a_nameless_holder_is_still_identified() {
        assert_eq!(label(&holder(77, "")), "pid 77");
        assert_eq!(
            label(&Holder {
                pid: 4,
                name: String::new(),
                service: Some("wuauserv".to_owned()),
            }),
            "wuauserv (4)"
        );
    }

    /// A refusal is a third thing, and must not be readable as either of
    /// the other two.
    #[test]
    fn a_refusal_never_reads_as_a_clean_bill_of_health() {
        let refused = card_lines(Some(&HolderState::Failed(RmFailure::Refused)), '⠋');
        let none = card_lines(
            Some(&HolderState::Known {
                named: String::new(),
                total: 0,
            }),
            '⠋',
        );
        assert!(
            refused[0].starts_with("open handles unknown · "),
            "{refused:?}"
        );
        assert_ne!(refused, none);
        for failure in [
            RmFailure::NoSession,
            RmFailure::NotRegistered,
            RmFailure::Refused,
        ] {
            let line = card_lines(Some(&HolderState::Failed(failure)), '⠋');
            assert!(line[0].starts_with("open handles unknown · "), "{line:?}");
        }
    }

    #[test]
    fn pending_carries_the_spinner_char() {
        assert_eq!(
            card_lines(Some(&HolderState::Pending), '⠙'),
            vec!["⠙ checking for open handles…".to_owned()]
        );
    }

    // ---- runtime transitions ---------------------------------------------

    /// The brake. Without it, arrowing through a directory spawns one
    /// 50 ms Restart Manager job per row passed over.
    #[test]
    fn a_row_must_settle_before_anything_is_asked() {
        let start = Instant::now();

        // First sighting of a row: start its timer, ask nothing.
        assert_eq!(brake(None, n(1), start), Brake::Rearm);

        // Scrolling past it — every intermediate row re-arms and fires
        // nothing, however fast the keys come.
        for step in [1, 5, 9, 40] {
            let now = start + Duration::from_millis(step);
            assert_eq!(brake(Some((n(1), start)), n(2), now), Brake::Rearm);
        }

        // Resting on it, but not yet long enough.
        assert_eq!(
            brake(
                Some((n(1), start)),
                n(1),
                start + DEBOUNCE - Duration::from_millis(1)
            ),
            Brake::Wait
        );

        // Settled: now it is worth the call.
        assert_eq!(
            brake(Some((n(1), start)), n(1), start + DEBOUNCE),
            Brake::Fire
        );
        assert_eq!(
            brake(Some((n(1), start)), n(1), start + Duration::from_secs(5)),
            Brake::Fire
        );
    }

    /// An armed row keeps the loop at frame cadence without having asked
    /// anything yet — otherwise the debounce would never elapse, because
    /// nothing would wake the loop to notice.
    #[test]
    fn an_armed_row_keeps_the_loop_awake_without_a_job_in_flight() {
        let mut rt = HolderRuntime::new(true);
        assert!(!rt.has_pending());
        rt.armed = Some((n(1), Instant::now()));
        assert!(rt.has_pending());
        assert!(rt.pending.is_empty(), "arming is not asking");
    }

    #[test]
    fn the_debounce_is_long_enough_to_survive_scrolling() {
        assert!(
            DEBOUNCE >= Duration::from_millis(150),
            "a debounce shorter than a keypress burst does not brake anything"
        );
    }

    #[test]
    fn poll_moves_a_node_from_pending_to_resolved() {
        let mut rt = HolderRuntime::new(true);
        rt.pending.insert(n(1), 1);
        let state = HolderState::Known {
            named: "Code.exe (1234)".to_owned(),
            total: 1,
        };
        rt.tx
            .send(JobMessage {
                epoch: 0,
                node: n(1),
                serial: 1,
                state: state.clone(),
            })
            .expect("send");

        assert!(rt.poll(0));
        assert!(!rt.has_pending());
        assert_eq!(rt.results.get(&n(1)), Some(&state));
    }

    #[test]
    fn poll_drops_a_result_from_a_stale_epoch() {
        let mut rt = HolderRuntime::new(true);
        rt.pending.insert(n(1), 1);
        rt.tx
            .send(JobMessage {
                epoch: 0,
                node: n(1),
                serial: 1,
                state: HolderState::Failed(RmFailure::Refused),
            })
            .expect("send");

        rt.poll(1);
        assert!(rt.has_pending(), "a stale result never resolves the query");
        assert!(rt.results.is_empty());
    }

    #[test]
    fn poll_drops_a_superseded_result() {
        let mut rt = HolderRuntime::new(true);
        rt.pending.insert(n(1), 2);
        rt.tx
            .send(JobMessage {
                epoch: 0,
                node: n(1),
                serial: 1,
                state: HolderState::Failed(RmFailure::Refused),
            })
            .expect("send");

        rt.poll(0);
        assert_eq!(rt.pending.get(&n(1)), Some(&2));
        assert!(rt.results.is_empty());
    }

    #[test]
    fn on_deletion_clears_every_cache() {
        let mut rt = HolderRuntime::new(true);
        rt.armed = Some((n(1), Instant::now()));
        rt.pending.insert(n(1), 1);
        rt.results
            .insert(n(2), HolderState::Failed(RmFailure::Refused));

        rt.on_deletion();

        assert!(rt.armed.is_none());
        assert!(rt.pending.is_empty());
        assert!(rt.results.is_empty());
    }

    /// `--no-proc-sweep` means no query and *no line*: an empty line would
    /// claim a coverage the user just switched off.
    #[test]
    fn a_disabled_runtime_never_arms_and_never_speaks() {
        let mut rt = HolderRuntime::new(false);
        assert!(!rt.has_pending());
        rt.armed = Some((n(1), Instant::now()));
        rt.on_deletion();
        assert!(!rt.has_pending());
    }
}
