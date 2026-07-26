use std::collections::BTreeMap;

use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;

use super::SwarmConnection;
use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::callback::InnerSwarmCallback;
use crate::utils::get_epoch_ms_i64;

/// Maximum number of peers that may be handshaking before a data channel opens.
pub(crate) const DEFAULT_PENDING_CONNECTION_CAPACITY: usize = 32;

pub(super) const PENDING_CONNECTION_TIMEOUT_MS: i64 = 180_000;

/// Identifies one pending handshake for a peer.
///
/// A peer can have a replacement handshake after a timeout. Callbacks carry
/// this token so a late callback from the replaced connection cannot promote
/// the newer handshake into the active routing set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingConnectionAttempt {
    pub(super) peer: Did,
    pub(super) generation: u64,
}

impl PendingConnectionAttempt {
    pub(crate) fn peer(self) -> Did {
        self.peer
    }
}

#[derive(Debug)]
pub(super) struct PendingPeer {
    generation: u64,
    admitted_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ExpiredPendingPeer {
    pub(super) attempt: PendingConnectionAttempt,
    pub(super) age_ms: i64,
}

/// Bounded, non-routable handshakes owned by the swarm lifecycle.
///
/// The pool deliberately has no DHT reference: a peer is visible to Chord
/// only after its data channel opens and the matching attempt is promoted.
pub(super) struct PendingPeerPool<const MAX_PENDING: usize> {
    next_generation: u64,
    peers: BTreeMap<Did, PendingPeer>,
}

impl<const MAX_PENDING: usize> PendingPeerPool<MAX_PENDING> {
    pub(super) fn new() -> Self {
        Self {
            next_generation: 0,
            peers: BTreeMap::new(),
        }
    }

    pub(super) fn reserve(&mut self, peer: Did, now_ms: i64) -> Result<PendingConnectionAttempt> {
        if self.peers.contains_key(&peer) {
            return Err(Error::AlreadyConnected);
        }
        if self.peers.len() >= MAX_PENDING {
            return Err(Error::PendingConnectionCapacityExceeded {
                capacity: MAX_PENDING,
            });
        }

        self.next_generation = self.next_generation.wrapping_add(1);
        let attempt = PendingConnectionAttempt {
            peer,
            generation: self.next_generation,
        };
        self.peers.insert(peer, PendingPeer {
            generation: attempt.generation,
            admitted_at_ms: now_ms,
        });
        Ok(attempt)
    }

    pub(super) fn contains(&self, peer: Did) -> bool {
        self.peers.contains_key(&peer)
    }

    pub(super) fn remove(&mut self, attempt: PendingConnectionAttempt) -> bool {
        let Some(peer) = self.peers.get(&attempt.peer) else {
            return false;
        };
        if peer.generation != attempt.generation {
            return false;
        }
        self.peers.remove(&attempt.peer);
        true
    }

