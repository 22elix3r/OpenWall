// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cancelling readiness does not cancel a compositor's eventual terminal event.
use std::collections::BTreeMap;

const LIMIT: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum RegistrationError {
    DuplicateSubmission,
    Capacity,
}
impl std::fmt::Display for RegistrationError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::DuplicateSubmission => "presentation submission already pending",
            Self::Capacity => "pending presentation feedback limit exceeded",
        })
    }
}
impl std::error::Error for RegistrationError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Presented,
    Discarded,
    CancelledPresented,
    CancelledDiscarded,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Snapshot {
    pub pending: usize,
    pub active: usize,
    pub cancelled: u64,
    pub cancelled_presented: u64,
    pub cancelled_discarded: u64,
    pub limit: usize,
}

#[derive(Default)]
pub struct Tracker {
    // false = active readiness request, true = locally cancelled, still awaiting
    // the protocol's terminal event. Keeping it occupies bounded capacity.
    pending: BTreeMap<u64, bool>,
    cancelled: u64,
    cancelled_presented: u64,
    cancelled_discarded: u64,
}

impl Tracker {
    pub fn register(&mut self, id: u64) -> Result<(), RegistrationError> {
        if self.pending.contains_key(&id) {
            return Err(RegistrationError::DuplicateSubmission);
        }
        if self.pending.len() == LIMIT {
            return Err(RegistrationError::Capacity);
        }
        self.pending.insert(id, false);
        Ok(())
    }

    pub fn cancel(&mut self, id: u64) -> bool {
        let Some(cancelled) = self.pending.get_mut(&id) else {
            return false;
        };
        if *cancelled {
            return false;
        }
        *cancelled = true;
        self.cancelled += 1;
        true
    }

    pub fn cancel_all(&mut self) {
        for cancelled in self.pending.values_mut() {
            if !*cancelled {
                *cancelled = true;
                self.cancelled += 1;
            }
        }
    }

    pub fn complete(&mut self, id: u64, presented: bool) -> Option<Completion> {
        let cancelled = self.pending.remove(&id)?;
        Some(match (cancelled, presented) {
            (false, true) => Completion::Presented,
            (false, false) => Completion::Discarded,
            (true, true) => {
                self.cancelled_presented += 1;
                Completion::CancelledPresented
            }
            (true, false) => {
                self.cancelled_discarded += 1;
                Completion::CancelledDiscarded
            }
        })
    }

    pub fn has_active(&self) -> bool {
        self.pending.values().any(|cancelled| !cancelled)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            pending: self.pending.len(),
            active: self
                .pending
                .values()
                .filter(|cancelled| !**cancelled)
                .count(),
            cancelled: self.cancelled,
            cancelled_presented: self.cancelled_presented,
            cancelled_discarded: self.cancelled_discarded,
            limit: LIMIT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_cancelled_feedback_cannot_complete_the_restoration_wait() {
        for presented in [false, true] {
            let mut tracker = Tracker::default();
            tracker.register(11).unwrap();
            assert!(tracker.cancel(11));
            tracker.register(12).unwrap();
            assert_eq!(
                tracker.complete(11, presented),
                Some(if presented {
                    Completion::CancelledPresented
                } else {
                    Completion::CancelledDiscarded
                })
            );
            assert!(tracker.has_active());
            assert_eq!(tracker.snapshot().pending, 1);
            assert_eq!(tracker.complete(12, true), Some(Completion::Presented));
            assert!(!tracker.has_active());
            assert_eq!(tracker.complete(11, true), None);
        }
    }

    #[test]
    fn restored_then_cancelled_terminal_order_is_also_safe() {
        let mut tracker = Tracker::default();
        tracker.register(1).unwrap();
        tracker.cancel(1);
        tracker.register(2).unwrap();
        assert_eq!(tracker.complete(2, true), Some(Completion::Presented));
        assert!(!tracker.has_active());
        assert_eq!(tracker.snapshot().pending, 1);
        assert_eq!(
            tracker.complete(1, false),
            Some(Completion::CancelledDiscarded)
        );
        assert_eq!(tracker.snapshot().pending, 0);
    }

    #[test]
    fn cancelled_requests_retain_capacity_until_terminal_feedback() {
        let mut tracker = Tracker::default();
        for id in 0..LIMIT as u64 {
            tracker.register(id).unwrap();
        }
        tracker.cancel_all();
        assert!(!tracker.has_active());
        assert_eq!(tracker.register(99), Err(RegistrationError::Capacity));
        assert_eq!(
            tracker.register(0),
            Err(RegistrationError::DuplicateSubmission)
        );
        assert!(!tracker.cancel(0));
        assert_eq!(tracker.snapshot().cancelled, LIMIT as u64);
        assert_eq!(
            tracker.complete(0, false),
            Some(Completion::CancelledDiscarded)
        );
        tracker.register(99).unwrap();
        assert!(tracker.has_active());
    }
}
