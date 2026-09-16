//! Real-network checks of the bootstrap plumbing: the routed successor lookup issued by
//! `ProcessorPort::reachable`, the backend's translation of swarm events into reachability
//! evidence, and the core's `PeerRetired` event on a locally decided disconnect.
//!
//! Topology for the probe: `C — B — A`, with keys chosen so that `A` is `B`'s successor head
//! (clockwise from `B`, `A` precedes `C`). `C`'s only peer is `B`, so every lookup `C` issues
//! goes to `B`, which answers `A` for any key in `(B, A]`.

use std::sync::Arc;

use rings_core::ecc::SecretKey;
use rings_core::swarm::SuccessorLookup;
use tokio::sync::oneshot::error::TryRecvError;

use super::common::*;
use super::*;
use crate::extension::Backend;
use crate::native::bootstrap::BootstrapPort;
use crate::native::bootstrap::DialFailure;
use crate::native::bootstrap::ProcessorPort;
use crate::native::bootstrap::ReachabilityEvidence;
use crate::seed::SeedPeer;
use crate::seed::ValidatedSeedPeer;

/// Draws of random identity keys before giving up on a chain layout.
const CHAIN_KEY_DRAWS: usize = 512;
/// An endpoint that validates as public and is never dialed.
const NEVER_DIALED: &str = "https://never-dialed.example.org:50001/";

/// Swarm callback that dispatches through the real [`Backend`] (so lookup reports and swarm
/// events reach the reachability evidence exactly as in `rings run`) while keeping the test
/// fixture's connection notifications.
struct ProbeTestCallback {
    backend: Backend,
    fixture: Arc<SwarmCallbackInstance>,
}

#[async_trait]
impl SwarmCallback for ProbeTestCallback {
    /// Route inbound payloads through the backend, then record them in the fixture.
    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), rings_core::error::CallbackError> {
        self.backend.on_inbound(payload).await?;
        self.fixture.on_inbound(payload).await
    }

    /// Forward events to both the backend (which writes the evidence) and the fixture's
    /// connection notifier.
    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), rings_core::error::CallbackError> {
        self.backend.on_event(event).await?;
        self.fixture.on_event(event).await
    }
}

/// One node of the probe topology.
struct ProbeNode {
    processor: Arc<Processor>,
    evidence: Arc<ReachabilityEvidence>,
    fixture: Arc<SwarmCallbackInstance>,
}

impl ProbeNode {
    /// A node over `key` whose swarm callback dispatches through a backend that writes fresh
    /// reachability evidence.
    async fn new(key: SecretKey) -> Self {
        let processor = Arc::new(prepare_processor_with_identity_key(key).await);
        let evidence = Arc::new(ReachabilityEvidence::default());
        let fixture = test_callback();
        let provider = Arc::new(Provider::from_processor(processor.clone()));
        let backend = Backend::new(provider).observed_by(evidence.clone());
        processor
            .swarm
            .set_callback(Arc::new(ProbeTestCallback {
                backend,
                fixture: fixture.clone(),
            }))
            .expect("callback installs");
        Self {
            processor,
            evidence,
            fixture,
        }
    }

    /// DID of this node.
    fn did(&self) -> Did {
        self.processor.did()
    }
}

