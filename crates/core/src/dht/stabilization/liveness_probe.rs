//! Liveness probing of admitted peers: a probe is due for a peer that is idle or withholds
//! credit, counts as sent once it is queued on the peer's control lane, and is sent through the
//! protocol egress, which never waits on the peer, so a peer that withholds credit is judged
//! without delaying any other.

use rings_transport::core::transport::WebrtcConnectionState;

use super::Stabilizer;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;
use crate::utils::get_epoch_ms_i64;

/// Liveness probe data after signing but before the send is recorded.
struct PreparedLivenessProbe {
    /// Transport attempt whose state owns the probe.
    attempt: PendingConnectionAttempt,
    /// Challenge registered against the outbound payload transaction.
    request: ProbeRequest,
    /// Signed probe payload ready for transport delivery.
    payload: MessagePayload,
    /// Best-effort state snapshot for diagnostics around the send.
    peer_state: Option<WebrtcConnectionState>,
}

impl Stabilizer {
    /// Send liveness probes for admitted peers that are due one and do not already have one
    /// pending.
    ///
    /// Law (independence): each probe is sent through the protocol egress, which takes its
    /// capacity and readiness now and returns once the probe is queued, so no probe waits on its
    /// peer; and one peer's failure is that peer's alone. A peer that withholds credit therefore
    /// delays and aborts no other peer's probe and cannot make the step overrun its deadline.
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

    /// Prepare, register and send one peer's probe.
    async fn probe_peer(
        &self,
        attempt: PendingConnectionAttempt,
        epoch: ProvisionalEpoch,
        now_ms: i64,
    ) -> Result<()> {
        let probe = self.prepare_liveness_probe(attempt, epoch).await?;
        match self.register_liveness_probe(probe)? {
            Some(probe) => self.send_registered_liveness_probe(probe, now_ms).await,
            None => Ok(()),
        }
    }

    /// Build and sign one liveness probe before it is registered as pending.
    async fn prepare_liveness_probe(
        &self,
        attempt: PendingConnectionAttempt,
        epoch: ProvisionalEpoch,
    ) -> Result<PreparedLivenessProbe> {
        let peer = attempt.peer();
        let peer_state = self
            .transport
            .get_connection(peer)
            .map(|conn| conn.webrtc_connection_state());
        let request = ProbeRequest::random_for_epoch(epoch);
        let payload = self
            .transport
            .originate(Message::ProbeRequest(request), peer, Some(peer))
            .await?;
        Ok(PreparedLivenessProbe {
            attempt,
            request,
            payload,
            peer_state,
        })
    }

    /// Register the probe transaction; returns `None` if another current probe
    /// already owns this attempt.
    fn register_liveness_probe(
        &self,
        probe: PreparedLivenessProbe,
    ) -> Result<Option<PreparedLivenessProbe>> {
        self.transport
            .register_pending_liveness_probe(
                probe.attempt,
                probe.payload.transaction.tx_id,
                probe.request,
            )
            .map(|registered| registered.then_some(probe))
    }

    /// Send a registered probe and either mark it sent or cancel the pending record.
    async fn send_registered_liveness_probe(
        &self,
        probe: PreparedLivenessProbe,
        now_ms: i64,
    ) -> Result<()> {
        let PreparedLivenessProbe {
            attempt,
            request,
            payload,
            peer_state,
        } = probe;
        let peer = attempt.peer();
        let tx_id = payload.transaction.tx_id;
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            peer = %peer,
            state = ?peer_state,
            idle_ms = PEER_LIVENESS_IDLE_MS,
            "STABILIZATION peer liveness probe send start"
        );
        // `Ok` once the probe is queued on the peer's control lane: from then on a probe the
        // peer will not let this end send (it withholds the lane's credit) counts as unanswered.
        // An `Err` is a failure before the queue: the capacity or readiness the probe lacked.
        // While this end waits for the peer's credit, that is the peer's own backpressure (its
        // stalled transfers hold the capacity), so the probe is charged as sent; otherwise it
        // is this end's failure and charges the peer nothing.
        match self.transport.protocol_egress().send_payload(payload).await {
            Ok(()) => {
                let matching_probe_recorded = self
                    .transport
                    .record_peer_liveness_probe_sent(attempt, now_ms, tx_id, request)?;
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    tx_id = %tx_id,
                    matching_probe_recorded,
                    "STABILIZATION peer liveness probe send complete"
                );
            }
            Err(error) if self.transport.credit_stalled(peer) => {
                self.transport
                    .record_peer_liveness_probe_sent(attempt, now_ms, tx_id, request)?;
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    state = ?peer_state,
                    error = ?error,
                    "STABILIZATION peer liveness probe not queued behind the peer's withheld credit; charged as unanswered"
                );
            }
            Err(error) => {
                self.transport
                    .cancel_pending_liveness_probe(attempt, tx_id, request)?;
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    state = ?peer_state,
                    error = ?error,
                    records_peer_failure = error.records_peer_send_failure(),
                    "STABILIZATION peer liveness probe send failed"
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
