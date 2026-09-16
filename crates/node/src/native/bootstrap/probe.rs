//! The production port over a live [`Processor`]: overlay reachability probe and HTTP redial.
//!
//! Reachability is a routed Chord lookup, not a direct-edge check. In a Chord ring every present
//! node is the successor of its own identifier, so a successor lookup for the target `t` answers
//! `t` exactly when `t` is in the overlay. Taken at one snapshot of the local topology:
//!
//! ```text
//!   reachable(t)  ⟺  is_peer_connected(t)
//!                  ∨  find_successor(t) = RemoteAction(next, _) ∧ report(tx).successor = t
//! ```
//!
//! The local step of the lookup decides two cases without any network round trip. A target
//! with a ready direct transport is reachable. A target whose identifier falls in the local
//! successor interval `(n, head]` gets `Some(head)`: either `head ≠ t`, so `t` is absent, or
//! `head = t` while its transport is not ready (else the direct check would have held); both
//! read as unreachable, and the routed request would only travel the ring to say the same.
//! Otherwise the probe sends `FindSuccessorSend { did: t, strict: false }` toward `t` and
//! accepts `t` as reachable iff the report that returns under the same transaction id names
//! `t`. A partition that lost `t` answers with the node now succeeding `t`'s position; a node
//! without successors decides locally and sends nothing. The report is authored by `t`'s
//! predecessor, which must already hold `t` as its successor head (Chord convergence); a not
//! yet converged ring can therefore refute a present target once, costing one redial that the
//! core then reports as an already existing attempt. On-path nodes are trusted exactly as far
//! as Chord routing already trusts them.
//!
//! A dial completes only on admission: the handshake pins the answering DID before any offer
//! is created, and the port then waits for the swarm to admit the peer, bounded by
//! [`DIAL_ADMISSION_TIMEOUT`], so a handshake whose transport never comes up is a failed dial
//! rather than a reachable target.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::Chord;
use rings_core::dht::Did;
use rings_core::dht::PeerRingAction;
use rings_core::message::FindSuccessorReportHandler;
use rings_core::message::FindSuccessorSend;
use rings_core::message::FindSuccessorThen;
use rings_core::message::Message;
use rings_core::swarm::Swarm;

use super::evidence::ReachabilityEvidence;
use super::BootstrapPort;
use super::ManagedTarget;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// Longest a routed lookup probe waits for its report before the target counts as unreachable.
pub(crate) const LOOKUP_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a dial waits, after its answer is accepted, for the swarm to admit the peer. ICE
/// over STUN completes within seconds; a peer that never comes up must not hold a burst
/// attempt for the core's full pending window.
pub(crate) const DIAL_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// `BootstrapPort` over a live processor: routed probe plus admitted HTTP handshake.
pub(crate) struct ProcessorPort {
    processor: Arc<Processor>,
    evidence: Arc<ReachabilityEvidence>,
}

impl ProcessorPort {
    /// A port whose probes and dials rendezvous with the swarm's reports in `evidence`.
    pub(crate) fn new(processor: Arc<Processor>, evidence: Arc<ReachabilityEvidence>) -> Self {
        Self {
            processor,
            evidence,
        }
    }
}

#[async_trait]
impl BootstrapPort for ProcessorPort {
    /// A ready direct transport short-circuits; otherwise the lookup decides.
    async fn reachable(&self, target: &ManagedTarget) -> bool {
        if self.processor.swarm.is_peer_connected(target.did()) {
            return true;
        }
        lookup_reaches(&self.processor.swarm, self.evidence.reports(), target.did()).await
    }

    /// Handshake pinned to the target's DID, then wait for its admission.
    async fn dial(&self, target: &ManagedTarget) -> Result<()> {
        let peer = target.did();
        self.processor
            .connect_peer_via_http(target.url(), target.api_token(), Some(peer))
            .await?;
        let admitted = self.evidence.admissions().await_admission(peer)?;
        if self.processor.swarm.is_peer_connected(peer) {
            self.evidence.admissions().forget(peer)?;
            return Ok(());
        }
        let outcome = tokio::time::timeout(DIAL_ADMISSION_TIMEOUT, admitted).await;
        self.evidence.admissions().forget(peer)?;
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) | Err(_) => Err(Error::AdmissionTimedOut { peer }),
        }
    }
}

/// Whether a successor lookup for `target` answers `target` itself, deciding locally when the
/// target's position lies in the local successor interval and otherwise by a routed request
/// that must report within [`LOOKUP_PROBE_TIMEOUT`].
async fn lookup_reaches(
    swarm: &Swarm,
    reports: &super::evidence::LookupReportLedger,
    target: Did,
) -> bool {
    match swarm.dht().find_successor(target) {
        Ok(PeerRingAction::RemoteAction(..)) => {}
        Ok(PeerRingAction::Some(_)) => return false,
        Ok(action) => {
            tracing::debug!(%target, ?action, "bootstrap lookup took no routable step");
            return false;
        }
        Err(error) => {
            tracing::debug!(%target, %error, "bootstrap lookup could not take its local step");
            return false;
        }
    }
    let request = Message::FindSuccessorSend(FindSuccessorSend {
        did: target,
        strict: false,
        then: FindSuccessorThen::Report(FindSuccessorReportHandler::None),
    });
    let tx_id = match swarm.send_message(request, target).await {
        Ok(tx_id) => tx_id,
        Err(error) => {
            tracing::debug!(%target, %error, "bootstrap lookup probe could not be routed");
            return false;
        }
    };
    let waiter = match reports.await_report(tx_id) {
        Ok(waiter) => waiter,
        Err(error) => {
            tracing::error!(%target, %error, "bootstrap lookup ledger unavailable");
            return false;
        }
    };
    let outcome = tokio::time::timeout(LOOKUP_PROBE_TIMEOUT, waiter).await;
    if let Err(error) = reports.forget(tx_id) {
        tracing::error!(%target, %error, "bootstrap lookup ledger unavailable");
    }
    match outcome {
        Ok(Ok(successor)) => successor == target,
        Ok(Err(_)) | Err(_) => false,
    }
}