/// Three identity keys whose DIDs satisfy `A - B < C - B` (clockwise ring distance, the `Sub`
/// on `Did`), so `A` is `B`'s successor head once both are connected to `B`.
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
#[tokio::test]
async fn routed_probe_reports_presence_through_one_hop() {
    let _guard = network_test_guard().await;
    let (a_key, b_key, c_key) = chain_keys();
    let a = ProbeNode::new(a_key).await;
    let b = ProbeNode::new(b_key).await;
    let c = ProbeNode::new(c_key).await;

    connect_processors(&b.processor, &a.processor, &b.fixture, &a.fixture).await;
    // The admission of B is what makes B reachable to C without a lookup, so the test waits
    // for the event, not merely for transport readiness.
    let b_admitted = c
        .evidence
        .admissions()
        .wait_for(b.did())
        .expect("record readable");
    connect_processors(&c.processor, &b.processor, &c.fixture, &b.fixture).await;
    assert_eq!(
        b_admitted.await,
        Ok(()),
        "the admission of B is announced to C"
    );

    let port = ProcessorPort::new(c.processor.clone(), c.evidence.clone());
    assert!(
        port.reachable(&target(b.did())).await,
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
        port.reachable(&target(a.did())).await,
        "a target one hop away answers as its own successor"
    );
    let absent = b.did() + Did::from(1);
    assert_ne!(absent, a.did());
    assert_ne!(absent, c.did());
    assert!(
        !port.reachable(&target(absent)).await,
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
        !port.reachable(&target(own_interval)).await,
        "a locally decided lookup is a refutation"
    );
}

/// The backend translates swarm events into evidence: `Connected` resolves an admission
/// waiter, `PeerRetired` records a loss, and other physical states record nothing.
#[tokio::test]
async fn backend_translates_admission_and_retirement_only() {
    let processor = Arc::new(prepare_processor().await);
    let managed = Did::from(1);
    let evidence = Arc::new(ReachabilityEvidence::default());
    let provider = Arc::new(Provider::from_processor(processor));
    let backend = Backend::new(provider).observed_by(evidence.clone());
    let state_change = |state: WebrtcConnectionState| SwarmEvent::ConnectionStateChange {
        peer: managed,
        state,
    };

    let mut admitted = evidence
        .admissions()
        .wait_for(managed)
        .expect("record readable");
    for state in [
        WebrtcConnectionState::Connecting,
        WebrtcConnectionState::Disconnected,
        WebrtcConnectionState::Failed,
        WebrtcConnectionState::Closed,
    ] {
        backend
            .on_event(&state_change(state))
            .await
            .expect("events are accepted");
    }
    assert!(evidence
        .losses()
        .take()
        .expect("record readable")
        .is_empty());
    assert!(
        matches!(admitted.try_recv(), Err(TryRecvError::Empty)),
        "no physical state other than Connected admits"
    );

    backend
        .on_event(&state_change(WebrtcConnectionState::Connected))
        .await
        .expect("events are accepted");
    assert_eq!(admitted.await, Ok(()));

    backend
        .on_event(&SwarmEvent::PeerRetired { peer: managed })
        .await
        .expect("events are accepted");
    assert_eq!(
        evidence.losses().take().expect("record readable"),
        [managed].into_iter().collect()
    );
}

/// A disconnect decided by the local node retires the peer through the core's single
/// retirement transition and reaches the evidence as a loss, without any physical terminal
/// state. The disconnect follows the admission *event*, not merely transport readiness: by the
/// retirement law a retirement before the admission was announced is silent.
#[tokio::test]
async fn a_local_disconnect_is_reported_as_a_peer_retirement() {
    let _guard = network_test_guard().await;
    let a = ProbeNode::new(SecretKey::random()).await;
    let b = ProbeNode::new(SecretKey::random()).await;
    let admitted = a
        .evidence
        .admissions()
        .wait_for(b.did())
        .expect("record readable");
    connect_processors(&a.processor, &b.processor, &a.fixture, &b.fixture).await;
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
    assert_eq!(
        a.evidence.losses().take().expect("record readable"),
        [b.did()].into_iter().collect(),
        "the retirement is observed before disconnect returns"
    );
}

/// A pending handshake to the target, here one this node reserved by offering, makes a dial a
/// deferral before any request leaves: the never-dialed endpoint would otherwise fail to
/// resolve.
#[tokio::test]
async fn a_pending_handshake_defers_the_dial_without_a_request() {
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
        .cancel_connection_attempt(attempt)
        .await
        .expect("records readable"));
}
