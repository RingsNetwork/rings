//! Real-network checks of the bootstrap plumbing: the routed successor lookup issued by
//! `ProcessorPort::reachable`, the backend's translation of the inbox's peer facts into
//! reachability evidence, and the core's `PeerRetired` event on a locally decided disconnect.
//!
//! Topology for the probe: `C — B — A`, with keys chosen so that `A` is `B`'s successor head
//! (clockwise from `B`, `A` precedes `C`). `C`'s only peer is `B`, so every lookup `C` issues
//! goes to `B`, which answers `A` for any key in `(B, A]`.

#[cfg(feature = "dummy")]
use std::collections::BTreeSet;
use std::sync::Arc;

use rings_core::ecc::SecretKey;
#[cfg(feature = "dummy")]
use rings_core::swarm::SuccessorLookup;

use super::common::*;
use super::*;
use crate::extension::ext::LinkFact;
use crate::extension::BackendObserver;
use crate::native::bootstrap::BootstrapPort;
use crate::native::bootstrap::DialFailure;
use crate::native::bootstrap::ProcessorPort;
use crate::native::bootstrap::ReachabilityEvidence;
use crate::seed::SeedPeer;
use crate::seed::ValidatedSeedPeer;
#[cfg(feature = "dummy")]
use crate::tests::activity::probe_on_activity;
use crate::tests::activity::record_activity;

/// Draws of random identity keys before giving up on a chain layout.
#[cfg(feature = "dummy")]
const CHAIN_KEY_DRAWS: usize = 512;
/// An endpoint that validates as public and is never dialed.
const NEVER_DIALED: &str = "https://never-dialed.example.org:50001/";

/// Reachability evidence that records activity after every fact it records, so probes waiting
/// on an admission or retirement the backend reported wake on it.
struct ActiveEvidence(Arc<ReachabilityEvidence>);

impl BackendObserver for ActiveEvidence {
    fn link_fact(&self, fact: LinkFact) {
        self.0.link_fact(fact);
        record_activity();
    }
}

/// One node of the probe topology.
struct ProbeNode {
    processor: Arc<Processor>,
    evidence: Arc<ReachabilityEvidence>,
}

impl ProbeNode {
    /// A node over `key` whose application inbox is served by a backend that writes fresh
    /// reachability evidence.
    async fn new(key: SecretKey) -> Self {
        let processor = Arc::new(prepare_processor_with_identity_key(key).await);
        let evidence = Arc::new(ReachabilityEvidence::default());
        // Served by the extension backend as a node task, recording activity on every fact.
        Provider::from_processor(processor.clone())
            .serve_application(Some(Arc::new(ActiveEvidence(evidence.clone()))))
            .expect("the probe node's inbox is unclaimed");
        Self {
            processor,
            evidence,
        }
    }

    /// DID of this node.
    fn did(&self) -> Did {
        self.processor.did()
    }
}

/// Three identity keys whose DIDs satisfy `A - B < C - B` (clockwise ring distance, the `Sub`
/// on `Did`), so `A` is `B`'s successor head once both are connected to `B`.
#[cfg(feature = "dummy")]
fn chain_keys() -> (SecretKey, SecretKey, SecretKey) {
    for _ in 0..CHAIN_KEY_DRAWS {
        let a = SecretKey::random();
        let b = SecretKey::random();
        let c = SecretKey::random();
        let b_did = Did::from(b.address());
        if Did::from(a.address()) - b_did < Did::from(c.address()) - b_did {
            return (a, b, c);
        }
    }
    panic!("no chain layout in {CHAIN_KEY_DRAWS} draws; the ordering has probability 1/2");
}

/// A seed entry for `did` at the never-dialed endpoint.
fn never_dialed(did: Did) -> SeedPeer {
    SeedPeer {
        did: did.to_string(),
        url: NEVER_DIALED.to_string(),
        api_token: None,
    }
}

/// A managed target for `did` at the never-dialed endpoint.
fn target(did: Did) -> ValidatedSeedPeer {
    ValidatedSeedPeer::try_from(never_dialed(did)).expect("a public endpoint validates")
}

