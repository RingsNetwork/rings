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
#[cfg(feature = "dummy")]
use std::sync::Mutex;

use rings_core::ecc::SecretKey;
#[cfg(feature = "dummy")]
use rings_core::message::Encoded;
#[cfg(feature = "dummy")]
use rings_core::message::Encoder;
#[cfg(feature = "dummy")]
use rings_core::message::MessagePayload;
#[cfg(feature = "dummy")]
use rings_core::swarm::SuccessorLookup;
#[cfg(feature = "dummy")]
use rings_rpc::protos::rings_node::AnswerOfferRequest;
#[cfg(feature = "dummy")]
use rings_rpc::protos::rings_node::NodeDidRequest;
#[cfg(feature = "dummy")]
use rings_rpc::protos::rings_node::NodeDidResponse;
#[cfg(feature = "dummy")]
use rings_rpc::protos::rings_node_handler::HandleRpc;

use super::common::*;
use super::*;
#[cfg(feature = "dummy")]
use crate::error::Error;
#[cfg(feature = "dummy")]
use crate::error::Result;
use crate::extension::ext::LinkFact;
use crate::extension::BackendObserver;
use crate::native::bootstrap::BootstrapPort;
#[cfg(feature = "dummy")]
use crate::native::bootstrap::BootstrapSupervisor;
#[cfg(feature = "dummy")]
use crate::native::bootstrap::BootstrapTargets;
use crate::native::bootstrap::DialFailure;
use crate::native::bootstrap::ProcessorPort;
use crate::native::bootstrap::ReachabilityEvidence;
#[cfg(feature = "dummy")]
use crate::native::config::BootstrapConfig;
#[cfg(feature = "dummy")]
use crate::prelude::StopSource;
#[cfg(feature = "dummy")]
use crate::processor::handshake::decode_handshake_payload;
#[cfg(feature = "dummy")]
use crate::processor::handshake::RemoteHandshake;
use crate::seed::SeedPeer;
use crate::seed::ValidatedSeedPeer;
#[cfg(feature = "dummy")]
use crate::test_support::TestTask;
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

/// Shared in-memory replay store that persists stream sequences across [`RestartableSeed`]
/// restarts, matching the on-disk replay store of a restarting native seed daemon.
#[cfg(feature = "dummy")]
#[derive(Clone)]
struct SharedReplayStorage(Arc<MemStorage<rings_core::message::ReplayRecord>>);

#[cfg(feature = "dummy")]
#[async_trait::async_trait]
impl rings_core::storage::KvStorageInterface<rings_core::message::ReplayRecord>
    for SharedReplayStorage
{
    async fn get(
        &self,
        key: &str,
    ) -> rings_core::error::Result<Option<rings_core::message::ReplayRecord>> {
        self.0.get(key).await
    }

    async fn put(
        &self,
        key: &str,
        value: &rings_core::message::ReplayRecord,
    ) -> rings_core::error::Result<()> {
        self.0.put(key, value).await
    }

    async fn get_all(
        &self,
    ) -> rings_core::error::Result<Vec<(String, rings_core::message::ReplayRecord)>> {
        self.0.get_all().await
    }

    async fn remove(&self, key: &str) -> rings_core::error::Result<()> {
        self.0.remove(key).await
    }

    async fn clear(&self) -> rings_core::error::Result<()> {
        self.0.clear().await
    }

    async fn count(&self) -> rings_core::error::Result<u32> {
        self.0.count().await
    }
}

#[cfg(feature = "dummy")]
#[async_trait::async_trait]
impl rings_core::storage::KvStorageScan<rings_core::message::ReplayRecord> for SharedReplayStorage {
    async fn scan(
        &self,
    ) -> rings_core::error::Result<
        Vec<rings_core::storage::ScannedRecord<rings_core::message::ReplayRecord>>,
    > {
        self.0.scan().await
    }

    fn record_name(&self, key: &str) -> String {
        self.0.record_name(key)
    }

    async fn put_batch(
        &self,
        entries: &[(String, rings_core::message::ReplayRecord)],
    ) -> rings_core::error::Result<()> {
        self.0.put_batch(entries).await
    }

    async fn remove_batch(&self, keys: &[String]) -> rings_core::error::Result<()> {
        self.0.remove_batch(keys).await
    }
}

