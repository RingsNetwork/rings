//! The production port over a live [`Processor`]: overlay reachability probe and HTTP redial.
//!
//! Reachability is a routed Chord lookup, not a direct-edge check: `Swarm::lookup_successor`
//! answers the target `t` exactly when `t` is in the overlay, as far as the answering node's
//! successor list is current. With the local step taken at one snapshot of the local topology
//! and the answer at one snapshot of the reporter's:
//!
//! ```text
//!   reachable(t)  ⟺  is_peer_admitted(t)
//!                  ∨  lookup_successor(t) = Routed(tx) ∧ report(tx).successor = t
//! ```
//!
//! Two cases are decided without any network round trip. A target admitted as the application
//! sees it is reachable: ready or recovering, its transport is the swarm's to heal or retire,
//! its retirement will be reported, and a dial would be refused as already connected. A lookup
//! the local topology decides, `Local(head)`, never confirms: `head` is an admitted peer, so
//! either `head ≠ t` and `t` is absent, or `head = t` whose admission is not yet announced;
//! both read as unreachable, and in the second case the dial that follows is refused as already
//! connected and deferred, not counted, until the announcement lands. Otherwise the lookup is
//! routed toward `t` and `t` is reachable iff the report that returns under the same
//! transaction id names `t`. A partition that lost `t` answers with the node now succeeding
//! `t`'s position.
//!
//! The answer is only as current as the reporter's successor list, in both directions: a
//! predecessor that has not yet adopted a freshly joined `t` refutes it (one redial that, with
//! no local record, forces the direct edge the module otherwise avoids), and a predecessor
//! that still names a departed `t` confirms it for up to one remote grace window (one slow
//! delay before the next look). On-path nodes are trusted exactly as far as Chord routing
//! already trusts them.
//!
//! A dial is refused without any request when the core already holds an unadmitted handshake
//! to the target, whichever side started it, so an in-flight handshake is never charged as a
//! failure; the same holds when the exchange itself is refused because the target's slot is
//! owned by another generation. Otherwise the handshake pins the answering DID before any
//! offer is created, and the port then waits for the swarm to admit the peer, bounded by
//! [`DIAL_ADMISSION_TIMEOUT`]; on timeout the generation this dial reserved is cancelled iff it
//! is still pending, so a handshake that is admitting at that instant completes on its own and
//! the next attempt can otherwise handshake afresh.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::swarm::SuccessorLookup;
use rings_core::swarm::Swarm;

use super::evidence::LookupReportLedger;
use super::evidence::ReachabilityEvidence;
use super::BootstrapPort;
use super::DialFailure;
use crate::error::Error;
use crate::processor::HandshakePeer;
use crate::processor::Processor;
use crate::seed::ValidatedSeedPeer;

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
    /// An announced admission short-circuits; otherwise the lookup decides.
    async fn reachable(&self, target: &ValidatedSeedPeer) -> bool {
        let swarm = &self.processor.swarm;
        match swarm.is_peer_admitted(target.did()) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => {
                tracing::error!(target = %target.did(), %error, "connection records unavailable");
                return false;
            }
        }
        lookup_reaches(swarm, self.evidence.reports(), target.did()).await
    }

    /// Refuse while a handshake is pending; else handshake pinned to the target's DID, then
    /// wait for its admission.
    async fn dial(&self, target: &ValidatedSeedPeer) -> std::result::Result<(), DialFailure> {
        let peer = target.did();
        let swarm = &self.processor.swarm;
        let core_failure = |error| DialFailure::Failed(Error::ConnectError(error));
        if swarm.has_pending_connection(peer).map_err(core_failure)? {
            return Err(DialFailure::InFlight);
        }
        let handshake = self
            .processor
            .connect_peer_via_http(
                target.endpoint(),
                target.api_token(),
                HandshakePeer::Pinned(peer),
            )
            .await
            .map_err(classify_handshake_error)?;
        // The waiter is registered before the record is checked, which closes the window: an
        // admission landing before the check is seen by it, one landing after resolves the
        // waiter. A closed waiter is unreachable under one turn per target; it reads as the
        // timeout it would otherwise become.
        let waiter = self
            .evidence
            .admissions()
            .wait_for(peer)
            .map_err(DialFailure::Failed)?;
        let admitted = swarm.is_peer_admitted(peer).map_err(core_failure)?
            || matches!(
                tokio::time::timeout(DIAL_ADMISSION_TIMEOUT, waiter).await,
                Ok(Ok(()))
            );
        self.evidence
            .admissions()
            .forget(peer)
            .map_err(DialFailure::Failed)?;
        if admitted {
            return Ok(());
        }
        // Only the generation this dial reserved is cancelled, and only while it is still
        // pending; one admitting or admitted at this instant completes on its own and is found
        // admitted by the next probe.
        self.processor.abandon_handshake(handshake.attempt).await;
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

/// Whether a successor lookup for `target` answers `target` itself: a lookup the local topology
/// decides is refuted (see the module doc), a routed one must report within
/// [`LOOKUP_PROBE_TIMEOUT`].
async fn lookup_reaches(swarm: &Swarm, reports: &LookupReportLedger, target: Did) -> bool {
    let tx_id = match swarm.lookup_successor(target).await {
        Ok(SuccessorLookup::Routed(tx_id)) => tx_id,
        Ok(SuccessorLookup::Local(_)) => return false,
        Err(error) => {
            tracing::debug!(%target, %error, "bootstrap lookup could not be issued");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The core refusing an offer, or superseding this node's attempt with the peer's own
    /// offer, is a deferral; every other failure counts.
    #[test]
    fn handshake_errors_sort_into_deferral_and_failure() {
        let superseded = || rings_core::error::Error::ConnectionAttemptSuperseded {
            peer: Did::from(1),
            generation: 1,
        };
        for deferred in [
            Error::CreateOffer(rings_core::error::Error::AlreadyConnected),
            Error::CreateOffer(superseded()),
            Error::AcceptAnswer(superseded()),
        ] {
            assert!(matches!(
                classify_handshake_error(deferred),
                DialFailure::InFlight
            ));
        }
        assert!(matches!(
            classify_handshake_error(Error::AdmissionTimedOut { peer: Did::from(1) }),
            DialFailure::Failed(Error::AdmissionTimedOut { .. })
        ));
        assert!(matches!(
            classify_handshake_error(Error::AcceptAnswer(
                rings_core::error::Error::AlreadyConnected
            )),
            DialFailure::Failed(Error::AcceptAnswer(_))
        ));
    }
}
