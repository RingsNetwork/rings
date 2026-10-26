//! Liveness probing of admitted peers, judged by probes alone (see `swarm::transport::liveness`).
//!
//! ```text
//!   due(peer)    ─▶ sign a fresh challenge ─▶ issue (deadline starts) ─┐
//!   unsent(peer) ─▶ sign its challenge anew ─▶ resend (same deadline) ──┴▶ send_probe
//!   send_probe: Ok ─▶ queued;  Err ─▶ unsent, sent again next pass
//! ```
//!
//! A probe is due for every admitted peer once a period after its previous one, whatever the
//! peer's traffic or credit. It is issued before it is sent, so its deadline counts from issue;
//! it is queued through the protocol egress without shared capacity and returns at once, so a
//! peer that withholds credit is judged by its own queue and delays no other peer's probe. A
//! probe that did not reach the queue is sent again on the next pass: the same challenge, under
//! the deadline of its first issue, signed anew so its transaction's sequence is current.
//!
//! This node's relay chain neighbours are probed on the chain period by a pass of their own,
//! run by the chain task beside the maintenance loop, and one whose probe goes unanswered for
//! the chain timeout is suspected for relay custody (#1070).

use super::Stabilizer;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;
use crate::swarm::transport::IssuedProbe;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::ProbePass;
use crate::swarm::transport::ProbeScope;
use crate::swarm::transport::ProbeWork;
use crate::utils::get_epoch_ms_i64;

impl Stabilizer {
    /// Probe every admitted peer that is due one.
    ///
    /// Law (independence): each probe is queued through the protocol egress, which takes the
    /// connection's readiness now and returns once the probe is queued, so no probe waits on
    /// its peer, and one peer's failure is that peer's alone. A peer that withholds credit
    /// therefore delays no other peer's probe and cannot make the step overrun its deadline.
    pub(super) async fn probe_peer_liveness(&self) -> Result<()> {
        self.run_probe_pass(ProbeScope::Every).await
    }

    /// Probe this node's relay chain neighbours that are due one on the chain period, and
    /// suspect for relay custody each whose probe is the chain timeout unanswered (#1070,
    /// `swarm::transport::liveness`): the chain moves past it at once, and nothing is evicted.
    /// Run every `CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS` by the chain task.
    pub(crate) async fn probe_relay_chain_neighbours(&self) -> Result<()> {
        self.run_probe_pass(ProbeScope::ChainNeighbours).await?;
        for peer in self
            .transport
            .unanswering_chain_neighbours(get_epoch_ms_i64())?
        {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                %peer,
                "STABILIZATION relay chain neighbour suspected: probe unanswered"
            );
            self.dht.suspect_relay_chain_neighbour(peer)?;
        }
        Ok(())
    }

    /// One probe pass of `scope`; see [`Self::probe_peer_liveness`].
    async fn run_probe_pass(&self, scope: ProbeScope) -> Result<()> {
        let Some(pass) = self
            .transport
            .liveness_probe_pass(get_epoch_ms_i64(), scope)?
        else {
            return Ok(());
        };
        for &work in pass.work() {
            let (attempt, prepared) = match work {
                ProbeWork::Issue(attempt) => (attempt, self.issue_probe(&pass, attempt).await),
                ProbeWork::Resend { attempt, request } => {
                    (attempt, self.resend_probe(&pass, attempt, request).await)
                }
            };
            match prepared {
                Ok(Some(probe)) => self.send_liveness_probe(probe).await?,
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %attempt.peer(),
                    error = ?error,
                    "STABILIZATION peer liveness probe not prepared"
                ),
            }
        }
        Ok(())
    }

    /// Sign and issue one peer's probe, stamped when it is issued. `None` when the clock refuses
    /// it (one is outstanding, or the generation was superseded).
    async fn issue_probe(
        &self,
        pass: &ProbePass<'_>,
        attempt: PendingConnectionAttempt,
    ) -> Result<Option<IssuedProbe>> {
        let now_ms = get_epoch_ms_i64();
        // Probe epochs use wall-clock seconds; topology deadlines use the monotonic ring clock.
        let unix_seconds = u64::try_from(now_ms).unwrap_or(0) / 1_000;
        let request =
            ProbeRequest::random_for_epoch(ProvisionalEpoch::from_unix_seconds(unix_seconds));
        let payload = self.sign_probe(attempt, request).await?;
        self.transport
            .issue_liveness_probe(pass, attempt, payload, request, now_ms)
    }

    /// Sign the outstanding probe of challenge `request` anew, under a fresh transaction and
    /// sequence, to send it again. `None` when it was answered or reached the queue meanwhile.
    async fn resend_probe(
        &self,
        pass: &ProbePass<'_>,
        attempt: PendingConnectionAttempt,
        request: ProbeRequest,
    ) -> Result<Option<IssuedProbe>> {
        let payload = self.sign_probe(attempt, request).await?;
        self.transport
            .resend_liveness_probe(pass, attempt, payload, request, get_epoch_ms_i64())
    }

    /// The signed payload of the probe of challenge `request` to `attempt`'s peer.
    async fn sign_probe(
        &self,
        attempt: PendingConnectionAttempt,
        request: ProbeRequest,
    ) -> Result<MessagePayload> {
        let peer = attempt.peer();
        self.transport
            .protocol_egress()
            .originate(Message::ProbeRequest(request), peer, Some(peer))
            .await
    }

    /// Queue an issued probe. A failure before the queue leaves it outstanding and unsent, to
    /// be sent again on the next pass; its deadline keeps running from its issue.
    async fn send_liveness_probe(&self, probe: IssuedProbe) -> Result<()> {
        let (attempt, request) = (probe.attempt(), probe.request());
        match self.transport.protocol_egress().send_probe(probe).await {
            Ok(()) => self.transport.liveness_probe_queued(attempt, request),
            Err(error) => {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %attempt.peer(),
                    error = ?error,
                    "STABILIZATION peer liveness probe not queued; sent again next pass"
                );
                Ok(())
            }
        }
    }

    /// Test-only hook that exposes liveness probing to the simulator.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn probe_peer_liveness_for_simulation(&self) -> Result<()> {
        self.probe_peer_liveness().await
    }
}
