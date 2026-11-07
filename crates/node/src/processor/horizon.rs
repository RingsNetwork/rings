//! The signing horizon of a node's own delegation: until when the process can send anything
//! (#1063, #1153).
//!
//! A node signs every frame it sends with its own delegation, whose expiry is `e`. From
//! `h = e − Δ` on, the sender refuses every frame that carries it ([`signing_horizon_ms`]), so
//! without a renewal the node is mute from `h` on:
//!
//! ```text
//!   Signing ──(t ≥ h − lead)──▶ Expiring ──(t ≥ h)──▶ Mute      (Mute is absorbing)
//!                                  │
//!                        renew(h' > h)
//!                                  ▼
//!                               Signing
//! ```
//!
//! Laws (#1153):
//! - **Monotone horizon**: After a successful renewal, `h' > h`. A renewal never shortens the
//!   horizon.
//! - **No unsigned gap**: At every instant in `[renewal, e)`, the node holds some key whose
//!   delegation verifies, and retains the previous key until `e` so envelopes sealed to it before
//!   peers observe the republished descriptors still open.
//! - **Fallback**: No delegator key, or a failed mint, gives the #1063 behaviour (warn at
//!   `h − lead` and exit at `h`).
//!
//! This module classifies instants and guards in-memory key rotation ([`SessionKeyRing`]); the
//! process shell (`native::horizon`) drives renewal and exit.

use std::sync::PoisonError;
use std::sync::RwLock;

use rings_core::delegation::DelegateeKey;
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

    /// The earliest instant at which in-process renewal is due for a delegation stamped at
    /// `created_at_ms`: `max(h − lead, c + ⌊(h − c) / 2⌋)`.
    ///
    /// Law (**Renewal instant**): For every delegation with `c < h`, `renew_at_ms(c) ∈ [c, h)`,
    /// and when `h − c ≥ 2`, `renew_at_ms(c) > c` so minting a replacement with the same TTL at
    /// `t ≥ renew_at_ms(c)` strictly advances the horizon (`h' > h`).
    pub const fn renew_at_ms(self, created_at_ms: u128) -> u128 {
        let lead_start = self.horizon_ms.saturating_sub(SIGNING_HORIZON_LEAD_MS);
        let half_life =
            created_at_ms.saturating_add(self.horizon_ms.saturating_sub(created_at_ms) / 2);
        if lead_start >= half_life {
            lead_start
        } else {
            half_life
        }
    }
}

/// Pure validation of a candidate [`DelegateeKey`] against `current` at `now_ms`.
///
/// Laws:
/// - **Same delegator**: `next.delegator_did() == current.delegator_did()`.
/// - **No unsigned gap**: `next`'s delegation verifies at `now_ms` and `now_ms < h'`.
/// - **Monotone horizon**: `h' > h`. A renewal never shortens the signing horizon.
fn validate_delegation_renewal(
    current: &DelegateeKey,
    next: DelegateeKey,
    now_ms: u128,
) -> Result<(DelegateeKey, DelegationHorizon)> {
    (next.delegator_did() == current.delegator_did())
        .then_some(())
        .ok_or_else(|| {
            Error::VerifyError(
                "renewed delegatee key delegator DID does not match node DID".to_string(),
            )
        })
        .and_then(|()| {
            next.delegation()
                .verify_delegator_authorization_at(now_ms)
                .map_err(Error::CoreError)
        })
        .and_then(|()| {
            let current_horizon = DelegationHorizon::of(&current.delegation());
            let next_horizon = DelegationHorizon::of(&next.delegation());
            next_horizon.admitted_at(now_ms).and_then(|()| {
                (next_horizon.horizon_ms() > current_horizon.horizon_ms())
                    .then_some((next, next_horizon))
                    .ok_or_else(|| current_horizon.reached())
            })
        })
}

/// Mutable state behind [`SessionKeyRing`].
#[derive(Debug)]
struct SessionKeyRingState {
    /// Active key used for all new signatures and descriptor publications.
    current: DelegateeKey,
    /// Previous keys retained until their delegation expiry `e` so mail or onion hops sealed to a
    /// prior key before descriptor republication settles can still be opened.
    retained: Vec<DelegateeKey>,
    /// Successful runtime rotations in this process lifetime.
    rotation_succeeded_total: u64,
    /// Failed runtime rotation attempts in this process lifetime.
    rotation_failed_total: u64,
}

