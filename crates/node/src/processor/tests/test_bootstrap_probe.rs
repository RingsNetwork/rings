//! Real-network check of the bootstrap reachability probe: the routed successor lookup issued
//! by `ProcessorPort::reachable` reports a present target as its own successor, reports a
//! different node for an absent key, is short-circuited by a direct edge, and is decided
//! locally for a key in this node's own range.
//!
//! Topology: `C — B — A`, with keys chosen so that `A` is `B`'s successor head (clockwise from
//! `B`, `A` precedes `C`). `C`'s only peer is `B`, so every lookup `C` issues goes to `B`, which
//! answers `A` for any key in `(B, A]`.

use std::sync::Arc;

use rings_core::ecc::SecretKey;

use super::common::*;
use super::*;
use crate::extension::Backend;
use crate::native::bootstrap::BootstrapConfig;
use crate::native::bootstrap::BootstrapPort;
use crate::native::bootstrap::BootstrapTargets;
use crate::native::bootstrap::ManagedTarget;
use crate::native::bootstrap::ProcessorPort;
use crate::native::bootstrap::ReachabilityEvidence;
use crate::seed::SeedPeer;

/// Swarm callback that dispatches through the real [`Backend`] (so lookup reports reach the
/// reachability evidence exactly as in `rings run`) while keeping the test fixture's
/// connection notifications.
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

    /// Forward events to both the backend (which records transport losses) and the fixture's
    /// connection notifier.
    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), rings_core::error::CallbackError> {
        self.backend.on_event(event).await?;
        self.fixture.on_event(event).await
    }
}

/// Clockwise ring distance from `from` to `to`.
fn clockwise(from: Did, to: Did) -> Did {
    to - from
}

/// Three identity keys whose DIDs satisfy `clockwise(B, A) < clockwise(B, C)`, so `A` is `B`'s
/// successor head once both are connected to `B`.
fn chain_keys() -> (SecretKey, SecretKey, SecretKey) {
    loop {
        let a = SecretKey::random();
        let b = SecretKey::random();
        let c = SecretKey::random();
        let (a_did, b_did, c_did) = (
            Did::from(a.address()),
            Did::from(b.address()),
            Did::from(c.address()),
        );
        if clockwise(b_did, a_did) < clockwise(b_did, c_did) {
            return (a, b, c);
        }
    }
}

/// A managed target for `did` with a syntactically valid endpoint that is never dialed.
fn target(did: Did) -> ManagedTarget {
    ManagedTarget::try_from(&SeedPeer {
        did: did.to_string(),
        url: "https://never-dialed.example.org:50001/".to_string(),
        api_token: None,
    })
    .expect("a public endpoint validates")
}

/// A processor over `key` whose swarm callback dispatches through a backend that writes
/// fresh reachability evidence.
async fn probe_processor(
    key: SecretKey,
) -> (
    Arc<Processor>,
    Arc<ReachabilityEvidence>,
    Arc<SwarmCallbackInstance>,
) {
    let processor = Arc::new(prepare_processor_with_identity_key(key).await);
    let evidence = Arc::new(ReachabilityEvidence::new(&BootstrapTargets::default()));
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
    (processor, evidence, fixture)
}

/// The backend translates connection state changes into transport-neutral losses: only a
/// terminal state of a managed target is recorded, never a transient `Disconnected`.
#[tokio::test]
async fn backend_records_only_terminal_states_of_managed_targets() {
    let processor = Arc::new(prepare_processor().await);
    let managed = Did::from(1);
    let targets = BootstrapTargets::from_config(
        &BootstrapConfig {
            peers: vec![SeedPeer {
                did: managed.to_string(),
                url: "https://never-dialed.example.org:50001/".to_string(),
                api_token: None,
            }],
        },
        processor.did(),
    )
    .expect("one public target validates");
    let evidence = Arc::new(ReachabilityEvidence::new(&targets));
    let provider = Arc::new(Provider::from_processor(processor));
    let backend = Backend::new(provider).observed_by(evidence.clone());
    let event =
        |peer: Did, state: WebrtcConnectionState| SwarmEvent::ConnectionStateChange { peer, state };

    for state in [
        WebrtcConnectionState::Connecting,
        WebrtcConnectionState::Connected,
        WebrtcConnectionState::Disconnected,
    ] {
        backend
            .on_event(&event(managed, state))
            .await
            .expect("events are accepted");
    }
    backend
        .on_event(&event(Did::from(2), WebrtcConnectionState::Closed))
        .await
        .expect("events are accepted");
    assert!(evidence.drops().take().expect("drops readable").is_empty());

    backend
        .on_event(&event(managed, WebrtcConnectionState::Failed))
        .await
        .expect("events are accepted");
    assert_eq!(
        evidence.drops().take().expect("drops readable"),
        [managed].into_iter().collect()
    );
}

/// Present targets are reachable through one hop, an absent key is not, a direct peer needs
/// no lookup at all, and a key in `C`'s own range is refuted without leaving `C`.
#[tokio::test]
async fn routed_probe_reports_presence_through_one_hop() {
    let _guard = network_test_guard().await;
    let (a_key, b_key, c_key) = chain_keys();
    let (a, _, a_fixture) = probe_processor(a_key).await;
    let (b, _, b_fixture) = probe_processor(b_key).await;
    let (c, c_evidence, c_fixture) = probe_processor(c_key).await;

    connect_processors(&b, &a, &b_fixture, &a_fixture).await;
    connect_processors(&c, &b, &c_fixture, &b_fixture).await;

    let port = ProcessorPort::new(c.clone(), c_evidence.reports());
    assert!(
        port.reachable(&target(b.did())).await,
        "a directly connected target is reachable without a lookup"
    );
    assert!(
        port.reachable(&target(a.did())).await,
        "a target one hop away answers as its own successor"
    );
    let absent = b.did() + Did::from(1);
    assert_ne!(absent, a.did());
    assert!(
        !port.reachable(&target(absent)).await,
        "an absent key is answered by another node's successor"
    );
    let own_range = c.did() + Did::from(1);
    assert_ne!(own_range, b.did());
    let before = c_evidence.reports().len().expect("ledger readable");
    assert!(
        !port.reachable(&target(own_range)).await,
        "a key succeeded by C's own successor is refuted locally"
    );
    assert_eq!(
        c_evidence.reports().len().expect("ledger readable"),
        before,
        "the local refutation registers no probe"
    );
}