    pub(super) fn expire(&mut self, now_ms: i64) -> Vec<ExpiredPendingPeer> {
        let expired = self
            .peers
            .iter()
            .filter_map(|(peer, pending)| {
                let age_ms = now_ms.saturating_sub(pending.admitted_at_ms);
                (age_ms >= PENDING_CONNECTION_TIMEOUT_MS).then_some(ExpiredPendingPeer {
                    attempt: PendingConnectionAttempt {
                        peer: *peer,
                        generation: pending.generation,
                    },
                    age_ms,
                })
            })
            .collect::<Vec<_>>();
        for expired in &expired {
            self.peers.remove(&expired.attempt.peer);
        }
        expired
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(super) fn len(&self) -> usize {
        self.peers.len()
    }
}

impl SwarmTransport {
    fn connection_lifecycle(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.connection_lifecycle
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    fn pending_peers(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, PendingPeerPool<DEFAULT_PENDING_CONNECTION_CAPACITY>>>
    {
        self.pending_peers
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    pub(super) fn active_peers(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<Did, u64>>> {
        self.active_peers
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    pub(super) fn get_raw_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.transport
            .connection(&peer.to_string())
            .map(|conn| SwarmConnection {
                peer,
                connection: conn,
            })
            .ok()
    }

    pub(super) fn is_pending_connection(&self, peer: Did) -> Result<bool> {
        Ok(self.pending_peers()?.contains(peer))
    }

    /// Return whether `peer` completed a handshake and still owns a logical slot.
    ///
    /// Unlike [`Self::is_active_connection`], this remains true while a terminal
    /// callback removes the peer, so lifecycle cleanup can evict it from the DHT
    /// even after WebRTC reports `Closed`.
    pub(crate) fn is_admitted_connection(&self, peer: Did) -> bool {
        self.active_peers()
            .map(|active| active.contains_key(&peer))
            .unwrap_or(false)
    }

    /// Return whether `attempt` owns the current active slot for its peer.
    ///
    /// Invariant: a terminal callback may remove an active peer only when its
    /// generation equals the generation admitted by data-channel open.
    pub(crate) fn is_admitted_connection_attempt(&self, attempt: PendingConnectionAttempt) -> bool {
        self.active_peers()
            .map(|active| active.get(&attempt.peer).copied() == Some(attempt.generation))
            .unwrap_or(false)
    }

    /// Return whether `peer` completed its pending handshake and can route traffic.
    ///
    /// Data-channel open is the admission boundary. WebRTC may still report a
    /// transient `Disconnected` state after admission, so only terminal states
    /// make an admitted peer non-routable before its callback removes the slot.
    pub(crate) fn is_active_connection(&self, peer: Did) -> bool {
        self.is_admitted_connection(peer)
            && self.get_raw_connection(peer).is_some_and(|connection| {
                !matches!(
                    connection.webrtc_connection_state(),
                    WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
                )
            })
    }

    pub(super) async fn reserve_pending_connection(
        &self,
        peer: Did,
    ) -> Result<PendingConnectionAttempt> {
        self.expire_pending_connections().await?;
        if peer == self.dht.did {
            return Err(Error::ShouldNotConnectSelf);
        }
        let _lifecycle = self.connection_lifecycle()?;
        // A peer keeps its active slot through transient WebRTC state changes
        // until its terminal callback removes it from the DHT. Do not admit a
        // second pending handshake for that DID during this interval.
        if self.active_peers()?.contains_key(&peer) {
            return Err(Error::AlreadyConnected);
        }
        let attempt = self.pending_peers()?.reserve(peer, get_epoch_ms_i64())?;
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            pending_timeout_ms = PENDING_CONNECTION_TIMEOUT_MS,
            "pending connection reserved"
        );
        Ok(attempt)
    }

    pub(super) fn pending_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        let pending = self.pending_peers()?;
        Ok(pending
            .peers
            .get(&peer)
            .map(|pending| PendingConnectionAttempt {
                peer,
                generation: pending.generation,
            }))
    }

    pub(crate) fn promote_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let _lifecycle = self.connection_lifecycle()?;
        let mut pending = self.pending_peers()?;
        if !pending.remove(attempt) {
            return Ok(false);
        }
        drop(pending);
        self.active_peers()?
            .insert(attempt.peer, attempt.generation);
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "pending connection promoted"
        );
        Ok(true)
    }

    pub(super) fn retire_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let _lifecycle = self.connection_lifecycle()?;
        Ok(self.pending_peers()?.remove(attempt))
    }

    pub(super) fn retire_active_connection(&self, peer: Did) -> Result<bool> {
        let _lifecycle = self.connection_lifecycle()?;
        let removed = self.active_peers()?.remove(&peer).is_some();
        if removed {
            self.remove_peer_liveness(peer)?;
        }
        Ok(removed)
    }

