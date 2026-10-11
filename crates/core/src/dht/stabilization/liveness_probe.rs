//! Liveness probing of admitted peers (L3, see `swarm::transport::liveness`).
//!
//! ```text
//!   due(peer) ─▶ sign ProbeRequest ─▶ issue (deadline starts) ─▶ send_probe ─┬─ Ok: queued
//!   unsent(peer) ─▶ the same probe ───────────────────────────▶ send_probe ─┘  Err: next pass
//! ```
//!
//! A probe is due for every admitted peer once a period after its previous one, whatever the
//! peer's traffic or credit. It is issued before it is sent, so its deadline counts from issue;
//! it is queued through the protocol egress without shared capacity and returns at once, so a
//! peer that withholds credit is judged by its own queue and delays no other peer's probe. A
//! probe that did not reach the queue is sent again, the same probe, on the next pass.

use super::Stabilizer;
use crate::error::Result;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;
use crate::swarm::transport::IssuedProbe;
use crate::swarm::transport::PendingConnectionAttempt;
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
        for work in self.transport.liveness_probe_work(get_epoch_ms_i64())? {
            let probe = match work {
                ProbeWork::Issue(attempt) => match self.issue_probe(attempt).await {
                    Ok(Some(probe)) => probe,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(
                            target: "rings_core::dht::stabilization",
                            local = %self.dht.did,
                            peer = %attempt.peer(),
                            error = ?error,
                            "STABILIZATION peer liveness probe not issued"
                        );
                        continue;
                    }
                },
                ProbeWork::Resend(probe) => *probe,
            };
            self.send_liveness_probe(probe).await?;
        }
        Ok(())
    }

    /// Sign and issue one peer's probe, stamped when it is issued. `None` when the clock refuses
    /// it (one is outstanding, or the generation was superseded).
    async fn issue_probe(&self, attempt: PendingConnectionAttempt) -> Result<Option<IssuedProbe>> {
        let peer = attempt.peer();
        let now_ms = get_epoch_ms_i64();
        // Probe epochs use wall-clock seconds; topology deadlines use the monotonic ring clock.
        let unix_seconds = u64::try_from(now_ms).unwrap_or(0) / 1_000;
        let request =
            ProbeRequest::random_for_epoch(ProvisionalEpoch::from_unix_seconds(unix_seconds));
        let payload = self
            .transport
            .protocol_egress()
            .originate(Message::ProbeRequest(request), peer, Some(peer))
            .await?;
        self.transport
            .issue_liveness_probe(attempt, payload, request, now_ms)
    }

    /// Queue an issued probe. A failure before the queue leaves it outstanding and unsent, to
    /// be sent again on the next pass; its deadline keeps running from its issue.
    async fn send_liveness_probe(&self, probe: IssuedProbe) -> Result<()> {
        let (attempt, identity) = (probe.attempt(), probe.identity());
        match self.transport.protocol_egress().send_probe(probe).await {
            Ok(()) => self.transport.liveness_probe_queued(attempt, identity),
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
