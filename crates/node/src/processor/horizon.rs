//! The signing horizon of a node's own delegation: until when the process can send anything
//! (#1063).
//!
//! A node signs every frame it sends with its own delegation, whose expiry `e` is fixed for the
//! life of the process: rotating it needs a restart. From `h = e − Δ` on, the sender refuses
//! every frame that carries it ([`signing_horizon_ms`]), so the node is mute, and it stays mute:
//!
//! ```text
//!   Signing ──(t ≥ h − lead)──▶ Expiring ──(t ≥ h)──▶ Mute      (Mute is absorbing)
//! ```
//!
//! This module only classifies instants; the shell acts on them. A native daemon ends at `h`, so
//! a supervisor restarts it with a fresh delegation (`native::horizon`), the processor refuses
//! to start mute ([`super::ProcessorBuilder::build`]), and the operator snapshot reports
//! `overlay_ready = false` once mute.

use rings_core::delegation::Delegation;
use rings_core::swarm::signing_horizon_ms;

use crate::error::Error;
use crate::error::Result;

/// How long before its horizon a node starts warning that it will go mute: one day, so an
/// operator can rotate the delegation before the process ends.
pub const SIGNING_HORIZON_LEAD_MS: u128 = 24 * 60 * 60 * 1000;

/// What a node can do at one instant, by its own delegation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigningState {
    /// Frames leave, and the horizon is `remaining_ms` away, more than
    /// [`SIGNING_HORIZON_LEAD_MS`].
    Signing {
        /// Time left until the horizon.
        remaining_ms: u128,
    },
    /// Frames leave, but the horizon is `remaining_ms` away, within the lead.
    Expiring {
        /// Time left until the horizon.
        remaining_ms: u128,
    },
    /// Past the horizon: every frame is refused until the process restarts.
    Mute,
}

/// The signing horizon `h = e − Δ` of a node's own delegation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationHorizon {
    /// The delegation's expiry `e`.
    expires_at_ms: u128,
    /// `h = e − Δ`.
    horizon_ms: u128,
}

impl DelegationHorizon {
    /// The horizon of `delegation`.
    pub fn of(delegation: &Delegation) -> Self {
        Self::expiring_at(delegation.expires_at_ms())
    }

    /// The horizon of a delegation that expires at `expires_at_ms`.
    pub const fn expiring_at(expires_at_ms: u128) -> Self {
        Self {
            expires_at_ms,
            horizon_ms: signing_horizon_ms(expires_at_ms),
        }
    }

    /// The delegation's expiry `e`.
    pub const fn expires_at_ms(self) -> u128 {
        self.expires_at_ms
    }

    /// `h`: the first instant at which the node can send nothing.
    pub const fn horizon_ms(self) -> u128 {
        self.horizon_ms
    }

    /// Whether the node can still send at `now_ms`: `now < h`.
    pub const fn signs_at(self, now_ms: u128) -> bool {
        now_ms < self.horizon_ms
    }

    /// A process may start at `now_ms` only while it can still send: a delegation with less
    /// than `Δ` left would make it mute from its first frame.
    ///
    /// # Errors
    ///
    /// [`Error::DelegationHorizonReached`] at or past the horizon.
    pub const fn admitted_at(self, now_ms: u128) -> Result<()> {
        if self.signs_at(now_ms) {
            Ok(())
        } else {
            Err(self.reached())
        }
    }

    /// The error of a node at or past this horizon.
    pub const fn reached(self) -> Error {
        Error::DelegationHorizonReached {
            expires_at_ms: self.expires_at_ms,
            horizon_ms: self.horizon_ms,
        }
    }

    /// The state at `now_ms` (module diagram). Law: `state_at(t) = Mute ⇔ ¬signs_at(t)`, and the
    /// state never goes back along the diagram as `t` grows.
    pub const fn state_at(self, now_ms: u128) -> SigningState {
        if !self.signs_at(now_ms) {
            return SigningState::Mute;
        }
        let remaining_ms = self.horizon_ms - now_ms;
        if remaining_ms <= SIGNING_HORIZON_LEAD_MS {
            SigningState::Expiring { remaining_ms }
        } else {
            SigningState::Signing { remaining_ms }
        }
    }
}

#[cfg(test)]
mod tests {
    use rings_core::swarm::signing_horizon_ms;

    use super::DelegationHorizon;
    use super::SigningState;
    use super::SIGNING_HORIZON_LEAD_MS;
    use crate::error::Error;

    /// The states follow the diagram: `Mute` exactly when not signing, `Expiring` only within the
    /// lead, and no instant goes back to an earlier state, around the lead's start and the
    /// horizon of an expiry well past both.
    #[test]
    fn test_states_follow_the_horizon_and_never_go_back() {
        let expires_at_ms = 10 * SIGNING_HORIZON_LEAD_MS;
        let horizon = DelegationHorizon::expiring_at(expires_at_ms);
        let h = horizon.horizon_ms();
        assert_eq!(h, signing_horizon_ms(expires_at_ms));
        let rank = |state| match state {
            SigningState::Signing { .. } => 0,
            SigningState::Expiring { .. } => 1,
            SigningState::Mute => 2,
        };
        let lead_start = h - SIGNING_HORIZON_LEAD_MS;
        let instants = [
            0,
            lead_start - 1,
            lead_start,
            lead_start + 1,
            h - 1,
            h,
            h + 1,
        ];
        let states = instants.map(|now| horizon.state_at(now));
        assert_eq!(states.map(rank), [0, 0, 1, 1, 1, 2, 2]);
        assert_eq!(states[4], SigningState::Expiring { remaining_ms: 1 });
        for (now, state) in instants.into_iter().zip(states) {
            assert_eq!(state == SigningState::Mute, !horizon.signs_at(now));
        }
    }

    /// A process starts only while it signs: one instant before the horizon it starts, and at
    /// the horizon, which a delegation with exactly `Δ` left reaches at once, it is refused
    /// with the horizon named.
    #[test]
    fn test_a_process_starts_only_before_its_horizon() {
        let horizon = DelegationHorizon::expiring_at(10 * SIGNING_HORIZON_LEAD_MS);
        let h = horizon.horizon_ms();
        assert!(horizon.admitted_at(h - 1).is_ok());
        for now_ms in [h, h + 1] {
            assert!(matches!(
                horizon.admitted_at(now_ms),
                Err(Error::DelegationHorizonReached { horizon_ms, .. }) if horizon_ms == h
            ));
        }
    }
}