impl SessionKeyRingState {
    /// Commit a validated renewal `next` at `now_ms`, pruning expired retained keys and keeping the
    /// displaced key if still unexpired.
    fn commit_renewal(&mut self, next: DelegateeKey, now_ms: u128) {
        self.retained
            .retain(|key| !key.delegation().is_expired_at(now_ms));
        let previous = std::mem::replace(&mut self.current, next);
        if !previous.delegation().is_expired_at(now_ms)
            && previous.delegatee_public_key() != self.current.delegatee_public_key()
        {
            self.retained.push(previous);
        }
        self.rotation_succeeded_total = self.rotation_succeeded_total.saturating_add(1);
    }
}

/// Shared live session key and retained unexpired previous keys for a [`super::Processor`].
///
/// Enforces the **Monotone horizon** (`h' > h`) and **No unsigned gap** laws across every clone
/// of the processor.
#[derive(Debug)]
pub(crate) struct SessionKeyRing {
    state: RwLock<SessionKeyRingState>,
}

impl SessionKeyRing {
    /// Construct a key ring starting with `current`.
    pub(crate) const fn new(current: DelegateeKey) -> Self {
        Self {
            state: RwLock::new(SessionKeyRingState {
                current,
                retained: Vec::new(),
                rotation_succeeded_total: 0,
                rotation_failed_total: 0,
            }),
        }
    }

    /// Return the active [`DelegateeKey`].
    pub(crate) fn current(&self) -> DelegateeKey {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .current
            .clone()
    }

    /// Return the [`DelegationHorizon`] of the active key.
    pub(crate) fn horizon(&self) -> DelegationHorizon {
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        DelegationHorizon::of(&state.current.delegation())
    }

    /// Return the active key followed by any retained previous keys that have not yet expired at
    /// `now_ms` (`t < e`).
    pub(crate) fn decryption_keys_at(&self, now_ms: u128) -> Vec<DelegateeKey> {
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        std::iter::once(state.current.clone())
            .chain(
                state
                    .retained
                    .iter()
                    .rev()
                    .filter(|key| !key.delegation().is_expired_at(now_ms))
                    .cloned(),
            )
            .collect()
    }

    /// Return `(rotation_succeeded_total, rotation_failed_total)`.
    pub(crate) fn rotation_counts(&self) -> (u64, u64) {
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        (state.rotation_succeeded_total, state.rotation_failed_total)
    }

    /// Run `mint`, validate the resulting [`DelegateeKey`] at `now_ms`, run `persist`, and install
    /// the key while retaining the previous unexpired key; any error from `mint`, law validation,
    /// or `persist` increments `rotation_failed_total` and leaves the active key untouched.
    ///
    /// Laws:
    /// - **Same delegator**: `next.delegator_did() == current.delegator_did()`.
    /// - **No unsigned gap**: `next`'s delegation verifies at `now_ms` and `now_ms < h'`.
    /// - **Monotone horizon**: `h' > h`. A renewal never shortens the signing horizon.
    pub(crate) fn renew_with<M, P>(
        &self,
        now_ms: u128,
        mint: M,
        persist: P,
    ) -> Result<(DelegateeKey, DelegationHorizon)>
    where
        M: FnOnce(&DelegateeKey) -> Result<DelegateeKey>,
        P: FnOnce(&DelegateeKey) -> Result<()>,
    {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        let outcome = mint(&state.current)
            .and_then(|next| validate_delegation_renewal(&state.current, next, now_ms))
            .and_then(|(next, next_horizon)| persist(&next).map(|()| (next, next_horizon)));
        match outcome {
            Ok((next, next_horizon)) => {
                state.commit_renewal(next.clone(), now_ms);
                Ok((next, next_horizon))
            }
            Err(error) => {
                state.rotation_failed_total = state.rotation_failed_total.saturating_add(1);
                Err(error)
            }
        }
    }

    /// Validate and install `next` at `now_ms`, retaining the previous key if it has not yet
    /// expired.
    pub(crate) fn renew_at(&self, next: DelegateeKey, now_ms: u128) -> Result<DelegationHorizon> {
        self.renew_with(now_ms, |_| Ok(next), |_| Ok(()))
            .map(|(_, horizon)| horizon)
    }
}

#[cfg(test)]
mod tests {
    use rings_core::delegation::DelegateeKey;
    use rings_core::ecc::SecretKey;
    use rings_core::swarm::signing_horizon_ms;

    use super::DelegationHorizon;
    use super::SessionKeyRing;
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