    /// Cancel a current pending handshake and release its non-routable transport object.
    pub(crate) async fn cancel_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        if !self.retire_pending_connection(attempt)? {
            return Ok(false);
        }
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "pending connection cancelled"
        );
        self.transport
            .close_connection(&attempt.peer.to_string())
            .await
            .map_err(Error::Transport)?;
        Ok(true)
    }

    pub(super) async fn abandon_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        operation: &str,
    ) {
        if let Err(error) = self.cancel_pending_connection(attempt).await {
            tracing::warn!(
                "failed to cancel pending connection to {} after {operation}: {error}",
                attempt.peer
            );
        }
    }

    /// Close pending handshakes whose data channel did not open before the deadline.
    ///
    /// These peers have never entered the DHT, so expiry only releases the
    /// transport object; it deliberately performs no topology mutation.
    pub(crate) async fn expire_pending_connections(&self) -> Result<()> {
        let expired = {
            let _lifecycle = self.connection_lifecycle()?;
            self.pending_peers()?.expire(get_epoch_ms_i64())
        };
        for expired in expired {
            let attempt = expired.attempt;
            let state = self
                .get_raw_connection(attempt.peer)
                .map(|conn| conn.webrtc_connection_state());
            tracing::warn!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %attempt.peer,
                generation = attempt.generation,
                age_ms = expired.age_ms,
                timeout_ms = PENDING_CONNECTION_TIMEOUT_MS,
                state = ?state,
                "pending connection timed out before data-channel open"
            );
            self.transport
                .close_connection(&attempt.peer.to_string())
                .await
                .map_err(Error::Transport)?;
        }
        Ok(())
    }

    /// Create a new non-routable transport connection and register its pending attempt.
    pub(super) async fn new_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        callback: InnerSwarmCallback,
    ) -> Result<()> {
        let cid = attempt.peer.to_string();
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "creating pending transport connection"
        );
        if let Err(error) = self
            .transport
            .new_connection(&cid, Box::new(callback))
            .await
        {
            let _ = self.retire_pending_connection(attempt);
            return Err(Error::Transport(error));
        }
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "pending transport connection created"
        );
        Ok(())
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn pending_connection_count(&self) -> Result<usize> {
        Ok(self.pending_peers()?.len())
    }
}

