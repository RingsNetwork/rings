use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use futures::lock::Mutex;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio::time::Duration;

use crate::delegation::DelegateeKey;
use crate::dht::entry::Entry;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::StorageKey;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::measure::MeasureImpl;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::storage::MemStorage;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::callback::SwarmEvent;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::activity::activity_after;
use crate::tests::activity::activity_mark;
use crate::tests::activity::probe_on_activity;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::activity::swarm_in_flight;
use crate::tests::activity::swarms_quiescent;
use crate::tests::activity::ActivityCallback;
use crate::tests::activity::ActivityObserver;

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
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_decode_boundaries;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_inbox;
mod test_message_handler;
#[cfg(all(feature = "std", not(feature = "dummy")))]
mod test_native_transport;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_outbound_scheduler;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_session_link;
mod test_stabilization;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_stabilization_failover;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_sync_storm;

const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;
/// ICE servers of every in-process test node: none, so peers gather host candidates only.
///
/// Every peer of these tests runs in this process, so host candidates connect them; an
/// external STUN server would only add a network dependency whose latency no test controls.
pub(crate) const TEST_ICE_SERVERS: &str = "";

/// Hang guard of every awaited test state: a failure bound that names the missing state,
/// never the condition a passing run waits for (that is always an observed state change).
///
/// Dummy builds deliver in memory. Native builds run real webrtc-rs handshakes, whose latency
/// under suite load has exceeded 5 s (#850); since no wait is paced by this bound any more, it
/// only has to exceed any plausible latency, so it is the former quiescence ceiling.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) const TEST_HANG_GUARD: Duration = Duration::from_secs(5);
#[cfg(not(all(feature = "dummy", not(target_family = "wasm"))))]
pub(crate) const TEST_HANG_GUARD: Duration = Duration::from_secs(60);

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

/// Callback of a test node: records every validated message in the node's inbox, and counts and
/// records activity through [`ActivityCallback`].
pub struct NodeCallback {
    message_tx: mpsc::UnboundedSender<MessagePayload>,
    activity: ActivityCallback,
}

impl Node {
    /// Build a test node from `builder`, with its activity observer and recording callback.
    pub fn build(builder: SwarmBuilder) -> Self {
        let swarm = Arc::new(builder.observer(Arc::new(ActivityObserver)).build());
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        let callback = NodeCallback {
            message_tx,
            activity: ActivityCallback,
        };
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

    /// Whether this node has work in flight; see [`swarm_in_flight`].
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub fn in_flight(&self) -> bool {
        swarm_in_flight(&self.swarm)
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
        self.activity.on_validate(payload).await
    }

    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.activity.on_inbound(payload).await
    }

    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.activity.on_event(event).await
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
    prepare_node_with_ice_servers_and_measure(key, TEST_ICE_SERVERS, measure)
}

/// Builds a node with explicit ICE servers and optional measurement recording.
fn prepare_node_with_ice_servers_and_measure(
    key: SecretKey,
    ice_servers: &str,
    measure: Option<MeasureImpl>,
) -> Result<Node> {
    let storage = Box::new(MemStorage::new());

    let delegatee_key = DelegateeKey::new_with_seckey(&key)?;
    let builder = SwarmBuilder::new(
        crate::tests::TEST_NETWORK_ID,
        ice_servers,
        storage,
        delegatee_key,
    )
    .dht_finger_table_size(TEST_DHT_FINGER_TABLE_SIZE)
    .dht_virtual_nodes(0);
    let builder = match measure {
        Some(measure) => builder.measure(measure),
        None => builder,
    };
    let node = Node::build(builder);

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", node.did());

    Ok(node)
}

/// Wait until `ready` holds, re-probing it on every observed activity; see
/// [`probe_on_activity`].
pub async fn wait_until_result(
    label: &str,
    mut ready: impl FnMut() -> crate::error::Result<bool>,
) -> crate::error::Result<()> {
    probe_on_activity(label, TEST_HANG_GUARD, || {
        let reached = ready();
        async move { reached.map(|reached| reached.then_some(())) }
    })
    .await
}

/// Whether `node` holds an active, routable connection to `peer` in `state`.
pub fn has_connection_in_state(node: &Node, peer: Did, state: WebrtcConnectionState) -> bool {
    node.swarm
        .transport
        .get_connection(peer)
        .is_some_and(|conn| conn.webrtc_connection_state() == state)
}