    /// Laws (**Monotone horizon** and **No unsigned gap**, #1153): `SessionKeyRing::renew_at`
    /// refuses any key that shortens or stalls `h`, or changes the delegator DID, while a valid
    /// renewal advances `h' > h`, keeps a verifying delegation across `[renewal, e)`, and retains
    /// the previous key for decryption until its expiry `e`.
    #[test]
    fn test_session_key_ring_enforces_monotone_horizon_and_no_unsigned_gap() {
        let delegator = SecretKey::random();
        let other_delegator = SecretKey::random();
        let t0: u128 = 1_800_000_000_000;
        let ttl_ms = u64::try_from(4 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX);
        let initial = match DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, t0) {
            Ok(key) => key,
            Err(error) => panic!("initial key mint must succeed: {error}"),
        };
        let initial_pubkey = initial.delegatee_public_key();
        let initial_expires = initial.delegation().expires_at_ms();
        let ring = SessionKeyRing::new(initial);
        let h0 = ring.horizon().horizon_ms();
        let renew_ms = h0 - SIGNING_HORIZON_LEAD_MS;

        // Same or earlier horizon (`h' <= h`) is rejected and leaves the active key untouched.
        let non_advancing = match DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, t0) {
            Ok(key) => key,
            Err(error) => panic!("non-advancing key mint must succeed: {error}"),
        };
        assert!(matches!(
            ring.renew_at(non_advancing, renew_ms),
            Err(Error::DelegationHorizonReached { horizon_ms, .. }) if horizon_ms == h0
        ));
        assert_eq!(ring.rotation_counts(), (0, 1));
        assert_eq!(ring.current().delegatee_public_key(), initial_pubkey);

        // Wrong delegator DID is rejected.
        let wrong_did_key =
            match DelegateeKey::new_with_seckey_at(&other_delegator, ttl_ms, renew_ms) {
                Ok(key) => key,
                Err(error) => panic!("wrong-did key mint must succeed: {error}"),
            };
        assert!(matches!(
            ring.renew_at(wrong_did_key, renew_ms),
            Err(Error::VerifyError(_))
        ));
        assert_eq!(ring.rotation_counts(), (0, 2));

        // Valid renewal at `h0 - R` advances `h1 > h0` and retains the previous key until `e0`.
        let renewed = match DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, renew_ms) {
            Ok(key) => key,
            Err(error) => panic!("renewed key mint must succeed: {error}"),
        };
        let renewed_pubkey = renewed.delegatee_public_key();
        let h1 = match ring.renew_at(renewed, renew_ms) {
            Ok(horizon) => horizon.horizon_ms(),
            Err(error) => panic!("valid renewal must succeed: {error}"),
        };
        assert!(h1 > h0);
        assert_eq!(ring.rotation_counts(), (1, 2));

        for instant in [renew_ms, h0, initial_expires - 1, initial_expires] {
            assert!(ring
                .current()
                .delegation()
                .verify_delegator_authorization_at(instant)
                .is_ok());
            let keys: Vec<_> = ring
                .decryption_keys_at(instant)
                .into_iter()
                .map(|key| key.delegatee_public_key())
                .collect();
            assert_eq!(keys, vec![renewed_pubkey, initial_pubkey]);
        }

        // Once the previous delegation expires (`t > e0`), only the active key remains.
        let keys_after_expiry: Vec<_> = ring
            .decryption_keys_at(initial_expires + 1)
            .into_iter()
            .map(|key| key.delegatee_public_key())
            .collect();
        assert_eq!(keys_after_expiry, vec![renewed_pubkey]);
    }

    /// Law (**Renewal instant**, #1153): `renew_at_ms(c) = max(h − lead, c + ⌊(h − c) / 2⌋)`,
    /// which lies in `(c, h)` whenever `h − c ≥ 2`.
    #[test]
    fn test_renew_at_ms_respects_lead_and_half_life_law() {
        let c: u128 = 1_800_000_000_000;
        let long_horizon = DelegationHorizon::expiring_at(c + 4 * SIGNING_HORIZON_LEAD_MS);
        assert_eq!(
            long_horizon.renew_at_ms(c),
            long_horizon.horizon_ms() - SIGNING_HORIZON_LEAD_MS
        );

        let short_horizon = DelegationHorizon::expiring_at(c + SIGNING_HORIZON_LEAD_MS);
        let h_short = short_horizon.horizon_ms();
        let r_short = short_horizon.renew_at_ms(c);
        assert_eq!(r_short, c + (h_short - c) / 2);
        assert!(r_short > c && r_short < h_short);
    }
}
