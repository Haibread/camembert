//! Transient top-right notifications (design slice 4): dump written,
//! deletion done, scan finished. Several stack, oldest on top, each
//! auto-dismissing after [`ToastQueue::TTL`].
//!
//! Distinct from the footer's [`super::Flash`]: a flash is synchronous
//! feedback tied to the keypress that produced it (a mark refusal,
//! "nothing marked") and lives right next to the key hints it explains a
//! failure of. A toast announces something that just *happened*
//! (usually asynchronously to input — the scan finishing, a dump landing
//! on disk) and would be easy to miss in the footer while the user is
//! looking at the table. Keeping both means each message goes to exactly
//! one place instead of the two mechanisms competing to show the same
//! thing.
//!
//! Expiry is always checked against an *injected* [`Instant`] — nothing
//! here calls `Instant::now()` except the `_now`-suffixed convenience
//! wrappers real call sites use — so the TTL logic is unit-testable
//! without sleeping.

use std::time::{Duration, Instant};

/// One active notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    expires_at: Instant,
}

/// Stack of active toasts, oldest first (arrival order — the render side
/// draws them top-to-bottom in this order, newest at the bottom).
#[derive(Debug, Default)]
pub struct ToastQueue {
    toasts: Vec<Toast>,
}

impl ToastQueue {
    /// How long a toast stays on screen once pushed.
    pub const TTL: Duration = Duration::from_secs(4);

    pub fn new() -> Self {
        Self::default()
    }

    /// Push a toast timed to expire `TTL` after `now`.
    pub fn push_at(&mut self, now: Instant, message: impl Into<String>) {
        self.toasts.push(Toast {
            message: message.into(),
            expires_at: now + Self::TTL,
        });
    }

    /// Convenience wrapper over [`Self::push_at`] using the real clock —
    /// every production call site.
    pub fn push(&mut self, message: impl Into<String>) {
        self.push_at(Instant::now(), message);
    }

    /// Drop everything expired as of `now`, then return what remains.
    pub fn active_at(&mut self, now: Instant) -> &[Toast] {
        self.toasts.retain(|toast| toast.expires_at > now);
        &self.toasts
    }

    /// Convenience wrapper over [`Self::active_at`] using the real clock —
    /// the render loop's per-frame call.
    pub fn active(&mut self) -> &[Toast] {
        self.active_at(Instant::now())
    }

    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toast_is_active_until_its_ttl_elapses() {
        let mut queue = ToastQueue::new();
        let t0 = Instant::now();
        queue.push_at(t0, "dump written: /tmp/x.cmbt");

        assert_eq!(queue.active_at(t0).len(), 1, "just pushed: active");
        assert_eq!(
            queue.active_at(t0 + Duration::from_secs(3)).len(),
            1,
            "still within the TTL"
        );
        assert!(
            queue
                .active_at(t0 + ToastQueue::TTL + Duration::from_millis(1))
                .is_empty(),
            "past the TTL: expired"
        );
    }

    #[test]
    fn expired_toasts_are_pruned_and_do_not_resurrect() {
        let mut queue = ToastQueue::new();
        let t0 = Instant::now();
        queue.push_at(t0, "scan finished in 1.2s");
        assert!(queue.active_at(t0 + Duration::from_secs(10)).is_empty());
        // Querying again at an earlier "now" than the previous prune
        // still shows nothing — pruning is a real removal, not a filter.
        assert!(queue.active_at(t0).is_empty(), "pruned, not just filtered");
    }

    #[test]
    fn several_toasts_stack_oldest_first_and_expire_independently() {
        let mut queue = ToastQueue::new();
        let t0 = Instant::now();
        queue.push_at(t0, "first");
        queue.push_at(t0 + Duration::from_secs(2), "second");

        let messages: Vec<&str> = queue
            .active_at(t0 + Duration::from_secs(3))
            .iter()
            .map(|toast| toast.message.as_str())
            .collect();
        assert_eq!(messages, ["first", "second"], "oldest first, both alive");

        // "first" (expires at t0+4) is gone by t0+5, "second" (expires at
        // t0+6) is not.
        let messages: Vec<&str> = queue
            .active_at(t0 + Duration::from_secs(5))
            .iter()
            .map(|toast| toast.message.as_str())
            .collect();
        assert_eq!(messages, ["second"], "first expired independently");
    }

    #[test]
    fn is_empty_reflects_unpruned_state() {
        let mut queue = ToastQueue::new();
        assert!(queue.is_empty());
        queue.push("x");
        assert!(!queue.is_empty());
    }
}
