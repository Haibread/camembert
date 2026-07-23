//! Micro-animations (design slice 5): eased bar fills and donut slice
//! morphs on navigation/sort changes, sampled fresh every frame against
//! an animation-start [`Instant`] — no timers, no threads, and (once the
//! duration has elapsed) no per-frame cost beyond a single comparison.
//! Follows [`super::toast`]'s `_at`-suffixed pattern: every function here
//! that reads the clock has an injectable-`Instant` twin, so the easing
//! math and the [`Motion`] state machine are fully unit-tested without
//! sleeping.
//!
//! `--no-motion` (env `NO_MOTION`) disables all of this outright — see
//! [`Motion::new`] — bars and the donut then always render at their
//! target value, exactly as if any animation had already finished.
//!
//! Scope, resolved here rather than left ambiguous: bars do not track a
//! from-value per row (that would mean a per-row `Instant` cache, live
//! and re-keyed every frame — real per-frame allocation even when idle).
//! Instead every bar in the current view shares one clock and grows from
//! 0 together, uniformly, on the same trigger the donut morphs on. The
//! donut, which the design explicitly calls out for a real morph, keeps
//! exactly one previous-fractions vector (the last frame actually drawn)
//! to morph from.

use std::time::{Duration, Instant};

/// How long a bar fill or donut morph takes to settle — comfortably
/// inside the design's "no animation longer than ~150ms" budget.
pub const DURATION: Duration = Duration::from_millis(150);

/// Cubic ease-out (`1 - (1-t)^3`): starts fast, settles gently — used for
/// both animations so they read as one coherent motion rather than two
/// different feels.
pub fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Eased progress in `[0, 1]` of an animation that started at `start`,
/// sampled at `now`. Saturates at 1.0 once `DURATION` has elapsed — and
/// stays there forever after, at the same (trivial) cost.
pub fn progress_at(start: Instant, now: Instant) -> f64 {
    let elapsed = now.saturating_duration_since(start);
    if elapsed >= DURATION {
        return 1.0;
    }
    ease_out_cubic(elapsed.as_secs_f64() / DURATION.as_secs_f64())
}

/// Whether an animation started at `start` is still running at `now` —
/// used to decide whether the render loop needs to keep polling at frame
/// cadence instead of idling (see `ui::event_loop`'s poll deadline).
pub fn is_active_at(start: Instant, now: Instant) -> bool {
    now.saturating_duration_since(start) < DURATION
}

fn lerp(from: f64, to: f64, t: f64) -> f64 {
    from + (to - from) * t
}

/// Morph donut slice fractions `from` into `to` at eased progress `t`.
/// Same slice count: lerp position-for-position (the merge order in
/// [`super::wheel::build_slices`] is stable within one directory/sort, so
/// index `i` means the same slice before and after). Different count — a
/// different directory's children, or nothing to morph from yet — grows
/// in from 0 instead of guessing at a correspondence that is not there.
pub fn morph_fracs(from: &[f64], to: &[f64], t: f64) -> Vec<f64> {
    if from.len() == to.len() {
        from.iter().zip(to).map(|(&f, &g)| lerp(f, g, t)).collect()
    } else {
        to.iter().map(|&g| lerp(0.0, g, t)).collect()
    }
}

/// Per-session animation state, carried across frames by the render
/// loop: when the view last changed ([`Self::observe`]), and what the
/// donut last drew ([`Self::donut_fracs`]) — needed to morph from
/// *something* real rather than recomputing an approximation every
/// frame.
#[derive(Debug)]
pub struct Motion {
    enabled: bool,
    /// `Some` for `DURATION` after the last observed view change; stays
    /// `None` forever when motion is disabled — `observe` never sets it
    /// in that case, so every other method here is a one-branch no-op.
    anim_start: Option<Instant>,
    /// [`super::state::UiState::view_change_seq`] as of the last
    /// `observe` call — a new value means a navigation or sort just
    /// landed.
    seq: u64,
    /// The donut's fractions as last drawn (target values, not the
    /// mid-morph interpolation — see [`Self::donut_fracs_at`]) — the
    /// `from` side of the next morph.
    last_fracs: Vec<f64>,
}

