use std::collections::BTreeMap;
use std::sync::MutexGuard;

use super::pending::ActiveConnectionSet;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::ProbeRequestV1;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms_i64;

/// Idle interval after which an admitted peer needs an overlay liveness probe.
pub(crate) const PEER_LIVENESS_IDLE_MS: i64 = 15_000;
/// Maximum age of an unanswered liveness probe before the peer is evicted.
pub(crate) const PEER_LIVENESS_TIMEOUT_MS: i64 = 45_000;

#[derive(Clone, Copy, Debug)]
struct PeerLiveness {
    generation: u64,
    connected_at_ms: i64,
    last_inbound_ms: i64,
    last_probe_ms: Option<i64>,
    unanswered_probe_since_ms: Option<i64>,
    pending_probe: Option<PendingProbe>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingProbe {
    tx_id: uuid::Uuid,
    request: ProbeRequestV1,
}

#[derive(Clone, Copy)]
enum PeerLivenessObservation {
    Connected,
    Inbound,
}

impl PeerLivenessObservation {
    fn apply(self, liveness: &mut PeerLivenessMap, peer: Did, generation: u64, now_ms: i64) {
        match self {
            Self::Connected => liveness.mark_connected(peer, generation, now_ms),
            Self::Inbound => liveness.mark_inbound(peer, generation, now_ms),
        }
    }
}

impl PeerLiveness {
    fn new(generation: u64, now_ms: i64) -> Self {
        Self {
            generation,
            connected_at_ms: now_ms,
            last_inbound_ms: now_ms,
            last_probe_ms: None,
            unanswered_probe_since_ms: None,
            pending_probe: None,
        }
    }

    fn mark_inbound(&mut self, now_ms: i64) {
        self.last_inbound_ms = now_ms;
        self.unanswered_probe_since_ms = None;
    }

    fn should_probe(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.last_inbound_ms) >= PEER_LIVENESS_IDLE_MS
            && self
                .last_probe_ms
                .map(|last_probe_ms| now_ms.saturating_sub(last_probe_ms) >= PEER_LIVENESS_IDLE_MS)
                .unwrap_or(true)
    }

    fn mark_probe_sent(&mut self, now_ms: i64) {
        self.last_probe_ms = Some(now_ms);
        self.unanswered_probe_since_ms.get_or_insert(now_ms);
    }

    fn set_pending_probe(&mut self, tx_id: uuid::Uuid, request: ProbeRequestV1) {
        self.pending_probe = Some(PendingProbe { tx_id, request });
    }

    fn mark_matching_probe_sent(
        &mut self,
        now_ms: i64,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) -> bool {
        if self.pending_probe != Some(PendingProbe { tx_id, request }) {
            return false;
        }
        self.mark_probe_sent(now_ms);
        true
    }

    fn cancel_pending_probe(&mut self, tx_id: uuid::Uuid, request: ProbeRequestV1) {
        if self.pending_probe == Some(PendingProbe { tx_id, request }) {
            self.pending_probe = None;
        }
    }

    fn consume_pending_probe(&mut self, tx_id: uuid::Uuid, request: ProbeRequestV1) -> bool {
        if self.pending_probe != Some(PendingProbe { tx_id, request }) {
            return false;
        }
        self.pending_probe = None;
        true
    }

    fn expiry(&self, now_ms: i64) -> Option<PeerLivenessExpiry> {
        let unanswered_since_ms = self.unanswered_probe_since_ms?;
        let unanswered_for_ms = now_ms.saturating_sub(unanswered_since_ms);
        (unanswered_for_ms >= PEER_LIVENESS_TIMEOUT_MS).then_some(PeerLivenessExpiry {
            unanswered_for_ms,
            timeout_ms: PEER_LIVENESS_TIMEOUT_MS,
        })
    }

    fn connected_for_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.connected_at_ms)
    }

    fn idle_for_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.last_inbound_ms)
    }
}

/// Bounded-by-active-peers overlay liveness state.
///
/// Invariant: for every `(did, state)` in this map, `state.generation` is the
/// active transport generation that admitted `did`. A new connection generation
/// resets liveness, so late observations from an old connection cannot prove the
/// new connection live.
pub(super) struct PeerLivenessMap {
    peers: BTreeMap<Did, PeerLiveness>,
}

