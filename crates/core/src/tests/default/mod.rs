use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use dashmap::DashMap;
use futures::lock::Mutex;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio::time::Duration;

use crate::dht::entry::Entry;
use crate::dht::successor::SuccessorReader;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::measure::MeasureImpl;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

mod test_dht_convergence;
// Uses the `stateright` model checker, which doesn't build for wasm32.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) mod dummy_hooks;
#[cfg(not(target_family = "wasm"))]
mod test_dht_stateright;
mod test_dht_trace_replay;
// Drives the dummy transport's controlled delivery queue (dummy-only).
mod test_connection;
#[cfg(feature = "dummy")]
mod test_dht_schedule;
// End-to-end chunking uses the dummy backend's `max_message_size` test hook.
#[cfg(feature = "dummy")]
mod test_chunk_e2e;
mod test_message_handler;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_outbound_scheduler;
mod test_stabilization;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_stabilization_failover;

const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;
const TEST_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const TEST_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(5);
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) const TEST_NETWORK_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(all(feature = "dummy", not(target_family = "wasm"))))]
pub(crate) const TEST_NETWORK_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Node {
    pub swarm: Arc<Swarm>,
    inbox: Mutex<NodeInbox>,
}

struct NodeInbox {
    buffered: VecDeque<MessagePayload>,
    receiver: mpsc::UnboundedReceiver<MessagePayload>,
}

pub(crate) struct NodeMessageScan<'a> {
    inbox: futures::lock::MutexGuard<'a, NodeInbox>,
    skipped: Vec<MessagePayload>,
}

pub struct NodeCallback {
    message_tx: mpsc::UnboundedSender<MessagePayload>,
}

impl Node {
    pub fn new(swarm: Arc<Swarm>) -> Self {
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        let callback = NodeCallback { message_tx };
        swarm.set_callback(Arc::new(callback)).unwrap();
        Self {
            swarm,
            inbox: Mutex::new(NodeInbox {
                buffered: VecDeque::new(),
                receiver: message_rx,
            }),
        }
    }

    pub async fn listen_once(&self) -> Option<MessagePayload> {
        self.message_scan().await.next().await
    }

    /// Non-blocking variant: pop a buffered message if one is immediately available, else `None`.
    pub async fn try_listen_once(&self) -> Option<MessagePayload> {
        let mut inbox = self.inbox.lock().await;
        match inbox.buffered.pop_front() {
            Some(payload) => Some(payload),
            None => inbox.receiver.try_recv().ok(),
        }
    }

    pub(crate) async fn message_scan(&self) -> NodeMessageScan<'_> {
        NodeMessageScan {
            inbox: self.inbox.lock().await,
            skipped: Vec::new(),
        }
    }

    /// Seed the front of this test node's inbox without changing message order.
    pub(crate) async fn prepend_messages_for_test(&self, messages: Vec<MessagePayload>) {
        let mut inbox = self.inbox.lock().await;
        for payload in messages.into_iter().rev() {
            inbox.buffered.push_front(payload);
        }
    }

    /// Whether any connection is still mid-handshake. Used to detect true
    /// quiescence without a wall clock.
    pub fn has_handshaking_connection(&self) -> bool {
        self.swarm
            .transport
            .pending_connection_count()
            .unwrap_or_default()
            > 0
    }

    /// Whether a transfer is queued, sending, or waiting for delivery.
    pub fn has_outbound_transfer(&self) -> bool {
        self.swarm
            .transport
            .outbound_admitted_transfer_total_for_test()
            > 0
    }

    /// Whether an admitted inbound message is queued or still being handled.
    pub fn has_inbound_message(&self) -> bool {
        self.swarm.transport.inbound_admitted_count_for_test() > 0
    }

    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    pub fn dht(&self) -> Arc<PeerRing> {
        self.swarm.dht().clone()
    }

    pub fn assert_transports(&self, addresses: Vec<Did>) {
        println!(
            "Check transport of {:?}: {:?} for addresses {:?}",
            self.did(),
            self.swarm.transport.get_connection_ids(),
            addresses
        );
        assert_eq!(
            self.swarm.transport.get_connections().len(),
            addresses.len()
        );
        for addr in addresses {
            assert!(self.swarm.transport.get_connection(addr).is_some());
        }
    }
}