/// Present targets are reachable through one hop (verified to be routed, not direct), an
/// absent key is not, a direct peer needs no lookup at all, and a key in `C`'s successor
/// interval is refuted without leaving `C`.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_routed_probe_reports_presence_through_one_hop() {
    let _guard = network_test_guard().await;
    let (a_key, b_key, c_key) = chain_keys();
    let a = ProbeNode::new(a_key).await;
    let b = ProbeNode::new(b_key).await;
    let c = ProbeNode::new(c_key).await;

    connect_processors(&b.processor, &a.processor).await;
    // The admission of B is what makes B reachable to C without a lookup, so the test waits
    // for the event, not merely for transport readiness.
    let b_admitted = c
        .evidence
        .admissions()
        .wait_for(b.did())
        .expect("record readable");
    connect_processors(&c.processor, &b.processor).await;
    assert_eq!(
        b_admitted.await,
        Ok(()),
        "the admission of B is announced to C"
    );

    let port = ProcessorPort::new(c.processor.clone(), c.evidence.clone());
    assert!(
        port.reachable(b.did()).await,
        "an announced admission is reachable without a lookup"
    );
    assert!(
        !c.processor
            .swarm
            .is_peer_admitted(a.did())
            .expect("records readable"),
        "the probe for A must be routed, not short-circuited"
    );
    assert!(
        port.reachable(a.did()).await,
        "a target one hop away answers as its own successor"
    );
    let absent = b.did() + Did::from(1);
    assert_ne!(absent, a.did());
    assert_ne!(absent, c.did());
    assert!(
        !port.reachable(absent).await,
        "an absent key is answered by another node's successor"
    );
    let own_interval = c.did() + Did::from(1);
    assert_ne!(own_interval, b.did());
    assert!(
        matches!(
            c.processor.swarm.lookup_successor(own_interval).await,
            Ok(SuccessorLookup::Local(head)) if head == b.did()
        ),
        "a key in C's successor interval is decided locally, by C's successor head"
    );
    assert!(
        !port.reachable(own_interval).await,
        "a locally decided lookup is a refutation"
    );
}

/// A disconnect decided by the local node retires the peer through the core's single
/// retirement transition and reaches the evidence as a loss, without any physical terminal
/// state. The disconnect follows the admission *event*, not merely transport readiness: a
/// retirement before the backend read the admission withdraws it, and the loss is never seen.
///
/// The retirement is announced to the inbox before `disconnect` returns, and the backend reads
/// it at its own pace (#935), so the loss is awaited on the activity the backend records.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_a_local_disconnect_is_reported_as_a_peer_retirement() {
    let _guard = network_test_guard().await;
    let a = ProbeNode::new(SecretKey::random()).await;
    let b = ProbeNode::new(SecretKey::random()).await;
    let admitted = a
        .evidence
        .admissions()
        .wait_for(b.did())
        .expect("record readable");
    connect_processors(&a.processor, &b.processor).await;
    assert_eq!(
        admitted.await,
        Ok(()),
        "the admission of B is announced to A"
    );
    assert!(a
        .evidence
        .losses()
        .take()
        .expect("record readable")
        .is_empty());

    a.processor
        .swarm
        .disconnect(b.did())
        .await
        .expect("disconnect succeeds");
    let lost = probe_on_activity(
        "the retirement reaches the evidence",
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let lost = a.evidence.losses().take().expect("record readable");
            async move { Ok((!lost.is_empty()).then_some(lost)) }
        },
    )
    .await
    .expect("the retirement of B is announced to A");
    assert_eq!(lost, BTreeSet::from([b.did()]));
}

/// A pending handshake to the target, here one this node reserved by offering, refuses a dial
/// before any request leaves: the never-dialed endpoint would otherwise fail to resolve.
#[tokio::test]
async fn test_a_pending_handshake_refuses_the_dial_without_a_request() {
    let _guard = network_test_guard().await;
    let a = ProbeNode::new(SecretKey::random()).await;
    let b = ProbeNode::new(SecretKey::random()).await;
    let (attempt, _offer) = a
        .processor
        .swarm
        .offer_connection(b.did())
        .await
        .expect("offer reserves a generation");
    let port = ProcessorPort::new(a.processor.clone(), a.evidence.clone());
    assert!(matches!(
        port.dial(&target(b.did())).await,
        Err(DialFailure::InFlight)
    ));
    assert!(a
        .processor
        .swarm
        .cancel_pending_connection_attempt(attempt)
        .await
        .expect("records readable"));
}
