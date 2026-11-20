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

/// Pure Kleisli validation of a candidate [`DelegateeKey`] against `current` at `now_ms`.
///
/// Separates all cryptographic and horizon-order invariants from key-file I/O and lock mutation:
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
        .and_then(|()| next.verify_at(now_ms).map_err(Error::CoreError))
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

    /// Monadic `Result` Kleisli pipeline (`mint >=> validate_delegation_renewal >=> persist`) that
    /// installs `next` while retaining the displaced unexpired key; any error from `mint`, pure
    /// law validation, or `persist` short-circuits, increments `rotation_failed_total`, and leaves
    /// the active key and retained list untouched.
    ///
    /// `persist` receives `(&current, &next)` **while the write lock is held**, so callers that
    /// synchronize auxiliary components (such as transport session credentials) always roll back to
    /// the exact committed `current` key even under concurrent renewal races.
    ///
    /// Laws:
    /// - **Same delegator**: `next.delegator_did() == current.delegator_did()`.
    /// - **No unsigned gap**: `next`'s delegation verifies at `now_ms` and `now_ms < h'`.
    /// - **Monotone horizon**: `h' > h`. A renewal never shortens the signing horizon.
    /// - **Transactional failure**: `Err(_)` preserves `state.current` and `state.retained`.
    pub(crate) fn renew_with<M, P>(
        &self,
        now_ms: u128,
        mint: M,
        persist: P,
    ) -> Result<(DelegateeKey, DelegationHorizon)>
    where
        M: FnOnce(&DelegateeKey) -> Result<DelegateeKey>,
        P: FnOnce(&DelegateeKey, &DelegateeKey) -> Result<()>,
    {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        let outcome = mint(&state.current)
            .and_then(|next| validate_delegation_renewal(&state.current, next, now_ms))
            .and_then(|(next, next_horizon)| {
                persist(&state.current, &next).map(|()| (next, next_horizon))
            });
        match outcome {
            Ok((next, next_horizon)) => {
                state
                    .retained
                    .retain(|key| !key.delegation().is_expired_at(now_ms));
                let previous = std::mem::replace(&mut state.current, next.clone());
                if !previous.delegation().is_expired_at(now_ms)
                    && previous.delegatee_public_key() != state.current.delegatee_public_key()
                {
                    state.retained.push(previous);
                }
                state.rotation_succeeded_total = state.rotation_succeeded_total.saturating_add(1);
                Ok((next, next_horizon))
            }
            Err(error) => {
                state.rotation_failed_total = state.rotation_failed_total.saturating_add(1);
                Err(error)
            }
        }
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

    /// Laws (**Monotone horizon** and **No unsigned gap**, #1153): `SessionKeyRing::renew_with`
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
            ring.renew_with(renew_ms, |_| Ok(non_advancing), |_, _| Ok(())),
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
            ring.renew_with(renew_ms, |_| Ok(wrong_did_key), |_, _| Ok(())),
            Err(Error::VerifyError(_))
        ));
        assert_eq!(ring.rotation_counts(), (0, 2));

        // Valid renewal at `h0 - R` advances `h1 > h0` and retains the previous key until `e0`.
        let renewed = match DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, renew_ms) {
            Ok(key) => key,
            Err(error) => panic!("renewed key mint must succeed: {error}"),
        };
        let renewed_pubkey = renewed.delegatee_public_key();
        let h1 = match ring.renew_with(renew_ms, |_| Ok(renewed), |_, _| Ok(())) {
            Ok((_, horizon)) => horizon.horizon_ms(),
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

    /// Four-Component Two-Phase Lockstep Integration Test (Issue #1291 §4):
    /// Exercises the concrete `Processor::renew_delegatee_key_with` two-phase commit pipeline
    /// across all failure stages (`MintErr`, `RingValidateErr`, `SwarmValidateErr`,
    /// `TransportPrepareErr`, `PersistErr`, `CommitOk`) and concurrent renewal contention,
    /// asserting `\square (k_ring == k_disk == k_swarm == k_transport)` without compensating
    /// rollbacks.
    #[test]
    fn test_processor_renew_delegatee_key_preserves_three_component_lockstep_invariant() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;
        use std::sync::Mutex;

        use rings_core::utils::ManualClock;
        use rings_transport::core::transport::SharedTransport;
        use rings_turn::format_turn_server_url;
        use rings_turn::TurnCredentialSigner;

        use crate::processor::NodeNetworkParameters;
        use crate::processor::ProcessorBuilder;
        use crate::processor::ProcessorConfig;
        use crate::processor::SessionRotationCommit;
        use crate::processor::TransportNetworkConfig;

        /// Fault-injectable [`NodeNetworkParameters`] wrapper around [`TransportNetworkConfig`]
        /// that counts phase-1 `prepare_session_key_rotation` calls vs phase-2 commit executions
        /// and allows injecting phase-1 preparation failures.
        #[derive(Clone, Debug)]
        struct FaultInjectableNetworkParams {
            /// Underlying assisted-traversal transport configuration.
            inner: TransportNetworkConfig,
            /// When `true`, `prepare_session_key_rotation` fails before mutating the transport.
            fail_rotation: Arc<AtomicBool>,
            /// Count of phase-1 preparation calls.
            prepare_calls: Arc<AtomicUsize>,
            /// Count of phase-2 commit closure executions.
            commit_calls: Arc<AtomicUsize>,
        }

        impl NodeNetworkParameters for FaultInjectableNetworkParams {
            /// Delegate initial transport construction to the inner [`TransportNetworkConfig`].
            fn build_transport(
                &self,
                delegatee_key: &DelegateeKey,
            ) -> crate::error::Result<SharedTransport> {
                NodeNetworkParameters::build_transport(&self.inner, delegatee_key)
            }

            /// Delegate endpoint hint inspection to the inner [`TransportNetworkConfig`].
            fn endpoint_hint(&self) -> Option<&str> {
                NodeNetworkParameters::endpoint_hint(&self.inner)
            }

            /// Fail in phase 1 when `fail_rotation` is armed; otherwise prepare the inner
            /// [`TransportNetworkConfig`] rotation without mutating `transport`.
            fn prepare_session_key_rotation(
                &self,
                transport: &SharedTransport,
                next_key: &DelegateeKey,
            ) -> crate::error::Result<SessionRotationCommit> {
                self.prepare_calls.fetch_add(1, Ordering::SeqCst);
                if self.fail_rotation.load(Ordering::SeqCst) {
                    return Err(Error::InvalidConfig(
                        "injected transport session-key rotation prepare failure".to_string(),
                    ));
                }
                let inner_commit = NodeNetworkParameters::prepare_session_key_rotation(
                    &self.inner,
                    transport,
                    next_key,
                )?;
                let commits = Arc::clone(&self.commit_calls);
                Ok(Box::new(move || {
                    commits.fetch_add(1, Ordering::SeqCst);
                    inner_commit();
                }))
            }
        }

        let delegator = SecretKey::random();
        let other_delegator = SecretKey::random();
        let turn_server_pubkey = SecretKey::random().pubkey();
        let turn_url =
            format_turn_server_url("turn://turn.rings.rs:3478", turn_server_pubkey).unwrap();
        let raw_ice = format!("stun://stun.l.google.com:19302;{turn_url}");

        let t0: u128 = 1_800_000_000_000;
        let ttl_ms = u64::try_from(4 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX);
        let k0 = DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, t0).unwrap();
        let clock = Arc::new(ManualClock::new(t0));
        let fail_rotation = Arc::new(AtomicBool::new(false));
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let commit_calls = Arc::new(AtomicUsize::new(0));
        let persisted_disk_key = Arc::new(Mutex::new(k0.clone()));

        let config = ProcessorConfig::new(1, raw_ice.clone(), k0.clone(), 15)
            .with_clock(clock)
            .with_network_parameters(FaultInjectableNetworkParams {
                inner: TransportNetworkConfig::new(raw_ice.clone(), None, None),
                fail_rotation: Arc::clone(&fail_rotation),
                prepare_calls: Arc::clone(&prepare_calls),
                commit_calls: Arc::clone(&commit_calls),
            });
        let processor = ProcessorBuilder::from_config(&config)
            .unwrap()
            .build_at(t0)
            .unwrap();

        let assert_lockstep = |expected_key: &DelegateeKey| {
            let k_ring = processor.delegatee_key();
            let k_disk = persisted_disk_key.lock().unwrap().clone();
            let k_swarm = processor.swarm.delegatee_key();
            let k_transport = processor
                .session_transport_credentials()
                .expect("transport must report active session credentials");
            let expected_credentials = expected_key.sign_ice_servers(&raw_ice).unwrap();

            assert_eq!(
                k_ring.delegatee_public_key(),
                expected_key.delegatee_public_key(),
                "k_ring diverged from expected key"
            );
            assert_eq!(
                k_disk.delegatee_public_key(),
                expected_key.delegatee_public_key(),
                "k_disk diverged from k_ring"
            );
            assert_eq!(
                k_swarm.delegatee_public_key(),
                expected_key.delegatee_public_key(),
                "k_swarm diverged from k_ring"
            );
            assert_eq!(
                k_transport, expected_credentials,
                "k_transport diverged from k_ring session credentials"
            );
        };

        assert_lockstep(&k0);

        let h0 = processor.signing_horizon().horizon_ms();
        let renew_ms = h0 - SIGNING_HORIZON_LEAD_MS;
        let k1 = DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, renew_ms).unwrap();

        // 1. MintErr: failure at Stage 1 preserves (k0, k0, k0, k0).
        let mint_err = processor.renew_delegatee_key_with(
            renew_ms,
            |_| Err(Error::VerifyError("injected mint failure".to_string())),
            |next| {
                *persisted_disk_key.lock().unwrap() = next.clone();
                Ok(())
            },
        );
        assert!(mint_err.is_err());
        assert_lockstep(&k0);

        // 2. RingValidateErr (non-advancing horizon): failure at Stage 2 preserves (k0, k0, k0, k0).
        let non_advancing = DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, t0).unwrap();
        let val_err_horizon = processor.renew_delegatee_key_with(
            renew_ms,
            |_| Ok(non_advancing),
            |next| {
                *persisted_disk_key.lock().unwrap() = next.clone();
                Ok(())
            },
        );
        assert!(val_err_horizon.is_err());
        assert_lockstep(&k0);

        // 3. RingValidateErr (wrong delegator DID): failure at Stage 2 preserves (k0, k0, k0, k0).
        let wrong_did =
            DelegateeKey::new_with_seckey_at(&other_delegator, ttl_ms, renew_ms).unwrap();
        let val_err_did = processor.renew_delegatee_key_with(
            renew_ms,
            |_| Ok(wrong_did),
            |next| {
                *persisted_disk_key.lock().unwrap() = next.clone();
                Ok(())
            },
        );
        assert!(val_err_did.is_err());
        assert_lockstep(&k0);

        // 4. TransportPrepareErr: failure in phase 1 (`prepare_session_key_rotation`) aborts
        //    before touching disk, swarm, or key ring, with zero phase-2 commits.
        fail_rotation.store(true, Ordering::SeqCst);
        let rot_err = processor.renew_delegatee_key_with(
            renew_ms,
            |_| Ok(k1.clone()),
            |next| {
                *persisted_disk_key.lock().unwrap() = next.clone();
                Ok(())
            },
        );
        fail_rotation.store(false, Ordering::SeqCst);
        assert!(rot_err.is_err());
        assert_eq!(commit_calls.load(Ordering::SeqCst), 0);
        assert_lockstep(&k0);

        // 5. PersistErr: failure at the single commit point (`persist`) aborts BEFORE any
        //    in-memory state (`swarm`, `transport`, `ring`) is mutated — `commit_calls` stays 0!
        let persist_err = processor.renew_delegatee_key_with(
            renew_ms,
            |_| Ok(k1.clone()),
            |_| Err(Error::CreateFileError("injected disk failure".to_string())),
        );
        assert!(persist_err.is_err());
        assert_eq!(
            commit_calls.load(Ordering::SeqCst),
            0,
            "PersistErr must abort before executing phase-2 transport commit"
        );
        assert_lockstep(&k0);

        // 6. CommitOk: atomic transition advances all four components to (k1, k1, k1, k1).
        let h1 = processor
            .renew_delegatee_key_with(
                renew_ms,
                |_| Ok(k1.clone()),
                |next| {
                    *persisted_disk_key.lock().unwrap() = next.clone();
                    Ok(())
                },
            )
            .expect("valid renewal must commit");
        assert!(h1.horizon_ms() > h0);
        assert_eq!(commit_calls.load(Ordering::SeqCst), 1);
        assert_lockstep(&k1);

        // 7. Subsequent TransportPrepareErr from committed state `k1` preserves (k1, k1, k1, k1).
        let renew_ms_2 = h1.horizon_ms() - SIGNING_HORIZON_LEAD_MS;
        let k2 = DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, renew_ms_2).unwrap();
        fail_rotation.store(true, Ordering::SeqCst);
        let rot_err_2 = processor.renew_delegatee_key_with(
            renew_ms_2,
            |_| Ok(k2.clone()),
            |next| {
                *persisted_disk_key.lock().unwrap() = next.clone();
                Ok(())
            },
        );
        fail_rotation.store(false, Ordering::SeqCst);
        assert!(rot_err_2.is_err());
        assert_lockstep(&k1);

        // 8. SwarmValidateErr: on an isolated processor whose swarm horizon was synthetically
        //    advanced ahead of its ring (`k2` vs `k0`), attempting to renew the ring to `k1`
        //    fails at `Swarm::validate_next_delegatee_key` BEFORE `prepare_session_key_rotation`
        //    or `persist(next)` ever runs.
        {
            let skew_prepares = Arc::new(AtomicUsize::new(0));
            let skew_commits = Arc::new(AtomicUsize::new(0));
            let skew_disk = Arc::new(Mutex::new(k0.clone()));
            let skew_cfg = ProcessorConfig::new(1, raw_ice.clone(), k0.clone(), 15)
                .with_network_parameters(FaultInjectableNetworkParams {
                    inner: TransportNetworkConfig::new(raw_ice.clone(), None, None),
                    fail_rotation: Arc::new(AtomicBool::new(false)),
                    prepare_calls: Arc::clone(&skew_prepares),
                    commit_calls: Arc::clone(&skew_commits),
                });
            let skew_proc = ProcessorBuilder::from_config(&skew_cfg)
                .unwrap()
                .build_at(t0)
                .unwrap();
            skew_proc.swarm.commit_delegatee_key_unchecked(k2.clone());
            let swarm_val_err = skew_proc.renew_delegatee_key_with(
                renew_ms,
                |_| Ok(k1.clone()),
                |next| {
                    *skew_disk.lock().unwrap() = next.clone();
                    Ok(())
                },
            );
            assert!(
                swarm_val_err.is_err(),
                "SwarmValidateErr must reject renewal when swarm rejects candidate key"
            );
            assert_eq!(
                skew_prepares.load(Ordering::SeqCst),
                0,
                "SwarmValidateErr must short-circuit before transport prepare"
            );
            assert_eq!(skew_commits.load(Ordering::SeqCst), 0);
            assert_eq!(
                skew_disk.lock().unwrap().delegatee_public_key(),
                k0.delegatee_public_key(),
                "SwarmValidateErr must never touch disk"
            );
            assert_eq!(
                skew_proc.delegatee_key().delegatee_public_key(),
                k0.delegatee_public_key(),
                "SwarmValidateErr must leave key ring untouched"
            );
        }

        // 9. Concurrent renewal race (native multi-threaded): Thread A blocks waiting for the
        //    key-ring write lock while Thread B commits `k1 -> k2`. Thread A then attempts
        //    `k2 -> k3` whose `persist` fails; because phase 1 never mutated `transport` or
        //    `swarm`, `(k2, k2, k2, k2)` stays intact without any rollback race.
        #[cfg(not(target_family = "wasm"))]
        {
            let renew_ms_3 =
                DelegationHorizon::of(&k2.delegation()).horizon_ms() - SIGNING_HORIZON_LEAD_MS;
            let k3 = DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, renew_ms_3).unwrap();
            let entered_b = Arc::new(std::sync::Barrier::new(2));
            let release_b = Arc::new(std::sync::Barrier::new(2));

            let proc_b = processor.clone();
            let disk_b = Arc::clone(&persisted_disk_key);
            let k2_for_b = k2.clone();
            let entered_b_tx = Arc::clone(&entered_b);
            let release_b_rx = Arc::clone(&release_b);
            let handle_b = std::thread::spawn(move || {
                proc_b.renew_delegatee_key_with(
                    renew_ms_2,
                    |_| {
                        entered_b_tx.wait();
                        release_b_rx.wait();
                        Ok(k2_for_b)
                    },
                    |next| {
                        *disk_b.lock().unwrap() = next.clone();
                        Ok(())
                    },
                )
            });

            entered_b.wait();
            let proc_a = processor.clone();
            let handle_a = std::thread::spawn(move || {
                proc_a.renew_delegatee_key_with(
                    renew_ms_3,
                    |_| Ok(k3),
                    |_| {
                        Err(Error::CreateFileError(
                            "concurrent persist fail".to_string(),
                        ))
                    },
                )
            });

            std::thread::sleep(std::time::Duration::from_millis(20));
            release_b.wait();

            assert!(handle_b.join().unwrap().is_ok());
            assert!(handle_a.join().unwrap().is_err());
            assert_lockstep(&k2);
        }
    }

    /// Exhaustive Stateright Model Check & Mutant Refutation for Two-Phase Session Rotation
    /// (Issue #1291 F4 + Impl #1):
    ///
    /// Explores every reachable state `(k_ring, k_disk, k_swarm, k_transport, step)` across
    /// arbitrary sequences of `{MintErr, RingValidateErr, SwarmValidateErr, TransportPrepareErr,
    /// PersistErr, PersistErrWithRollbackFault, CommitOk}` over up to 3 sequential rotations:
    /// 1. Proves `ProtocolVariant::TwoPhase` satisfies `\square (k_ring == k_disk == k_swarm ==
    ///    k_transport)` across 100% of reachable states.
    /// 2. Refutes `ProtocolVariant::MutantPersistBeforeSwarmValidate` (proving that running
    ///    `persist` before `Swarm::validate_next_delegatee_key` breaks `k_disk == k_swarm`).
    /// 3. Refutes `ProtocolVariant::MutantMutateTransportWithFallibleRollback` (proving that
    ///    mutating `transport` before `persist` with best-effort `let _ = rollback` breaks
    ///    `k_transport == k_ring` when rollback fails).
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn test_stateright_two_phase_rotation_lockstep_and_mutant_refutation() {
        use stateright::Checker;
        use stateright::Model;
        use stateright::Property;

        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        enum ProtocolVariant {
            TwoPhase,
            MutantPersistBeforeSwarmValidate,
            MutantMutateTransportWithFallibleRollback,
        }

        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        struct RotationState {
            k_ring: u8,
            k_disk: u8,
            k_swarm: u8,
            k_transport: u8,
            depth: u8,
        }

        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        enum RotationAction {
            MintErr,
            RingValidateErr,
            SwarmValidateErr,
            TransportPrepareErr,
            PersistErr,
            PersistErrWithRollbackFault,
            CommitOk,
        }

        struct RotationModel {
            variant: ProtocolVariant,
            max_depth: u8,
        }

        impl Model for RotationModel {
            type State = RotationState;
            type Action = RotationAction;

            fn init_states(&self) -> Vec<Self::State> {
                vec![RotationState {
                    k_ring: 0,
                    k_disk: 0,
                    k_swarm: 0,
                    k_transport: 0,
                    depth: 0,
                }]
            }

            fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
                if state.depth >= self.max_depth {
                    return;
                }
                actions.extend([
                    RotationAction::MintErr,
                    RotationAction::RingValidateErr,
                    RotationAction::SwarmValidateErr,
                    RotationAction::TransportPrepareErr,
                    RotationAction::PersistErr,
                    RotationAction::PersistErrWithRollbackFault,
                    RotationAction::CommitOk,
                ]);
            }

            fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
                let mut next = *last;
                next.depth = last.depth + 1;
                let candidate = last.k_ring + 1;

                match self.variant {
                    ProtocolVariant::TwoPhase => {
                        // Phase 1 (pure): mint -> ring_validate -> swarm_validate -> transport_prepare
                        // Single commit point: persist(candidate)
                        // Phase 2 (infallible): swarm_commit -> transport_commit -> ring_commit
                        match action {
                            RotationAction::MintErr
                            | RotationAction::RingValidateErr
                            | RotationAction::SwarmValidateErr
                            | RotationAction::TransportPrepareErr
                            | RotationAction::PersistErr
                            | RotationAction::PersistErrWithRollbackFault => Some(next),
                            RotationAction::CommitOk => {
                                next.k_disk = candidate;
                                next.k_swarm = candidate;
                                next.k_transport = candidate;
                                next.k_ring = candidate;
                                Some(next)
                            }
                        }
                    }
                    ProtocolVariant::MutantPersistBeforeSwarmValidate => match action {
                        RotationAction::MintErr
                        | RotationAction::RingValidateErr
                        | RotationAction::TransportPrepareErr
                        | RotationAction::PersistErr
                        | RotationAction::PersistErrWithRollbackFault => Some(next),
                        RotationAction::SwarmValidateErr => {
                            // Bug: persisted to disk BEFORE swarm validation rejected the key!
                            next.k_disk = candidate;
                            Some(next)
                        }
                        RotationAction::CommitOk => {
                            next.k_disk = candidate;
                            next.k_swarm = candidate;
                            next.k_transport = candidate;
                            next.k_ring = candidate;
                            Some(next)
                        }
                    },
                    ProtocolVariant::MutantMutateTransportWithFallibleRollback => match action {
                        RotationAction::MintErr
                        | RotationAction::RingValidateErr
                        | RotationAction::SwarmValidateErr
                        | RotationAction::TransportPrepareErr => Some(next),
                        RotationAction::PersistErr => {
                            // Mutated transport, then rolled back successfully.
                            next.k_transport = last.k_ring;
                            Some(next)
                        }
                        RotationAction::PersistErrWithRollbackFault => {
                            // Bug: mutated transport before persist, and `let _ = rollback` failed!
                            next.k_transport = candidate;
                            Some(next)
                        }
                        RotationAction::CommitOk => {
                            next.k_disk = candidate;
                            next.k_swarm = candidate;
                            next.k_transport = candidate;
                            next.k_ring = candidate;
                            Some(next)
                        }
                    },
                }
            }

            fn properties(&self) -> Vec<Property<Self>> {
                vec![Property::<Self>::always(
                    "four-component lockstep invariant",
                    |_, s| {
                        s.k_ring == s.k_disk && s.k_ring == s.k_swarm && s.k_ring == s.k_transport
                    },
                )]
            }
        }

        // 1. Two-phase protocol satisfies lockstep across every reachable interleaving.
        let checker_ok = RotationModel {
            variant: ProtocolVariant::TwoPhase,
            max_depth: 3,
        }
        .checker()
        .spawn_bfs()
        .join();
        checker_ok.assert_properties();
        assert!(
            checker_ok.unique_state_count() > 1,
            "BFS must explore multi-epoch transitions"
        );

        // 2. Mutant A (`persist` before `Swarm::validate_next_delegatee_key`) is refuted.
        let checker_mutant_a = RotationModel {
            variant: ProtocolVariant::MutantPersistBeforeSwarmValidate,
            max_depth: 2,
        }
        .checker()
        .spawn_bfs()
        .join();
        assert!(
            checker_mutant_a
                .discovery("four-component lockstep invariant")
                .is_some(),
            "Stateright must refute persist-before-swarm-validate mutant"
        );

        // 3. Mutant B (`mutate transport before persist` with fallible rollback) is refuted.
        let checker_mutant_b = RotationModel {
            variant: ProtocolVariant::MutantMutateTransportWithFallibleRollback,
            max_depth: 2,
        }
        .checker()
        .spawn_bfs()
        .join();
        assert!(
            checker_mutant_b
                .discovery("four-component lockstep invariant")
                .is_some(),
            "Stateright must refute fallible-rollback mutant"
        );
    }
}