impl NodeMessageScan<'_> {
    pub(crate) async fn next(&mut self) -> Option<MessagePayload> {
        match self.inbox.buffered.pop_front() {
            Some(payload) => Some(payload),
            None => self.inbox.receiver.recv().await,
        }
    }

    pub(crate) fn skip(&mut self, payload: MessagePayload) {
        self.skipped.push(payload);
    }

    pub(crate) fn skipped(&self) -> &[MessagePayload] {
        &self.skipped
    }
}

impl Drop for NodeMessageScan<'_> {
    fn drop(&mut self) {
        for payload in self.skipped.drain(..).rev() {
            self.inbox.buffered.push_front(payload);
        }
    }
}

#[async_trait]
impl SwarmCallback for NodeCallback {
    async fn on_validate(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        // Here we are using on_validate to record messages.
        // When on_validate return error, the message will be ignored, which is not on purpose.
        // To prevent returning errors when sending fails, we choose to panic instead.
        self.message_tx.send(payload.clone()).unwrap();
        Ok(())
    }
}

pub async fn prepare_node(key: SecretKey) -> Node {
    prepare_node_with_optional_measure(key, None).unwrap()
}

pub(super) fn prepare_node_with_measure(key: SecretKey, measure: MeasureImpl) -> Result<Node> {
    prepare_node_with_optional_measure(key, Some(measure))
}

fn prepare_node_with_optional_measure(
    key: SecretKey,
    measure: Option<MeasureImpl>,
) -> Result<Node> {
    let stun = "stun://stun.l.google.com:19302";
    let storage = Box::new(MemStorage::new());

    let session_sk = SessionSk::new_with_seckey(&key)?;
    let builder = SwarmBuilder::new(0, stun, storage, session_sk)
        .dht_finger_table_size(TEST_DHT_FINGER_TABLE_SIZE)
        .dht_virtual_nodes(0);
    let builder = match measure {
        Some(measure) => builder.measure(measure),
        None => builder,
    };
    let swarm = Arc::new(builder.build());

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", swarm.did());

    Ok(Node::new(swarm))
}

pub async fn wait_until_result(
    label: &str,
    mut ready: impl FnMut() -> crate::error::Result<bool>,
) -> crate::error::Result<()> {
    // Pre: `ready` observes the protocol state named by `label`.
    // Post: returns Ok only after `ready` is true; timeout is a failure deadline,
    // not the condition that makes the passing path proceed.
    let started = Instant::now();
    loop {
        if ready()? {
            return Ok(());
        }

        assert!(
            started.elapsed() <= TEST_WAIT_TIMEOUT,
            "condition did not become true within {TEST_WAIT_TIMEOUT:?}: {label}"
        );
        tokio::task::yield_now().await;
        sleep(TEST_WAIT_POLL_INTERVAL).await;
    }
}

pub async fn wait_for_connection_state(
    node: &Node,
    peer: Did,
    state: WebrtcConnectionState,
) -> crate::error::Result<()> {
    wait_until_result("connection reaches expected state", || {
        Ok(node
            .swarm
            .transport
            .get_connection(peer)
            .map(|conn| conn.webrtc_connection_state() == state)
            .unwrap_or(false))
    })
    .await
}

pub async fn wait_for_successor(node: &Node, successor: Did) -> crate::error::Result<()> {
    wait_until_result("successor list contains expected peer", || {
        Ok(node.dht().successors().list()?.contains(&successor))
    })
    .await
}

pub async fn wait_for_finger(node: &Node, peer: Did) -> crate::error::Result<()> {
    wait_until_result("finger table contains expected peer", || {
        Ok(node.dht().lock_finger()?.contains(Some(peer)))
    })
    .await
}

pub async fn wait_for_predecessor(node: &Node, predecessor: Did) -> crate::error::Result<()> {
    wait_until_result("predecessor becomes expected peer", || {
        Ok(*node.dht().lock_predecessor()? == Some(predecessor))
    })
    .await
}

pub async fn wait_for_storage_entry(node: &Node, entry: Did) -> crate::error::Result<Entry> {
    let started = Instant::now();
    loop {
        if let Some(entry) = node.dht().storage.get(&entry.to_string()).await? {
            return Ok(entry);
        }

        assert!(
            started.elapsed() <= TEST_WAIT_TIMEOUT,
            "storage entry did not appear within {TEST_WAIT_TIMEOUT:?}: {entry}"
        );
        tokio::task::yield_now().await;
        sleep(TEST_WAIT_POLL_INTERVAL).await;
    }
}