impl Motion {
    /// `enabled = false` is `--no-motion`/`NO_MOTION`: every method below
    /// then behaves as if any animation had already finished.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            anim_start: None,
            seq: 0,
            last_fracs: Vec::new(),
        }
    }

    /// Call once per frame with the current
    /// [`super::state::UiState::view_change_seq`]: a value different
    /// from the last call starts a fresh animation window. The very
    /// first call (session start) never triggers — there is nothing to
    /// animate *from* yet, so the initial screen simply appears at rest.
    pub fn observe(&mut self, seq: u64) {
        self.observe_at(seq, Instant::now());
    }

    pub fn observe_at(&mut self, seq: u64, now: Instant) {
        if self.enabled && seq != self.seq {
            self.anim_start = Some(now);
        }
        self.seq = seq;
    }

    /// Whether an animation is currently in flight.
    pub fn is_active(&self) -> bool {
        self.is_active_at(Instant::now())
    }

    pub fn is_active_at(&self, now: Instant) -> bool {
        self.anim_start
            .is_some_and(|start| is_active_at(start, now))
    }

    /// The uniform 0->1 reveal multiplier every table bar's fraction is
    /// scaled by this frame.
    pub fn bar_progress(&self) -> f64 {
        self.bar_progress_at(Instant::now())
    }

    pub fn bar_progress_at(&self, now: Instant) -> f64 {
        match self.anim_start {
            Some(start) if self.enabled => progress_at(start, now),
            _ => 1.0,
        }
    }

    /// The donut fractions to actually draw this frame: morphed from
    /// whatever was last drawn while an animation is in flight, the raw
    /// `to` otherwise. Always remembers `to` as the new "last drawn" —
    /// call this every frame the donut renders, active animation or not,
    /// so `last_fracs` is never stale when the next navigation lands.
    pub fn donut_fracs(&mut self, to: &[f64]) -> Vec<f64> {
        self.donut_fracs_at(to, Instant::now())
    }

    pub fn donut_fracs_at(&mut self, to: &[f64], now: Instant) -> Vec<f64> {
        let result = match self.anim_start {
            Some(start) if self.enabled && is_active_at(start, now) => {
                morph_fracs(&self.last_fracs, to, progress_at(start, now))
            }
            _ => to.to_vec(),
        };
        self.last_fracs = to.to_vec();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_boundaries_and_shape() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        // Ease-out: more than half the distance is already covered past
        // the midpoint of the input — the "fast start" shape.
        assert!(ease_out_cubic(0.5) > 0.5);
        assert!(ease_out_cubic(0.25) < ease_out_cubic(0.75), "monotonic");
        // Out-of-range input clamps instead of over/undershooting.
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
    }

    #[test]
    fn progress_at_tracks_elapsed_time_and_saturates() {
        let t0 = Instant::now();
        assert_eq!(progress_at(t0, t0), 0.0, "just started");
        let mid = progress_at(t0, t0 + DURATION / 2);
        assert!(mid > 0.0 && mid < 1.0);
        assert_eq!(progress_at(t0, t0 + DURATION), 1.0, "exactly done");
        assert_eq!(
            progress_at(t0, t0 + DURATION * 10),
            1.0,
            "long past done: still 1.0, not an error"
        );
    }

    #[test]
    fn is_active_at_boundary() {
        let t0 = Instant::now();
        assert!(is_active_at(t0, t0));
        assert!(is_active_at(t0, t0 + DURATION - Duration::from_millis(1)));
        assert!(!is_active_at(t0, t0 + DURATION));
        assert!(!is_active_at(t0, t0 + DURATION * 2));
    }

    #[test]
    fn morph_fracs_same_length_lerps_elementwise() {
        let from = vec![0.5, 0.5];
        let to = vec![0.2, 0.8];
        assert_eq!(morph_fracs(&from, &to, 0.0), from);
        assert_eq!(morph_fracs(&from, &to, 1.0), to);
        let half = morph_fracs(&from, &to, 0.5);
        assert!((half[0] - 0.35).abs() < 1e-9);
        assert!((half[1] - 0.65).abs() < 1e-9);
    }

    #[test]
    fn morph_fracs_different_length_grows_in_from_zero() {
        let from = vec![1.0]; // a single "rest" slice in the previous dir
        let to = vec![0.3, 0.3, 0.4];
        assert_eq!(morph_fracs(&from, &to, 0.0), vec![0.0, 0.0, 0.0]);
        assert_eq!(morph_fracs(&from, &to, 1.0), to);
        let half = morph_fracs(&from, &to, 0.5);
        assert!((half[0] - 0.15).abs() < 1e-9);
    }

    #[test]
    fn motion_disabled_always_reports_the_finished_state() {
        let mut motion = Motion::new(false);
        let t0 = Instant::now();
        motion.observe_at(1, t0);
        motion.observe_at(2, t0); // would be a "navigation" if enabled
        assert!(!motion.is_active_at(t0), "disabled: never animating");
        assert_eq!(motion.bar_progress_at(t0), 1.0);
        assert_eq!(motion.donut_fracs_at(&[0.5, 0.5], t0), vec![0.5, 0.5]);
    }

    #[test]
    fn motion_starts_an_animation_only_when_the_seq_changes() {
        let mut motion = Motion::new(true);
        let t0 = Instant::now();
        // `Motion::new`'s internal seq starts at 0, matching
        // `UiState::view_change_seq`'s own starting value — observing
        // that same baseline first is what "no prior seq to differ
        // from" means in practice, not an arbitrary starting number.
        motion.observe_at(0, t0);
        assert!(
            !motion.is_active_at(t0),
            "first observe at the baseline seq: no prior seq to differ from"
        );

        let t1 = t0 + Duration::from_millis(10);
        motion.observe_at(0, t1); // same seq: no new trigger
        assert!(!motion.is_active_at(t1));

        let t2 = t1 + Duration::from_millis(10);
        motion.observe_at(1, t2); // seq changed: navigation/sort landed
        assert!(motion.is_active_at(t2));
        assert!(motion.is_active_at(t2 + DURATION - Duration::from_millis(1)));
        assert!(!motion.is_active_at(t2 + DURATION));
    }

    #[test]
    fn motion_bar_progress_eases_from_zero_to_one_over_the_animation_window() {
        let mut motion = Motion::new(true);
        let t0 = Instant::now();
        motion.observe_at(1, t0);
        motion.observe_at(2, t0); // trigger

        assert_eq!(motion.bar_progress_at(t0), 0.0);
        let mid = motion.bar_progress_at(t0 + DURATION / 2);
        assert!(mid > 0.0 && mid < 1.0);
        assert_eq!(motion.bar_progress_at(t0 + DURATION), 1.0);
    }

    #[test]
    fn motion_donut_fracs_morphs_from_the_last_drawn_value() {
        let mut motion = Motion::new(true);
        let t0 = Instant::now();
        motion.observe_at(0, t0); // the baseline seq — see the seq test's note
        // First ever draw: no trigger yet (seq 0 is the baseline) — draws
        // at target immediately.
        assert_eq!(motion.donut_fracs_at(&[1.0], t0), vec![1.0]);

        // Navigate: seq changes, and the previous frame's fracs ([1.0])
        // become the morph's `from`.
        let t1 = t0 + Duration::from_millis(10);
        motion.observe_at(1, t1);
        let target = vec![0.4, 0.6];
        let morphed = motion.donut_fracs_at(&target, t1);
        assert_eq!(
            morphed,
            vec![0.0, 0.0],
            "different slice count: grows in from 0, t≈0"
        );

        let later = motion.donut_fracs_at(&target, t1 + DURATION);
        assert_eq!(later, target, "animation finished: at the target exactly");
    }
}
