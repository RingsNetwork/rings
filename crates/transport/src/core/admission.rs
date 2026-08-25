//! Shared atomic admission state model.
//!
//! The pure transition function is the single specification for cancellation,
//! irrevocable admission, successful acceptance, and explicit rollback. Users
//! expose only the events their layer can safely perform: transport sends never
//! roll back after becoming irrevocable, while a higher-level detached transfer
//! may roll back before any backend send was accepted.

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Phase of one cancellable admission transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AdmissionPhase {
    /// No irreversible work has started.
    Pending = 0,
    /// Admission was cancelled before success.
    Cancelled = 1,
    /// Work crossed its final cancellation-safe boundary.
    Irrevocable = 2,
    /// The admitted operation completed its acceptance boundary.
    Accepted = 3,
}

impl AdmissionPhase {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Pending,
            1 => Self::Cancelled,
            2 => Self::Irrevocable,
            3 => Self::Accepted,
            _ => Self::Cancelled,
        }
    }

    /// Apply one event to the pure state machine.
    ///
    /// `None` means the event is illegal or already terminal in this phase.
    pub const fn transition(self, event: AdmissionEvent) -> Option<Self> {
        match (self, event) {
            (Self::Pending, AdmissionEvent::Cancel)
            | (Self::Irrevocable, AdmissionEvent::Rollback) => Some(Self::Cancelled),
            (Self::Pending, AdmissionEvent::MarkIrrevocable) => Some(Self::Irrevocable),
            (Self::Irrevocable, AdmissionEvent::Accept) => Some(Self::Accepted),
            _ => None,
        }
    }
}

/// Event accepted by the admission transition model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionEvent {
    /// Cancel work that is still pending.
    Cancel,
    /// Cross the final cancellation-safe boundary.
    MarkIrrevocable,
    /// Publish successful acceptance of irrevocable work.
    Accept,
    /// Return irrevocable higher-level admission to a terminal cancelled state.
    Rollback,
}

/// Cloneable atomic shell around [`AdmissionPhase::transition`].
#[derive(Clone)]
pub struct AtomicAdmission {
    state: Arc<AtomicU8>,
}

impl AtomicAdmission {
    /// Construct one pending admission transaction.
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(AdmissionPhase::Pending as u8)),
        }
    }

    /// Observe the current admission phase.
    pub fn phase(&self) -> AdmissionPhase {
        AdmissionPhase::from_raw(self.state.load(Ordering::Acquire))
    }

    /// Atomically apply one event.
    ///
    /// On failure, returns the phase that rejected the event.
    pub fn try_transition(&self, event: AdmissionEvent) -> Result<AdmissionPhase, AdmissionPhase> {
        let mut raw = self.state.load(Ordering::Acquire);
        loop {
            let current = AdmissionPhase::from_raw(raw);
            let Some(next) = current.transition(event) else {
                return Err(current);
            };
            match self.state.compare_exchange_weak(
                raw,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(next),
                Err(observed) => raw = observed,
            }
        }
    }
}

impl Default for AtomicAdmission {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::AdmissionEvent;
    use super::AdmissionPhase;
    use super::AtomicAdmission;

    #[test]
    fn transition_table_has_only_declared_edges() {
        use AdmissionEvent::Accept;
        use AdmissionEvent::Cancel;
        use AdmissionEvent::MarkIrrevocable;
        use AdmissionEvent::Rollback;
        use AdmissionPhase::Accepted;
        use AdmissionPhase::Cancelled;
        use AdmissionPhase::Irrevocable;
        use AdmissionPhase::Pending;

        assert_eq!(Pending.transition(Cancel), Some(Cancelled));
        assert_eq!(Pending.transition(MarkIrrevocable), Some(Irrevocable));
        assert_eq!(Irrevocable.transition(Accept), Some(Accepted));
        assert_eq!(Irrevocable.transition(Rollback), Some(Cancelled));
        for terminal in [Cancelled, Accepted] {
            for event in [Cancel, MarkIrrevocable, Accept, Rollback] {
                assert_eq!(terminal.transition(event), None);
            }
        }
    }

    #[test]
    fn atomic_shell_observes_the_pure_model() {
        let admission = AtomicAdmission::new();
        assert_eq!(admission.phase(), AdmissionPhase::Pending);
        assert_eq!(
            admission.try_transition(AdmissionEvent::MarkIrrevocable),
            Ok(AdmissionPhase::Irrevocable)
        );
        assert_eq!(
            admission.try_transition(AdmissionEvent::Accept),
            Ok(AdmissionPhase::Accepted)
        );
        assert_eq!(
            admission.try_transition(AdmissionEvent::Cancel),
            Err(AdmissionPhase::Accepted)
        );
    }
}
