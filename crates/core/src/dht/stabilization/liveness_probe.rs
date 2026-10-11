//! Liveness probing of admitted peers (L3, see `swarm::transport::liveness`).
//!
//! ```text
//!   due(peer) ─▶ sign ProbeRequest ─▶ issue (deadline starts) ─▶ send_probe ─┬─ Ok: queued
//!                                                                             └─ Err: withdraw
//! ```
//!
//! A probe is due for every admitted peer once a period after its previous one, whatever the
//! peer's traffic or credit. It is issued before it is sent, so its deadline counts from issue;
//! it is queued through the protocol egress without shared capacity and returns at once, so a
//! peer that withholds credit is judged by its own queue and delays no other peer's probe.

use rings_transport::core::transport::WebrtcConnectionState;

use super::Stabilizer;
use crate::error::Result;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;
use crate::swarm::transport::IssuedProbe;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::utils::get_epoch_ms_i64;

impl Stabilizer {
    /// Probe every admitted peer that is due one.
    ///
    /// Law (independence): each probe is queued through the protocol egress, which takes the
    /// connection's readiness now and returns once the probe is queued, so no probe waits on
    /// its peer, and one peer's failure is that peer's alone. A peer that withholds credit
    /// therefore delays no other peer's probe and cannot make the step overrun its deadline.
    pub(super) async fn probe_peer_liveness(&self) -> Result<()> {
        let now_ms = get_epoch_ms_i64();
        // Probe epochs use wall-clock seconds; topology deadlines use the
        // monotonic ring clock elsewhere.
        let unix_seconds = u64::try_from(now_ms).unwrap_or(0) / 1_000;
        let epoch = ProvisionalEpoch::from_unix_seconds(unix_seconds);
        let candidates = self.transport.liveness_probe_candidates(now_ms)?;
        for attempt in candidates {
            if let Err(error) = self.probe_peer(attempt, epoch, now_ms).await {
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %attempt.peer(),
                    error = ?error,
                    "STABILIZATION peer liveness probe failed"
                );
            }
        }
        Ok(())
    }

    /// Sign, issue and send one peer's probe. A probe the clock refuses (one is outstanding,
    /// or the generation was superseded) is not sent.
    async fn probe_peer(
        &self,
        attempt: PendingConnectionAttempt,
        epoch: ProvisionalEpoch,
        now_ms: i64,
    ) -> Result<()> {
        let peer = attempt.peer();
        let request = ProbeRequest::random_for_epoch(epoch);
        let payload = self
            .transport
            .protocol_egress()
            .originate(Message::ProbeRequest(request), peer, Some(peer))
            .await?;
        match self
            .transport
            .issue_liveness_probe(attempt, payload, request, now_ms)?
        {
            Some(probe) => self.send_liveness_probe(probe).await,
            None => Ok(()),
        }
    }

    /// Queue an issued probe; a failure before the queue withdraws it without charging the
    /// peer.
    async fn send_liveness_probe(&self, probe: IssuedProbe) -> Result<()> {
        let (attempt, identity) = (probe.attempt, probe.identity);
        let peer = attempt.peer();
        let peer_state: Option<WebrtcConnectionState> = self
            .transport
            .get_connection(peer)
            .map(|conn| conn.webrtc_connection_state());
        match self.transport.protocol_egress().send_probe(probe).await {
            Ok(()) => tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %peer,
                tx_id = %identity.tx_id,
                "STABILIZATION peer liveness probe queued"
            ),
            Err(error) => {
                self.transport.withdraw_liveness_probe(attempt, identity)?;
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    state = ?peer_state,
                    error = ?error,
                    "STABILIZATION peer liveness probe not queued; withdrawn"
                );
            }
        }
        Ok(())
    }

    /// Test-only hook that exposes liveness probing to the simulator.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn probe_peer_liveness_for_simulation(&self) -> Result<()> {
        self.probe_peer_liveness().await
    }
}