/// Public HTTP handshake endpoint of an external seed that can be restarted in place under the
/// same identity key and replay store (same DID and persisted stream sequences, fresh delegatee
/// key and swarm).
#[cfg(feature = "dummy")]
struct RestartableSeed {
    seed_key: SecretKey,
    replay_storage: SharedReplayStorage,
    current: Mutex<Arc<Processor>>,
}

#[cfg(feature = "dummy")]
impl RestartableSeed {
    /// Start the initial seed instance for `seed_key`.
    async fn new(seed_key: SecretKey) -> Self {
        let replay_storage = SharedReplayStorage(Arc::new(MemStorage::new()));
        let processor = Self::spawn_instance(&seed_key, replay_storage.clone()).await;
        Self {
            seed_key,
            replay_storage,
            current: Mutex::new(processor),
        }
    }

    /// Create a seed [`Processor`] with a freshly minted [`DelegateeKey`] over `seed_key` and
    /// the seed's persistent `replay_storage`.
    async fn spawn_instance(
        seed_key: &SecretKey,
        replay_storage: SharedReplayStorage,
    ) -> Arc<Processor> {
        let config = crate::tests::test_processor_config(seed_key, 0);
        let processor = Arc::new(
            crate::tests::test_processor_builder(&config)
                .replay_storage(Box::new(replay_storage))
                .build()
                .expect("seed processor builds"),
        );
        let evidence = Arc::new(ReachabilityEvidence::default());
        Provider::from_processor(processor.clone())
            .serve_application(Some(Arc::new(ActiveEvidence(evidence))))
            .expect("the seed processor's inbox is unclaimed");
        processor
    }

    /// Stable DID of the seed across restarts.
    fn did(&self) -> Did {
        self.seed_key.address().into()
    }

    /// Current live seed [`Processor`].
    fn current(&self) -> Arc<Processor> {
        self.current
            .lock()
            .expect("seed processor lock is not poisoned")
            .clone()
    }

    /// Replace the running seed with a freshly booted instance of the same identity key and
    /// close the previous instance's transports, without restarting any cluster node.
    ///
    /// `DummyConnection::close` fences only the closing endpoint, so each cluster node that held
    /// a dummy edge to the retiring seed also disconnects that dead edge to deliver the remote
    /// transport-close retirement that WebRTC/SCTP delivers over the wire.
    async fn restart(&self, cluster: &[ProbeNode]) -> Arc<Processor> {
        let seed_did = self.did();
        let next = Self::spawn_instance(&self.seed_key, self.replay_storage.clone()).await;
        let previous = {
            let mut guard = self
                .current
                .lock()
                .expect("seed processor lock is not poisoned");
            std::mem::replace(&mut *guard, next.clone())
        };
        previous.close().await.expect("previous seed closes");
        for node in cluster {
            if processor_has_admitted_peer(&node.processor, seed_did) {
                node.processor
                    .disconnect(seed_did)
                    .await
                    .expect("remote transport close retires the dead seed generation");
            }
        }
        next
    }
}

#[cfg(feature = "dummy")]
#[async_trait::async_trait]
impl RemoteHandshake for RestartableSeed {
    async fn node_did(&self) -> Result<NodeDidResponse> {
        self.current()
            .handle_rpc(NodeDidRequest {})
            .await
            .map_err(Error::InternalRpcError)
    }

    async fn answer_offer(&self, offer: &MessagePayload) -> Result<MessagePayload> {
        let offer = offer.encode().map_err(|_| Error::EncodeError)?;
        let reply = self
            .current()
            .handle_rpc(AnswerOfferRequest {
                offer: offer.to_string(),
            })
            .await
            .map_err(Error::InternalRpcError)?;
        decode_handshake_payload(&Encoded::from(reply.answer))
    }

    async fn accept_answer(&self, _answer: &MessagePayload) -> Result<()> {
        Err(Error::RemoteRpcError("401 Unauthorized".to_string()))
    }
}

/// Production [`ProcessorPort`] whose HTTP handshake reaches the in-process [`RestartableSeed`].
#[cfg(feature = "dummy")]
struct SeededProcessorPort {
    inner: ProcessorPort,
    seed: Arc<RestartableSeed>,
}