pub async fn wait_for_connection_state(
    node: &Node,
    peer: Did,
    state: WebrtcConnectionState,
) -> crate::error::Result<()> {
    wait_until_result("connection reaches expected state", || {
        Ok(has_connection_in_state(node, peer, state))
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

/// Wait until the value `node` stores at `key` (or its absence) satisfies `ready`, returning
/// that value.
///
/// Post: returns only after `ready` held. Storage changes on a node follow a handled message,
/// whose `on_inbound` records activity, so every change is probed.
pub async fn wait_for_storage_state(
    node: &Node,
    key: StorageKey,
    label: &str,
    ready: impl Fn(Option<&Entry>) -> bool,
) -> crate::error::Result<Option<Entry>> {
    let ready = &ready;
    probe_on_activity(
        &format!("storage at {key}: {label}"),
        TEST_HANG_GUARD,
        || async move {
            let stored = node.dht().storage.get(&key.to_string()).await?;
            Ok(ready(stored.as_ref()).then_some(stored))
        },
    )
    .await
}

/// Wait until `node` no longer stores a value at `key`.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub async fn wait_for_storage_absence(node: &Node, key: StorageKey) -> crate::error::Result<()> {
    wait_for_storage_state(node, key, "absent", |stored| stored.is_none())
        .await
        .map(drop)
}

pub async fn wait_for_storage_entry(node: &Node, key: StorageKey) -> crate::error::Result<Entry> {
    wait_for_storage_state(node, key, "present", |stored| stored.is_some())
        .await?
        .ok_or_else(|| crate::error::Error::InvalidMessage(format!("no entry at {key}")))
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

/// Wait until `nodes` are quiescent (see [`nodes_quiescent`]), draining and logging every
/// message they receive meanwhile.
///
/// The quiescence predicate is re-probed on every observed activity; no step waits for a
/// duration. Every in-flight message is visible to the predicate: as a handshake, an inbound or
/// outbound transfer, a queued dummy event, or a conservation deficit, so the predicate cannot
/// hold while a message is still on its way. The hang guard is only a failure bound.
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
                    did_name_or_default(&did_names, payload.signer()),
                    did_name_or_default(&did_names, node.did()),
                    did_name_or_default(&did_names, payload.transaction.signer()),
                    did_name_or_default(&did_names, payload.transaction.destination),
                    payload.transaction.data::<Message>().unwrap()
                );
            }
        }
        drained
    };
    let reached = timeout(TEST_HANG_GUARD, async {
        loop {
            let mark = activity_mark();
            if drain().await {
                continue;
            }
            if nodes_quiescent(&nodes) {
                return;
            }
            activity_after(mark).await;
        }
    })
    .await;
    if reached.is_err() {
        panic_wait_for_msgs_timeout(&nodes, &did_names, TEST_HANG_GUARD);
    }
}

/// Whether `nodes` are quiescent (see [`swarms_quiescent`]) and, in dummy builds, no event
/// waits in this thread's controlled delivery queue.
fn nodes_quiescent(nodes: &[&Node]) -> bool {
    pending_transport_events() == 0
        && swarms_quiescent(nodes.iter().map(|node| node.swarm.as_ref()))
}

fn did_name_or_default(did_names: &DashMap<Did, String>, did: Did) -> String {
    did_names
        .get(&did)
        .map(|name| name.clone())
        .unwrap_or_default()
}

fn active_node_counts(
    nodes: &[&Node],
    did_names: &DashMap<Did, String>,
    count: impl Fn(&Node) -> usize,
) -> Vec<(String, usize)> {
    nodes
        .iter()
        .filter_map(|node| {
            let admitted = count(node);
            (admitted > 0).then(|| (did_name_or_default(did_names, node.did()), admitted))
        })
        .collect()
}

fn panic_wait_for_msgs_timeout(
    nodes: &[&Node],
    did_names: &DashMap<Did, String>,
    ceiling: Duration,
) -> ! {
    let handshaking_nodes: Vec<String> = nodes
        .iter()
        .filter(|node| node.has_handshaking_connection())
        .map(|node| did_name_or_default(did_names, node.did()))
        .collect();
    let outbound_nodes = active_node_counts(nodes, did_names, |node| {
        node.swarm
            .transport
            .outbound_admitted_transfer_total_for_test()
    });
    let inbound_nodes = active_node_counts(nodes, did_names, |node| {
        node.swarm.transport.inbound_admitted_count_for_test()
    });
    let sent: u64 = nodes
        .iter()
        .map(|node| node.swarm.transport.frames_for_test().sent())
        .sum();
    let arrived: u64 = nodes
        .iter()
        .map(|node| node.swarm.transport.frames_for_test().arrived())
        .sum();
    panic!(
        "wait_for_msgs did not reach quiescence within {ceiling:?}: still-handshaking \
         nodes={handshaking_nodes:?}, inbound={inbound_nodes:?}, outbound={outbound_nodes:?}, \
         transport-pending={}, sent={sent}, arrived={arrived}",
        pending_transport_events()
    );
}

/// Events queued on this thread's controlled dummy transport and not yet delivered.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn pending_transport_events() -> usize {
    rings_transport::connections::dummy_controlled::pending()
}

/// Events queued on a controlled dummy transport: none outside dummy builds.
#[cfg(not(all(feature = "dummy", not(target_family = "wasm"))))]
const fn pending_transport_events() -> usize {
    0
}