pub fn gen_pure_dht(did: Did) -> PeerRing {
    let storage = Box::new(MemStorage::new());
    PeerRing::new_with_storage(did, 3, storage)
}

pub fn gen_sorted_dht(s: usize) -> Vec<PeerRing> {
    let mut keys: Vec<crate::ecc::SecretKey> = vec![];
    for _i in 0..s {
        keys.push(crate::ecc::SecretKey::random());
    }
    keys.sort_by_key(|a| a.address());

    #[allow(clippy::needless_collect)]
    let dids: Vec<crate::dht::Did> = keys
        .iter()
        .map(|sk| crate::dht::Did::from(sk.address()))
        .collect();

    let mut iter = dids.into_iter();
    let mut ret: Vec<crate::dht::PeerRing> = vec![];
    for _ in 0..s {
        ret.push(crate::tests::default::gen_pure_dht(iter.next().unwrap()))
    }
    ret
}

pub async fn assert_no_more_msg(nodes: impl IntoIterator<Item = &Node>) {
    let nodes: Vec<&Node> = nodes.into_iter().collect();
    let did_names: DashMap<Did, String> = DashMap::new();

    for (i, node) in nodes.iter().enumerate() {
        let name = format!("node{}", i + 1);
        did_names.insert(node.did(), name);
    }

    tokio::task::yield_now().await;
    for node in nodes {
        // The quiescence proof belongs to `wait_for_msgs`. This assertion only checks that no
        // buffered application message remains after the causal wait has completed.
        if let Some(payload) = node.try_listen_once().await {
            let node_name = did_names
                .get(&node.did())
                .map(|name| name.clone())
                .unwrap_or_else(|| node.did().to_string());
            let signer_name = did_names
                .get(&payload.signer())
                .map(|name| name.clone())
                .unwrap_or_else(|| payload.signer().to_string());
            let transaction_signer_name = did_names
                .get(&payload.transaction.signer())
                .map(|name| name.clone())
                .unwrap_or_else(|| payload.transaction.signer().to_string());
            let destination_name = did_names
                .get(&payload.transaction.destination)
                .map(|name| name.clone())
                .unwrap_or_else(|| payload.transaction.destination.to_string());
            panic!(
                "{node_name} should not receive any Msg, but got Msg {signer_name} -> \
                 {node_name} [{transaction_signer_name} => {destination_name}] : {:?}",
                payload.transaction.data::<Message>()
            );
        }
    }
}