#[cfg(test)]
mod lifecycle_model {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PeerLifecycle {
        Absent,
        Pending(u64),
        Active(u64),
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CallbackGeneration {
        Current,
        Previous,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum LifecycleAction {
        Replacement,
        Open(CallbackGeneration),
        Close(CallbackGeneration),
        Failed(CallbackGeneration),
        Timeout,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct LifecycleModel {
        peer: PeerLifecycle,
        next_generation: u64,
        previous_generation: Option<u64>,
        dht_member: bool,
        transport_slot: bool,
    }

    impl Default for LifecycleModel {
        fn default() -> Self {
            Self {
                peer: PeerLifecycle::Absent,
                next_generation: 0,
                previous_generation: None,
                dht_member: false,
                transport_slot: false,
            }
        }
    }

    impl LifecycleModel {
        fn apply(mut self, action: LifecycleAction) -> Self {
            match action {
                LifecycleAction::Replacement => self.replace_pending(),
                LifecycleAction::Open(callback) => self.open(callback),
                LifecycleAction::Close(callback) | LifecycleAction::Failed(callback) => {
                    self.terminal(callback)
                }
                LifecycleAction::Timeout => self.timeout(),
            }
            self
        }

        fn replace_pending(&mut self) {
            if let PeerLifecycle::Active(_) = self.peer {
                return;
            }
            self.remember_current_generation();
            self.next_generation = self.next_generation.wrapping_add(1);
            self.peer = PeerLifecycle::Pending(self.next_generation);
            self.dht_member = false;
            self.transport_slot = true;
        }

        fn open(&mut self, callback: CallbackGeneration) {
            let Some(callback_generation) = self.callback_generation(callback) else {
                return;
            };
            if self.peer == PeerLifecycle::Pending(callback_generation) {
                self.peer = PeerLifecycle::Active(callback_generation);
                self.dht_member = true;
                self.transport_slot = true;
            }
        }

        fn terminal(&mut self, callback: CallbackGeneration) {
            let Some(callback_generation) = self.callback_generation(callback) else {
                return;
            };
            if self.generation_matches_live_slot(callback_generation) {
                self.previous_generation = Some(callback_generation);
                self.peer = PeerLifecycle::Absent;
                self.dht_member = false;
                self.transport_slot = false;
            }
        }

        fn timeout(&mut self) {
            if let PeerLifecycle::Pending(generation) = self.peer {
                self.previous_generation = Some(generation);
                self.peer = PeerLifecycle::Absent;
                self.dht_member = false;
                self.transport_slot = false;
            }
        }

        fn callback_generation(self, callback: CallbackGeneration) -> Option<u64> {
            match callback {
                CallbackGeneration::Current => self.current_generation(),
                CallbackGeneration::Previous => self.previous_generation,
            }
        }

        fn current_generation(self) -> Option<u64> {
            match self.peer {
                PeerLifecycle::Absent => None,
                PeerLifecycle::Pending(generation) | PeerLifecycle::Active(generation) => {
                    Some(generation)
                }
            }
        }

        fn remember_current_generation(&mut self) {
            if let Some(generation) = self.current_generation() {
                self.previous_generation = Some(generation);
            }
        }

        fn generation_matches_live_slot(self, generation: u64) -> bool {
            matches!(
                self.peer,
                PeerLifecycle::Pending(current) | PeerLifecycle::Active(current) if current == generation
            )
        }

        fn assert_invariants(self) {
            // Invariant: Pending(g) is a physical transport slot only; it is not a Chord member.
            if matches!(self.peer, PeerLifecycle::Pending(_)) {
                assert!(
                    !self.dht_member,
                    "pending peers must never be routable: {self:?}"
                );
            }
            assert_eq!(
                self.dht_member,
                matches!(self.peer, PeerLifecycle::Active(_)),
                "only admitted active peers may appear in DHT membership: {self:?}"
            );
            assert_eq!(
                self.transport_slot,
                matches!(
                    self.peer,
                    PeerLifecycle::Pending(_) | PeerLifecycle::Active(_)
                ),
                "terminal and expiry transitions must remove the transport slot: {self:?}"
            );
        }
    }

    #[test]
    fn pending_admission_model_preserves_generation_and_routing_invariants() {
        const MAX_DEPTH: usize = 4;
        let actions = [
            LifecycleAction::Replacement,
            LifecycleAction::Open(CallbackGeneration::Current),
            LifecycleAction::Open(CallbackGeneration::Previous),
            LifecycleAction::Close(CallbackGeneration::Current),
            LifecycleAction::Close(CallbackGeneration::Previous),
            LifecycleAction::Failed(CallbackGeneration::Current),
            LifecycleAction::Failed(CallbackGeneration::Previous),
            LifecycleAction::Timeout,
        ];

        explore(LifecycleModel::default(), &actions, MAX_DEPTH);
    }

    #[test]
    fn pending_admission_model_matches_wrapping_generation_boundary() {
        let model = LifecycleModel {
            next_generation: u64::MAX,
            ..LifecycleModel::default()
        }
        .apply(LifecycleAction::Replacement);

        assert_eq!(model.peer, PeerLifecycle::Pending(0));
        assert_eq!(model.next_generation, 0);
        model.assert_invariants();
    }

    fn explore(model: LifecycleModel, actions: &[LifecycleAction], remaining_depth: usize) {
        model.assert_invariants();
        if remaining_depth == 0 {
            return;
        }
        for action in actions.iter().copied() {
            explore(model.apply(action), actions, remaining_depth - 1);
        }
    }
}