impl PeerLivenessMap {
    pub(super) fn new() -> Self {
        Self {
            peers: BTreeMap::new(),
        }
    }

    fn mark_connected(&mut self, peer: Did, generation: u64, now_ms: i64) {
        self.peers
            .insert(peer, PeerLiveness::new(generation, now_ms));
    }

    fn mark_inbound(&mut self, peer: Did, generation: u64, now_ms: i64) {
        match self.peers.get_mut(&peer) {
            Some(liveness) if liveness.generation == generation => {
                liveness.mark_inbound(now_ms);
            }
            _ => self.mark_connected(peer, generation, now_ms),
        }
    }

    pub(super) fn remove(&mut self, peer: Did) {
        self.peers.remove(&peer);
    }

    fn retain_active(&mut self, active: &ActiveConnectionSet) {
        self.peers.retain(|peer, liveness| {
            active
                .attempt(*peer)
                .is_some_and(|attempt| attempt.generation == liveness.generation)
        });
    }

    fn probe_candidates(
        &mut self,
        active: &ActiveConnectionSet,
        now_ms: i64,
    ) -> Vec<PendingConnectionAttempt> {
        self.retain_active(active);
        for attempt in active.iter() {
            self.peers
                .entry(attempt.peer)
                .or_insert_with(|| PeerLiveness::new(attempt.generation, now_ms));
        }

        active
            .iter()
            .filter(|attempt| {
                self.peers
                    .get(&attempt.peer)
                    .is_some_and(|liveness| liveness.should_probe(now_ms))
            })
            .collect()
    }

    fn register_probe(
        &mut self,
        peer: Did,
        generation: u64,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) {
        match self.peers.get_mut(&peer) {
            Some(liveness) if liveness.generation == generation => {
                liveness.set_pending_probe(tx_id, request);
            }
            _ => {
                // The active generation is registered before probe candidates are
                // returned, so a missing/mismatched entry is a superseded attempt.
            }
        }
    }

    fn mark_probe_sent(
        &mut self,
        peer: Did,
        generation: u64,
        now_ms: i64,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) -> bool {
        self.peers
            .get_mut(&peer)
            .filter(|liveness| liveness.generation == generation)
            .is_some_and(|liveness| liveness.mark_matching_probe_sent(now_ms, tx_id, request))
    }

    fn cancel_pending_probe(
        &mut self,
        peer: Did,
        generation: u64,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) {
        if let Some(liveness) = self
            .peers
            .get_mut(&peer)
            .filter(|liveness| liveness.generation == generation)
        {
            liveness.cancel_pending_probe(tx_id, request);
        }
    }

    fn consume_pending_probe(
        &mut self,
        peer: Did,
        generation: u64,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) -> bool {
        self.peers
            .get_mut(&peer)
            .filter(|liveness| liveness.generation == generation)
            .is_some_and(|liveness| liveness.consume_pending_probe(tx_id, request))
    }

    fn expiry(&self, peer: Did, generation: u64, now_ms: i64) -> Option<PeerLivenessExpiry> {
        let liveness = self.peers.get(&peer)?;
        (liveness.generation == generation)
            .then(|| liveness.expiry(now_ms))
            .flatten()
    }

    pub(super) fn connected_for_ms(&self, peer: Did, generation: u64, now_ms: i64) -> Option<i64> {
        let liveness = self.peers.get(&peer)?;
        (liveness.generation == generation).then(|| liveness.connected_for_ms(now_ms))
    }

