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
//! the local topology decides, `Local(head)`, never confirms: either `head ≠ t`, so `t` is
//! absent from the interval; or `head` is this node, which has no successor and is not `t`; or
//! `head = t` is in the local successor list without an announced admission at the first read,
//! because it is admitted but not yet announced (the dial that follows is refused as already
//! connected and settles as a miss until the announcement lands) or because stabilization
//! adopted it from a report without an edge (the dial proceeds and creates the edge Chord's own
//! stabilization seeks). Otherwise the lookup is routed toward `t` and `t` is reachable iff the
//! report that returns under the same transaction id names `t`. A partition that lost `t`
//! answers with the node now succeeding `t`'s position.
//!
//! The report returns Chord-routed to this node, so it also needs this node to be adopted by
//! its own predecessor; in the window right after a first admission no report can arrive and
//! the probe ends by its timeout, not by a refutation. The answer is only as current as the
//! reporter's successor list, in both directions: a
//! predecessor that has not yet adopted a freshly joined `t` refutes it (one redial that, with
//! no local record, forces the direct edge the module otherwise avoids), and a predecessor
//! that still names a departed `t` confirms it for up to one remote grace window (one slow
//! delay before the next look). On-path nodes are trusted exactly as far as Chord routing
//! already trusts them.
//!
//! A dial is the processor's pinned handshake (refused as `InFlight` rather than failed when
//! the local core already holds an unadmitted handshake to the target or refuses the exchange
//! because the target's slot is owned by another attempt; the answering DID is pinned before
//! any offer is created), followed by a wait for the swarm to admit the peer, bounded by
//! [`DIAL_ADMISSION_TIMEOUT`]. Whatever ends the wait short of an admission, the generation the
//! dial reserved is cancelled iff it is still pending, so a handshake that is admitting at that
//! instant completes on its own and the next attempt can otherwise handshake afresh.

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
use crate::error::Result;
use crate::processor::HandshakeFailure;
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

impl From<HandshakeFailure> for DialFailure {
    /// A refused handshake is `InFlight`; a failed one is `Failed`.
    fn from(failure: HandshakeFailure) -> Self {
        match failure {
            HandshakeFailure::InFlight { .. } => Self::InFlight,
            HandshakeFailure::Failed(error) => Self::Failed(error),
        }
    }
}

#[async_trait]
impl BootstrapPort for ProcessorPort {
    /// An announced admission short-circuits; otherwise the lookup decides.
    async fn reachable(&self, target: Did) -> bool {
        let swarm = &self.processor.swarm;
        match swarm.is_peer_admitted(target) {
            Ok(true) => true,
            Ok(false) => lookup_reaches(swarm, self.evidence.reports(), target).await,
            Err(error) => {
                tracing::error!(peer = %target, %error, "connection records unavailable");
                false
            }
        }
    }

    /// The pinned handshake, then the wait for admission; the reserved generation is abandoned
    /// on every exit short of an admission.
    async fn dial(&self, target: &ValidatedSeedPeer) -> std::result::Result<(), DialFailure> {
        let peer = target.did();
        let attempt = self
            .processor
            .connect_peer_via_http(
                target.endpoint(),
                target.api_token(),
                HandshakePeer::Pinned(peer),
            )
            .await?;
        let admitted = self.await_admission(peer).await;
        if !matches!(admitted, Ok(true)) {
            // Only the generation this dial reserved is cancelled, and only while it is still
            // pending; one admitting or admitted at this instant completes on its own and is
            // found admitted by the next probe.
            self.processor.abandon_handshake(attempt).await;
        }
        match admitted {
            Ok(true) => Ok(()),
            Ok(false) => Err(DialFailure::Failed(Error::AdmissionTimedOut { peer })),
            Err(error) => Err(DialFailure::Failed(error)),
        }
    }
}

impl ProcessorPort {
    /// Whether `peer`'s admission is announced within [`DIAL_ADMISSION_TIMEOUT`]. The waiter is
    /// registered before the record is checked, which closes the window: an admission announced
    /// before the check is seen by it, one announced after resolves the waiter. A closed waiter
    /// is unreachable under one turn per target; it reads as the timeout it would otherwise
    /// become.
    async fn await_admission(&self, peer: Did) -> Result<bool> {
        let waiter = self.evidence.admissions().wait_for(peer)?;
        if self
            .processor
            .swarm
            .is_peer_admitted(peer)
            .map_err(Error::ConnectError)?
        {
            return Ok(true);
        }
        Ok(matches!(
            tokio::time::timeout(DIAL_ADMISSION_TIMEOUT, waiter).await,
            Ok(Ok(()))
        ))
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
            tracing::debug!(peer = %target, %error, "bootstrap lookup could not be issued");
            return false;
        }
    };
    let waiter = match reports.wait_for(tx_id) {
        Ok(waiter) => waiter,
        Err(error) => {
            tracing::error!(peer = %target, %error, "bootstrap lookup record unavailable");
            return false;
        }
    };
    matches!(
        tokio::time::timeout(LOOKUP_PROBE_TIMEOUT, waiter).await,
        Ok(Ok(successor)) if successor == target
    )
}
