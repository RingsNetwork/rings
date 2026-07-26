use std::collections::BTreeMap;
use std::sync::MutexGuard;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
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
}

impl PeerLiveness {
    fn new(generation: u64, now_ms: i64) -> Self {
        Self {
            generation,
            connected_at_ms: now_ms,
            last_inbound_ms: now_ms,
            last_probe_ms: None,
            unanswered_probe_since_ms: None,
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

    fn remove(&mut self, peer: Did) {
        self.peers.remove(&peer);
    }

    fn retain_active(&mut self, active: &BTreeMap<Did, u64>) {
        self.peers.retain(|peer, liveness| {
            active
                .get(peer)
                .is_some_and(|generation| *generation == liveness.generation)
        });
    }

    fn probe_candidates(&mut self, active: &BTreeMap<Did, u64>, now_ms: i64) -> Vec<Did> {
        self.retain_active(active);
        for (peer, generation) in active {
            self.peers
                .entry(*peer)
                .or_insert_with(|| PeerLiveness::new(*generation, now_ms));
        }

        self.peers
            .iter()
            .filter_map(|(peer, liveness)| liveness.should_probe(now_ms).then_some(*peer))
            .collect()
    }

    fn mark_probe_sent(&mut self, peer: Did, generation: u64, now_ms: i64) {
        match self.peers.get_mut(&peer) {
            Some(liveness) if liveness.generation == generation => {
                liveness.mark_probe_sent(now_ms);
            }
            _ => {
                let mut liveness = PeerLiveness::new(generation, now_ms);
                liveness.mark_probe_sent(now_ms);
                self.peers.insert(peer, liveness);
            }
        }
    }

    fn expiry(&self, peer: Did, generation: u64, now_ms: i64) -> Option<PeerLivenessExpiry> {
        let liveness = self.peers.get(&peer)?;
        (liveness.generation == generation)
            .then(|| liveness.expiry(now_ms))
            .flatten()
    }

    fn connected_for_ms(&self, peer: Did, generation: u64, now_ms: i64) -> Option<i64> {
        let liveness = self.peers.get(&peer)?;
        (liveness.generation == generation).then(|| liveness.connected_for_ms(now_ms))
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn force_probe_sent_at(&mut self, peer: Did, generation: u64, sent_at_ms: i64) {
        let mut liveness = PeerLiveness::new(generation, sent_at_ms);
        liveness.mark_probe_sent(sent_at_ms);
        self.peers.insert(peer, liveness);
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn force_connected_at(&mut self, peer: Did, generation: u64, connected_at_ms: i64) {
        self.peers
            .insert(peer, PeerLiveness::new(generation, connected_at_ms));
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
    fn peer_liveness(&self) -> Result<MutexGuard<'_, PeerLivenessMap>> {
        self.peer_liveness
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    fn active_generations(&self) -> Result<BTreeMap<Did, u64>> {
        Ok(self.active_peers()?.clone())
    }

    fn active_generation(&self, peer: Did) -> Result<Option<u64>> {
        Ok(self.active_peers()?.get(&peer).copied())
    }

    pub(crate) fn mark_peer_liveness_connected(&self, peer: Did) {
        let now_ms = get_epoch_ms_i64();
        let generation = match self.active_generation(peer) {
            Ok(Some(generation)) => generation,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    "failed to read active generation for connected peer {peer}: {error}"
                );
                return;
            }
        };
        if let Err(error) = self
            .peer_liveness()
            .map(|mut liveness| liveness.mark_connected(peer, generation, now_ms))
        {
            tracing::warn!("failed to mark liveness for connected peer {peer}: {error}");
        }
    }

    pub(crate) fn mark_peer_liveness_inbound(&self, peer: Did) {
        let now_ms = get_epoch_ms_i64();
        let generation = match self.active_generation(peer) {
            Ok(Some(generation)) => generation,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("failed to read active generation for inbound peer {peer}: {error}");
                return;
            }
        };
        if let Err(error) = self
            .peer_liveness()
            .map(|mut liveness| liveness.mark_inbound(peer, generation, now_ms))
        {
            tracing::warn!("failed to mark liveness for inbound peer {peer}: {error}");
        }
    }

    pub(crate) fn remove_peer_liveness(&self, peer: Did) -> Result<()> {
        self.peer_liveness()?.remove(peer);
        Ok(())
    }

    pub(crate) fn liveness_probe_candidates(&self, now_ms: i64) -> Result<Vec<Did>> {
        let active = self.active_generations()?;
        Ok(self.peer_liveness()?.probe_candidates(&active, now_ms))
    }

    pub(crate) fn record_peer_liveness_probe_sent(&self, peer: Did, now_ms: i64) -> Result<()> {
        let Some(generation) = self.active_generation(peer)? else {
            return Ok(());
        };
        self.peer_liveness()?
            .mark_probe_sent(peer, generation, now_ms);
        Ok(())
    }

    pub(crate) fn peer_liveness_expiry(
        &self,
        peer: Did,
        now_ms: i64,
    ) -> Result<Option<PeerLivenessExpiry>> {
        let Some(generation) = self.active_generation(peer)? else {
            return Ok(None);
        };
        Ok(self.peer_liveness()?.expiry(peer, generation, now_ms))
    }

    /// Return how long an admitted peer has owned its current active generation.
    pub(crate) fn peer_connected_for_ms(&self, peer: Did, now_ms: i64) -> Result<Option<i64>> {
        let Some(generation) = self.active_generation(peer)? else {
            return Ok(None);
        };
        Ok(self
            .peer_liveness()?
            .connected_for_ms(peer, generation, now_ms))
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_liveness_probe_sent_at(
        &self,
        peer: Did,
        sent_at_ms: i64,
    ) -> Result<()> {
        let Some(generation) = self.active_generation(peer)? else {
            return Ok(());
        };
        self.peer_liveness()?
            .force_probe_sent_at(peer, generation, sent_at_ms);
        Ok(())
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_connected_at(&self, peer: Did, connected_at_ms: i64) -> Result<()> {
        let Some(generation) = self.active_generation(peer)? else {
            return Ok(());
        };
        self.peer_liveness()?
            .force_connected_at(peer, generation, connected_at_ms);
        Ok(())
    }
}