    /// Time since the last authenticated inbound payload on this generation.
    pub(super) fn idle_for_ms(&self, peer: Did, generation: u64, now_ms: i64) -> Option<i64> {
        let liveness = self.peers.get(&peer)?;
        (liveness.generation == generation).then(|| liveness.idle_for_ms(now_ms))
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn unanswered_probe_since_ms(&self, peer: Did, generation: u64) -> Option<i64> {
        let liveness = self.peers.get(&peer)?;
        (liveness.generation == generation)
            .then_some(liveness.unanswered_probe_since_ms)
            .flatten()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn force_probe_sent_at(&mut self, peer: Did, generation: u64, sent_at_ms: i64) {
        let mut liveness = PeerLiveness::new(generation, sent_at_ms);
        liveness.mark_probe_sent(sent_at_ms);
        self.peers.insert(peer, liveness);
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn force_last_inbound_at(&mut self, peer: Did, generation: u64, last_inbound_ms: i64) -> bool {
        let Some(liveness) = self
            .peers
            .get_mut(&peer)
            .filter(|liveness| liveness.generation == generation)
        else {
            return false;
        };
        liveness.last_inbound_ms = last_inbound_ms;
        true
    }

    #[cfg(test)]
    fn force_connected_at(&mut self, peer: Did, generation: u64, connected_at_ms: i64) -> bool {
        let Some(liveness) = self
            .peers
            .get_mut(&peer)
            .filter(|liveness| liveness.generation == generation)
        else {
            return false;
        };
        liveness.connected_at_ms = connected_at_ms;
        true
    }
}

/// Peer liveness expiry evidence.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerLivenessExpiry {
    /// How long the probe has been unanswered.
    pub(crate) unanswered_for_ms: i64,
    /// Configured timeout for unanswered probes.
    pub(crate) timeout_ms: i64,
}

impl SwarmTransport {
    pub(super) fn peer_liveness(&self) -> Result<MutexGuard<'_, PeerLivenessMap>> {
        self.peer_liveness
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    fn observe_peer_liveness(
        &self,
        attempt: PendingConnectionAttempt,
        observation: PeerLivenessObservation,
        before_update: impl FnOnce(),
    ) -> Result<()> {
        let now_ms = get_epoch_ms_i64();
        self.with_active_slot(attempt, || {
            before_update();
            let mut liveness = self.peer_liveness()?;
            observation.apply(&mut liveness, attempt.peer, attempt.generation, now_ms);
            Ok(())
        })
        .map(|_| ())
    }

    pub(crate) fn mark_peer_liveness_connected(&self, attempt: PendingConnectionAttempt) {
        if let Err(error) =
            self.observe_peer_liveness(attempt, PeerLivenessObservation::Connected, || {})
        {
            tracing::warn!(
                "failed to mark liveness for connected peer {} generation {}: {error}",
                attempt.peer,
                attempt.generation
            );
        }
    }

    pub(crate) fn mark_peer_liveness_inbound(&self, attempt: PendingConnectionAttempt) {
        if let Err(error) =
            self.observe_peer_liveness(attempt, PeerLivenessObservation::Inbound, || {})
        {
            tracing::warn!(
                "failed to mark liveness for inbound peer {} generation {}: {error}",
                attempt.peer,
                attempt.generation
            );
        }
    }

    pub(crate) fn liveness_probe_candidates(
        &self,
        now_ms: i64,
    ) -> Result<Vec<PendingConnectionAttempt>> {
        self.with_connection_lifecycle(|| {
            let active = self.active_connections()?;
            Ok(self.peer_liveness()?.probe_candidates(&active, now_ms))
        })
    }

    pub(crate) fn register_pending_liveness_probe(
        &self,
        attempt: PendingConnectionAttempt,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) -> Result<bool> {
        self.with_active_slot(attempt, || {
            self.peer_liveness()?
                .register_probe(attempt.peer, attempt.generation, tx_id, request);
            Ok(())
        })
        .map(|registered| registered.is_some())
    }

    pub(crate) fn record_peer_liveness_probe_sent(
        &self,
        attempt: PendingConnectionAttempt,
        now_ms: i64,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) -> Result<bool> {
        self.with_active_slot(attempt, || {
            Ok(self.peer_liveness()?.mark_probe_sent(
                attempt.peer,
                attempt.generation,
                now_ms,
                tx_id,
                request,
            ))
        })
        .map(|recorded| recorded.unwrap_or(false))
    }

    pub(crate) fn cancel_pending_liveness_probe(
        &self,
        attempt: PendingConnectionAttempt,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) -> Result<()> {
        self.with_active_slot(attempt, || {
            self.peer_liveness()?.cancel_pending_probe(
                attempt.peer,
                attempt.generation,
                tx_id,
                request,
            );
            Ok(())
        })
        .map(|_| ())
    }

    pub(crate) fn consume_pending_probe(
        &self,
        provider: Did,
        tx_id: uuid::Uuid,
        request: ProbeRequestV1,
    ) -> Result<bool> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(provider)? else {
                return Ok(false);
            };
            Ok(self.peer_liveness()?.consume_pending_probe(
                provider,
                attempt.generation,
                tx_id,
                request,
            ))
        })
    }

    pub(crate) fn peer_liveness_expiry(
        &self,
        attempt: PendingConnectionAttempt,
        now_ms: i64,
    ) -> Result<Option<PeerLivenessExpiry>> {
        self.with_active_slot(attempt, || {
            Ok(self
                .peer_liveness()?
                .expiry(attempt.peer, attempt.generation, now_ms))
        })
        .map(Option::flatten)
    }

    /// Return how long an admitted peer has owned its current active generation.
    pub(crate) fn peer_connected_for_ms(&self, peer: Did, now_ms: i64) -> Result<Option<i64>> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Ok(None);
            };
            Ok(self
                .peer_liveness()?
                .connected_for_ms(peer, attempt.generation, now_ms))
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn mark_peer_liveness_connected_with_observer_for_test(
        &self,
        attempt: PendingConnectionAttempt,
        before_update: impl FnOnce(),
    ) -> Result<()> {
        self.observe_peer_liveness(attempt, PeerLivenessObservation::Connected, before_update)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn peer_liveness_count_for_test(&self) -> Result<usize> {
        self.with_connection_lifecycle(|| Ok(self.peer_liveness()?.peers.len()))
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn peer_liveness_unanswered_since_for_test(&self, peer: Did) -> Result<Option<i64>> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Ok(None);
            };
            Ok(self
                .peer_liveness()?
                .unanswered_probe_since_ms(peer, attempt.generation))
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_liveness_probe_sent_at(
        &self,
        peer: Did,
        sent_at_ms: i64,
    ) -> Result<()> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Ok(());
            };
            self.peer_liveness()?
                .force_probe_sent_at(peer, attempt.generation, sent_at_ms);
            Ok(())
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_last_inbound_at(&self, peer: Did, last_inbound_ms: i64) -> Result<()> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Err(Error::InvalidMessage(format!(
                    "cannot age missing active peer {peer}"
                )));
            };
            if self.peer_liveness()?.force_last_inbound_at(
                peer,
                attempt.generation,
                last_inbound_ms,
            ) {
                Ok(())
            } else {
                Err(Error::InvalidMessage(format!(
                    "cannot age missing liveness generation for peer {peer}"
                )))
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn force_peer_connected_at(&self, peer: Did, connected_at_ms: i64) -> Result<()> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Err(Error::InvalidMessage(format!(
                    "cannot age missing active peer {peer}"
                )));
            };
            if self
                .peer_liveness()?
                .force_connected_at(peer, attempt.generation, connected_at_ms)
            {
                Ok(())
            } else {
                Err(Error::InvalidMessage(format!(
                    "cannot age missing liveness generation for peer {peer}"
                )))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_before_send_completion_consumes_the_registered_probe_once() {
        let request = crate::message::test_probe_request(9);
        let tx_id = uuid::Uuid::new_v4();
        let mut liveness = PeerLiveness::new(1, 10);

        liveness.set_pending_probe(tx_id, request);
        assert!(liveness.consume_pending_probe(tx_id, request));
        assert!(!liveness.mark_matching_probe_sent(11, tx_id, request));
        assert_eq!(liveness.unanswered_probe_since_ms, None);
        assert!(!liveness.consume_pending_probe(tx_id, request));
    }

    #[test]
    fn failed_send_cancels_only_its_matching_registered_probe() {
        let request = crate::message::test_probe_request(10);
        let replacement = crate::message::test_probe_request(11);
        let tx_id = uuid::Uuid::new_v4();
        let replacement_tx_id = uuid::Uuid::new_v4();
        let mut liveness = PeerLiveness::new(1, 10);

        liveness.set_pending_probe(replacement_tx_id, replacement);
        liveness.cancel_pending_probe(tx_id, request);
        assert!(liveness.consume_pending_probe(replacement_tx_id, replacement));
    }
}
