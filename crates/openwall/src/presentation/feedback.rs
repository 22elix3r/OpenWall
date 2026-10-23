// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cancelling readiness does not cancel a compositor's eventual terminal event.
use std::{cell::Cell, collections::BTreeMap, rc::Rc};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Presented,
    Discarded,
    CancelledPresented,
    CancelledDiscarded,
}

/// An owner-thread receipt for exactly one feedback registration. It retains only
/// terminal bookkeeping, never native surfaces or media. Dropping it does not cancel
/// protocol feedback; cancelled requests still occupy capacity until a terminal event.
#[derive(Clone, Debug)]
pub struct Receipt {
    id: u64,
    completion: Rc<Cell<Option<Completion>>>,
}

impl Receipt {
    pub fn submission_id(&self) -> u64 {
        self.id
    }
    pub fn completion(&self) -> Option<Completion> {
        self.completion.get()
    }
}

struct Pending {
    cancelled: bool,
    completion: Rc<Cell<Option<Completion>>>,
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
    // Locally cancelled requests still await protocol terminal events and occupy
    // bounded capacity. Per-registration receipts outlive diagnostic history.
    pending: BTreeMap<u64, Pending>,
    cancelled: u64,
    cancelled_presented: u64,
    cancelled_discarded: u64,
}

impl Tracker {
    pub fn register(&mut self, id: u64) -> Result<Receipt, RegistrationError> {
        if self.pending.contains_key(&id) {
            return Err(RegistrationError::DuplicateSubmission);
        }
        if self.pending.len() == LIMIT {
            return Err(RegistrationError::Capacity);
        }
        let completion = Rc::new(Cell::new(None));
        self.pending.insert(
            id,
            Pending {
                cancelled: false,
                completion: completion.clone(),
            },
        );
        Ok(Receipt { id, completion })
    }

    pub fn cancel(&mut self, id: u64) -> bool {
        let Some(pending) = self.pending.get_mut(&id) else {
            return false;
        };
        if pending.cancelled {
            return false;
        }
        pending.cancelled = true;
        self.cancelled += 1;
        true
    }

    pub fn cancel_all(&mut self) {
        for pending in self.pending.values_mut() {
            if !pending.cancelled {
                pending.cancelled = true;
                self.cancelled += 1;
            }
        }
    }

    pub fn complete(&mut self, id: u64, presented: bool) -> Option<Completion> {
        let pending = self.pending.remove(&id)?;
        let completion = match (pending.cancelled, presented) {
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
        };
        pending.completion.set(Some(completion));
        Some(completion)
    }

    pub fn has_active(&self) -> bool {
        self.pending.values().any(|pending| !pending.cancelled)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            pending: self.pending.len(),
            active: self
                .pending
                .values()
                .filter(|pending| !pending.cancelled)
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
        assert_eq!(
            tracker.register(99).unwrap_err(),
            RegistrationError::Capacity
        );
        assert_eq!(
            tracker.register(0).unwrap_err(),
            RegistrationError::DuplicateSubmission
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

    #[test]
    fn receipts_keep_exact_terminal_proof_after_many_later_presentations() {
        let mut tracker = Tracker::default();
        let first = tracker.register(0).unwrap();
        let cancelled = tracker.register(1).unwrap();
        tracker.cancel(1);
        assert_eq!(first.completion(), None);
        assert_eq!(cancelled.completion(), None);
        let restored = tracker.register(2).unwrap();
        tracker.complete(1, true);
        assert_eq!(cancelled.completion(), Some(Completion::CancelledPresented));
        assert_eq!(restored.completion(), None);
        tracker.complete(2, true);
        tracker.complete(0, true);
        for id in 3..10_000 {
            let receipt = tracker.register(id).unwrap();
            assert_eq!(receipt.submission_id(), id);
            assert_eq!(receipt.completion(), None);
            tracker.complete(id, id % 2 == 0);
            assert_eq!(
                receipt.completion(),
                Some(if id % 2 == 0 {
                    Completion::Presented
                } else {
                    Completion::Discarded
                })
            );
        }
        assert_eq!(tracker.snapshot().pending, 0);
        assert_eq!(first.completion(), Some(Completion::Presented));
        assert_eq!(restored.completion(), Some(Completion::Presented));
        assert_eq!(cancelled.completion(), Some(Completion::CancelledPresented));
        assert_eq!(tracker.complete(1, false), None);
        assert_eq!(cancelled.completion(), Some(Completion::CancelledPresented));
    }
}
