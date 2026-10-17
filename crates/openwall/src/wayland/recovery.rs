// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded first-frame recovery; a queued present is not compositor readiness.
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Recovery {
    requested: bool,
    ready: bool,
    attempts: u8,
    deadline: Option<Instant>,
}

impl Recovery {
    pub fn request_callback(&mut self) -> bool {
        if self.requested || self.ready || self.attempts >= 3 {
            return false;
        }
        self.requested = true;
        true
    }

    pub fn submitted(&mut self, now: Instant) {
        self.requested = false;
        self.attempts = self.attempts.saturating_add(1);
        self.deadline =
            (!self.ready && self.attempts < 3).then_some(now + Duration::from_millis(200));
    }

    pub fn callback_done(&mut self) {
        self.ready = true;
        self.deadline = None;
    }

    pub fn take_due(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|deadline| deadline <= now) {
            self.deadline = None;
            true
        } else {
            false
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unacknowledged_initial_frame_has_only_three_timed_attempts() {
        let mut recovery = Recovery::default();
        let mut now = Instant::now();
        for _ in 0..2 {
            assert!(recovery.request_callback());
            assert!(!recovery.request_callback());
            recovery.submitted(now);
            assert!(!recovery.take_due(now + Duration::from_millis(199)));
            now += Duration::from_millis(200);
            assert!(recovery.take_due(now));
            assert!(!recovery.take_due(now));
        }
        assert!(recovery.request_callback());
        recovery.submitted(now);
        assert!(!recovery.request_callback());
        assert_eq!(recovery.deadline(), None);
        assert!(!recovery.take_due(now + Duration::from_secs(60)));
    }

    #[test]
    fn callback_cancels_recovery_without_scheduling_more_idle_frames() {
        let mut recovery = Recovery::default();
        let now = Instant::now();
        recovery.submitted(now);
        recovery.callback_done();
        assert!(!recovery.take_due(now + Duration::from_secs(1)));
        recovery.submitted(now + Duration::from_secs(2));
        assert_eq!(recovery.deadline(), None);
    }
}