#[cfg(feature = "dummy")]
#[async_trait::async_trait]
impl BootstrapPort for SeededProcessorPort {
    async fn reachable(&self, target: Did) -> bool {
        self.inner.reachable(target).await
    }

    async fn dial(&self, target: &ValidatedSeedPeer) -> std::result::Result<(), DialFailure> {
        self.inner.dial_over(target.did(), self.seed.as_ref()).await
    }
}

/// Whether every node in `cluster` and `seed` mutually list each other in state `Connected`.
#[cfg(feature = "dummy")]
fn all_cluster_nodes_connected_to_seed(cluster: &[ProbeNode], seed: &Processor) -> bool {
    let seed_did = seed.did().to_string();
    cluster.iter().all(|node| {
        let node_did = node.did().to_string();
        node.processor
            .swarm
            .peers()
            .iter()
            .any(|peer| peer.did == seed_did && peer.state == "Connected")
            && seed
                .swarm
                .peers()
                .iter()
                .any(|peer| peer.did == node_did && peer.state == "Connected")
    })
}

/// Acceptance test for #1154: a 3-node ring cluster pointed at an external seed through
/// `bootstrap.peers` (`BootstrapSupervisor`, with no one-shot connect) connects to the seed, and
/// after the seed restarts with a fresh delegatee key, every cluster node is `Connected` to the
/// restarted seed again within the supervision's redial bound, with no cluster restart.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_cluster_reconnects_to_restarted_seed_through_bootstrap_peers() {
    let _guard = network_test_guard().await;
    let seed = Arc::new(RestartableSeed::new(SecretKey::random()).await);
    let seed_did = seed.did();
    let cluster = [
        ProbeNode::new(SecretKey::random()).await,
        ProbeNode::new(SecretKey::random()).await,
        ProbeNode::new(SecretKey::random()).await,
    ];

    // Connect the cluster in the default `ring` topology (`0 -> 1 -> 2 -> 0`), with no one-shot
    // connect to the external seed.
    let node_count = cluster.len();
    for index in 0..node_count {
        let source = cluster.get(index).expect("source node exists");
        let target = cluster
            .get((index + 1) % node_count)
            .expect("target node exists");
        connect_processors(&source.processor, &target.processor).await;
    }

    let stop = StopSource::new();
    let mut supervisor_tasks = Vec::with_capacity(node_count);
    for (index, node) in cluster.iter().enumerate() {
        let targets = BootstrapTargets::from_config(
            BootstrapConfig {
                peers: vec![never_dialed(seed_did)],
            },
            node.did(),
        )
        .expect("seed bootstrap target validates");
        let port = Arc::new(SeededProcessorPort {
            inner: ProcessorPort::new(node.processor.clone(), node.evidence.clone()),
            seed: seed.clone(),
        });
        let supervisor = BootstrapSupervisor::new(
            targets,
            port,
            node.evidence.clone(),
            u64::try_from(index).unwrap_or(0),
        );
        supervisor_tasks.push(TestTask::spawn(supervisor.run(stop.token())));
    }

    let initial_seed = seed.current();
    probe_on_activity(
        "every cluster node connects to the initial seed through BootstrapSupervisor",
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let ready = all_cluster_nodes_connected_to_seed(&cluster, initial_seed.as_ref());
            async move { Ok(ready.then_some(())) }
        },
    )
    .await
    .expect("cluster connects to the initial seed within the supervision bound");

    // Restart the external seed in place (closing the old instance and replacing it with a
    // fresh instance under the same DID) without restarting any cluster node or supervisor.
    let restarted_seed = seed.restart(&cluster).await;
    probe_on_activity(
        "every cluster node reconnects to the restarted seed within the redial bound",
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let ready = all_cluster_nodes_connected_to_seed(&cluster, restarted_seed.as_ref());
            async move { Ok(ready.then_some(())) }
        },
    )
    .await
    .expect("every cluster node is Connected to the restarted seed within the redial bound");

    stop.request_stop();
    for task in supervisor_tasks {
        let _ = task.await;
    }
    restarted_seed.close().await.expect("restarted seed closes");
    for node in &cluster {
        node.processor.close().await.expect("cluster node closes");
    }
}