/// Wait until the nodes are quiescent, **state-driven, not on a wall clock**: every connection has
/// finished its handshake, every inbound and outbound transfer has completed, and no buffered
/// message remains.
///
/// The old version returned after a fixed 3-second silence gap, which could fire *mid-handshake* —
/// e.g. while a stabilization-triggered connection's answer SDP (`ConnectNodeReport`) was still
/// being gathered against STUN — and `assert_no_more_msg` would then catch that late message. Here a
/// connection a node initiates is created synchronously while its trigger message is handled, so it
/// is observable as `New`/`Connecting` and is waited on. The timeout is only a failure ceiling.
pub async fn wait_for_msgs(nodes: impl IntoIterator<Item = &Node>) {
    let nodes: Vec<&Node> = nodes.into_iter().collect();
    let did_names: DashMap<Did, String> = DashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        did_names.insert(node.did(), format!("node{}", i + 1));
    }

    // Drain everything immediately queued across all nodes; returns whether anything was drained.
    let drain = || async {
        let mut drained = false;
        for node in &nodes {
            while let Some(payload) = node.try_listen_once().await {
                drained = true;
                println!(
                    "Msg {} -> {} [{} => {}] : {:?}",
                    did_names
                        .get(&payload.signer())
                        .map(|n| n.clone())
                        .unwrap_or_default(),
                    did_names
                        .get(&node.did())
                        .map(|n| n.clone())
                        .unwrap_or_default(),
                    did_names
                        .get(&payload.transaction.signer())
                        .map(|n| n.clone())
                        .unwrap_or_default(),
                    did_names
                        .get(&payload.transaction.destination)
                        .map(|n| n.clone())
                        .unwrap_or_default(),
                    payload.transaction.data::<Message>().unwrap()
                );
            }
        }
        drained
    };
    let handshaking = || nodes.iter().any(|n| n.has_handshaking_connection());
    let inbound = || nodes.iter().any(|n| n.has_inbound_message());
    let outbound = || nodes.iter().any(|n| n.has_outbound_transfer());
    let transport_activity = pending_transport_snapshot;
    // A snapshot of every node's DHT. Opening the data channel fires `join_dht`, which mutates the
    // DHT and emits more messages *after* the ICE connection state reached `Connected` — so true
    // quiescence also requires the DHT to have stopped changing, not just the handshakes to be done.
    let snapshot = || {
        nodes
            .iter()
            .map(|n| crate::inspect::DHTInspect::inspect(&n.dht()))
            .collect::<Vec<_>>()
    };

    // Diagnostics + hard failure if quiescence is never reached — never silently proceed, or later
    // assertions would run against unresolved async state (the bug this helper exists to catch).
    let ceiling = TEST_NETWORK_IDLE_TIMEOUT;
    let started = std::time::Instant::now();
    loop {
        let drained = drain().await;
        let before = snapshot();
        let transport_before = transport_activity();
        if !drained && !handshaking() && !inbound() && !outbound() && transport_before.is_idle() {
            // Quiescent candidate: settle briefly, then require that across the gap nothing changed
            // — no message handed off, no handshake started, and no DHT mutation (join_dht /
            // stabilize chains). Any change means activity is still in flight; keep waiting.
            sleep(Duration::from_millis(500)).await;
            let quiet = !drain().await && !handshaking() && !inbound() && !outbound();
            let unchanged_dht = snapshot() == before;
            // This is the final synchronous observation before returning. The dummy queue is
            // thread-local, so no event can be enqueued between this snapshot and the return.
            let transport_after = transport_activity();
            let unchanged_transport =
                transport_after == transport_before && transport_after.is_idle();
            if quiet && unchanged_dht && unchanged_transport {
                return;
            }
        } else {
            sleep(Duration::from_millis(50)).await;
        }

        if started.elapsed() > ceiling {
            let handshaking_nodes: Vec<String> = nodes
                .iter()
                .filter(|n| n.has_handshaking_connection())
                .map(|n| {
                    did_names
                        .get(&n.did())
                        .map(|s| s.clone())
                        .unwrap_or_default()
                })
                .collect();
            let outbound_nodes: Vec<(String, usize)> = nodes
                .iter()
                .filter_map(|n| {
                    let admitted = n
                        .swarm
                        .transport
                        .outbound_admitted_transfer_total_for_test();
                    (admitted > 0).then(|| {
                        (
                            did_names
                                .get(&n.did())
                                .map(|s| s.clone())
                                .unwrap_or_default(),
                            admitted,
                        )
                    })
                })
                .collect();
            let inbound_nodes: Vec<(String, usize)> = nodes
                .iter()
                .filter_map(|n| {
                    let admitted = n.swarm.transport.inbound_admitted_count_for_test();
                    (admitted > 0).then(|| {
                        (
                            did_names
                                .get(&n.did())
                                .map(|s| s.clone())
                                .unwrap_or_default(),
                            admitted,
                        )
                    })
                })
                .collect();
            panic!(
                "wait_for_msgs did not reach quiescence within {ceiling:?}: still-handshaking \
                 nodes={handshaking_nodes:?}, inbound={inbound_nodes:?}, \
                 outbound={outbound_nodes:?}, transport-pending={}, last-loop drained={drained}",
                pending_transport_snapshot().pending
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingTransportSnapshot {
    pending: usize,
    generation: u64,
}

impl PendingTransportSnapshot {
    const fn is_idle(self) -> bool {
        self.pending == 0
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn pending_transport_snapshot() -> PendingTransportSnapshot {
    let snapshot = rings_transport::connections::dummy_controlled::snapshot();
    PendingTransportSnapshot {
        pending: snapshot.pending(),
        generation: snapshot.generation(),
    }
}

#[cfg(not(all(feature = "dummy", not(target_family = "wasm"))))]
const fn pending_transport_snapshot() -> PendingTransportSnapshot {
    PendingTransportSnapshot {
        pending: 0,
        generation: 0,
    }
}
