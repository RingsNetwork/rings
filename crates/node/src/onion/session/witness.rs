//! The client endpoint's witness of one session: what it knows of the session at an instant,
//! for a user that names how its stream ended without waiting on the stream's events (#950).
//!
//! ```text
//! Live ⇄ Stalled             a reply held behind a missing one / the missing one arrived
//!                            (OnionReorder::is_stalled, after every reply)
//! Live | Stalled → Failed    the driver or the pump failed the session closed
//! ```
//!
//! Laws (tested in `session::tests::test_witness`):
//!
//! - **Absorbing.** Once `Failed`, the witness reads `Failed` for the rest of the session.
//! - **Report before end.** The driver records `Failed` before it hands its user `Failed`, and
//!   the `tcp` pump before it resets its local stream, so a user that observes the stream's end
//!   and then reads the witness reads the failure.
//! - **Whole stream.** `Failed` is recorded only while the reply direction is open: once the
//!   world's `fin` reached the user its stream is whole, and its end is orderly (`tcp`, Law fail
//!   closed).

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// What the client endpoint knows of a session at an instant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnionSessionCondition {
    /// No failure was observed and no reply is known missing.
    Live,
    /// A reply is known missing: a later one arrived and is held behind it.
    Stalled,
    /// The session failed closed.
    Failed,
}

impl OnionSessionCondition {
    /// The witness's encoding of the condition.
    const fn code(self) -> u8 {
        match self {
            Self::Live => 0,
            Self::Stalled => 1,
            Self::Failed => 2,
        }
    }

    /// The condition of a witness's `code`, the inverse of [`Self::code`]; every other byte is
    /// read as `Failed`, so a witness never reads healthier than it was set.
    #[cfg(any(rings_native, test))]
    const fn of_code(code: u8) -> Self {
        match code {
            0 => Self::Live,
            1 => Self::Stalled,
            _ => Self::Failed,
        }
    }
}

/// A shared, lock-free witness of one session's [`OnionSessionCondition`]: the driver and the
/// pump write it, and the stream's user reads it at any instant.
#[derive(Clone, Debug, Default)]
pub(crate) struct OnionSessionWitness(Arc<AtomicU8>);

impl OnionSessionWitness {
    /// The session's condition now; only native streams have a reader.
    #[cfg(any(rings_native, test))]
    pub(crate) fn condition(&self) -> OnionSessionCondition {
        OnionSessionCondition::of_code(self.0.load(Ordering::Acquire))
    }

    /// Record whether a reply is known missing; a failed session stays failed (Law Absorbing).
    pub(crate) fn record_stalled(&self, stalled: bool) {
        let condition = if stalled {
            OnionSessionCondition::Stalled
        } else {
            OnionSessionCondition::Live
        };
        let failed = OnionSessionCondition::Failed.code();
        let _ = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != failed).then_some(condition.code())
            });
    }

    /// Record that the session failed closed.
    pub(crate) fn record_failed(&self) {
        self.0
            .store(OnionSessionCondition::Failed.code(), Ordering::Release);
    }
}
