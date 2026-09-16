//! The production port over a live [`Processor`]: overlay reachability probe and HTTP redial.
//!
//! Reachability is a routed Chord lookup, not a direct-edge check. In a Chord ring every present
//! node is the successor of its own identifier, so a successor lookup for the target `t` answers
//! `t` exactly when `t` is in the overlay. With the local step taken at one snapshot of the
//! local topology and the answer at one snapshot of the reporter's:
//!
//! ```text
//!   reachable(t)  ⟺  is_peer_connected(t)
//!                  ∨  find_successor(t) ∈ RemoteAction ∧ report(tx).successor = t
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
//! without successors decides locally and sends nothing.
//!
//! The answer is only as current as the reporter's successor list, in both directions: a
//! predecessor that has not yet adopted a freshly joined `t` refutes it (one redial that, with
//! no local record, forces the direct edge the module otherwise avoids), and a predecessor
//! that still names a departed `t` confirms it for up to one remote grace window (one slow
//! delay before the next look). On-path nodes are trusted exactly as far as Chord routing
//! already trusts them.
//!
//! A dial is refused without any request when the core already holds a connection attempt to
//! the target, so the node's own in-flight handshake is never charged as a failure. Otherwise
//! the handshake pins the answering DID before any offer is created, and the port then waits
//! for the swarm to admit the peer, bounded by [`DIAL_ADMISSION_TIMEOUT`]; on timeout the
//! node's own unadmitted attempt is cancelled so the next attempt can handshake afresh.

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

use super::evidence::LookupReportLedger;
use super::evidence::ReachabilityEvidence;
use super::BootstrapPort;
use super::DialFailure;
use super::ManagedTarget;
use crate::error::Error;
use crate::processor::HandshakePeer;
use crate::processor::Processor;

/// Longest a routed lookup probe waits for its report before the target counts as unreachable.
const LOOKUP_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a dial waits, after its answer is accepted, for the swarm to admit the peer. ICE
/// over STUN completes within seconds; a peer that never comes up must not hold a burst
/// attempt for the core's full pending window.
const DIAL_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// Refuse while an attempt exists; else handshake pinned to the target's DID, then wait for
    /// its admission.
    async fn dial(&self, target: &ManagedTarget) -> std::result::Result<(), DialFailure> {
        let peer = target.did();
        let swarm = &self.processor.swarm;
        if swarm
            .has_connection_attempt(peer)
            .map_err(|error| DialFailure::Failed(Error::ConnectError(error)))?
        {
            return Err(DialFailure::InFlight);
        }
        self.processor
            .connect_peer_via_http(
                target.url(),
                target.api_token(),
                HandshakePeer::Pinned(peer),
            )
            .await
            .map_err(classify_handshake_error)?;
        let admitted = self
            .evidence
            .admissions()
            .wait_for(peer)
            .map_err(DialFailure::Failed)?;
        // Registering before the direct check closes the window: an admission landing before
        // the check is seen by it, one landing after resolves the waiter.
        let outcome = if swarm.is_peer_connected(peer) {
            Ok(Ok(()))
        } else {
            tokio::time::timeout(DIAL_ADMISSION_TIMEOUT, admitted).await
        };
        self.evidence
            .admissions()
            .forget(peer)
            .map_err(DialFailure::Failed)?;
        if matches!(outcome, Ok(Ok(()))) {
            return Ok(());
        }
        // A closed waiter is unreachable under one turn per target; it is treated as the
        // timeout it would otherwise become. An attempt admitted after the timeout is left
        // alone and found connected by the next probe.
        if let Err(error) = swarm.cancel_pending_connection(peer).await {
            tracing::warn!(%peer, %error, "failed to cancel the timed-out handshake");
        }
        Err(DialFailure::Failed(Error::AdmissionTimedOut { peer }))
    }
}

/// Sort a handshake error: the core refusing because it already holds an attempt to the peer
/// (reserved concurrently, or superseded by the peer's own offer) is a deferral, anything else
/// a failure.
fn classify_handshake_error(error: Error) -> DialFailure {
    match error {
        Error::CreateOffer(
            rings_core::error::Error::AlreadyConnected
            | rings_core::error::Error::ConnectionAttemptSuperseded { .. },
        )
        | Error::AcceptAnswer(rings_core::error::Error::ConnectionAttemptSuperseded { .. }) => {
            DialFailure::InFlight
        }
        other => DialFailure::Failed(other),
    }
}

/// Whether a successor lookup for `target` answers `target` itself, deciding locally when the
/// target's position lies in the local successor interval and otherwise by a routed request
/// that must report within [`LOOKUP_PROBE_TIMEOUT`].
async fn lookup_reaches(swarm: &Swarm, reports: &LookupReportLedger, target: Did) -> bool {
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
    let waiter = match reports.wait_for(tx_id) {
        Ok(waiter) => waiter,
        Err(error) => {
            tracing::error!(%target, %error, "bootstrap lookup record unavailable");
            return false;
        }
    };
    let outcome = tokio::time::timeout(LOOKUP_PROBE_TIMEOUT, waiter).await;
    if let Err(error) = reports.forget(tx_id) {
        tracing::error!(%target, %error, "bootstrap lookup record unavailable");
    }
    matches!(outcome, Ok(Ok(successor)) if successor == target)
}
