// SPDX-License-Identifier: AGPL-3.0-or-later
//! Independent playback policy reasons. Presentation/transaction eligibility remains
//! separate: clearing policy reasons never authorizes an unpublished source to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    User,
    PowerPolicy,
}

impl PauseReason {
    fn bit(self) -> u8 {
        match self {
            Self::User => 1,
            Self::PowerPolicy => 2,
        }
    }
}

#[derive(Default)]
pub struct Pauses(u8);
impl Pauses {
    /// Returns whether this producer's reason changed; duplicate events are idempotent.
    pub fn set(&mut self, reason: PauseReason, paused: bool) -> bool {
        let previous = self.0;
        if paused {
            self.0 |= reason.bit();
        } else {
            self.0 &= !reason.bit();
        }
        previous != self.0
    }
    pub fn is_paused(&self) -> bool {
        self.0 != 0
    }
    pub fn active(&self) -> impl Iterator<Item = PauseReason> + '_ {
        [PauseReason::User, PauseReason::PowerPolicy]
            .into_iter()
            .filter(|reason| self.0 & reason.bit() != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_reasons_combine_and_duplicate_updates_are_idempotent() {
        for first in [PauseReason::User, PauseReason::PowerPolicy] {
            let second = if first == PauseReason::User {
                PauseReason::PowerPolicy
            } else {
                PauseReason::User
            };
            let mut pauses = Pauses::default();
            assert!(!pauses.is_paused());
            assert!(!pauses.set(first, false));
            assert!(pauses.set(first, true));
            assert!(!pauses.set(first, true));
            assert!(pauses.set(second, true));
            assert_eq!(pauses.active().count(), 2);
            assert!(pauses.set(first, false));
            assert!(pauses.is_paused());
            assert!(!pauses.set(first, false));
            assert_eq!(pauses.active().collect::<Vec<_>>(), vec![second]);
            assert!(pauses.set(second, false));
            assert!(!pauses.is_paused());
        }
    }
}
