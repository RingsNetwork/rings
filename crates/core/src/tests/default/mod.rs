use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use futures::lock::Mutex;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio::time::Duration;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

mod dht_convergence;
// Uses the `stateright` model checker, which doesn't build for wasm32.
#[cfg(not(target_family = "wasm"))]
mod dht_stateright;
mod dht_trace_replay;
// Drives the dummy transport's controlled delivery queue (dummy-only).
#[cfg(feature = "dummy")]
mod dht_schedule;
mod test_connection;
// End-to-end chunking uses the dummy backend's `max_message_size` test hook.
#[cfg(feature = "dummy")]
mod test_chunk_e2e;
mod test_message_handler;
mod test_stabilization;

pub struct Node {
    pub swarm: Arc<Swarm>,
    message_rx: Mutex<mpsc::UnboundedReceiver<MessagePayload>>,
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
            message_rx: Mutex::new(message_rx),
        }
    }

    pub async fn listen_once(&self) -> Option<MessagePayload> {
        self.message_rx.lock().await.recv().await
    }

    /// Non-blocking variant: pop a buffered message if one is immediately available, else `None`.
    pub async fn try_listen_once(&self) -> Option<MessagePayload> {
        self.message_rx.lock().await.try_recv().ok()
    }

    /// Whether any connection is still mid-handshake (`New`/`Connecting`) — i.e. its offer/answer
    /// SDP exchange has not finished. Used to detect true quiescence without a wall clock.
    pub fn has_handshaking_connection(&self) -> bool {
        self.swarm.transport.get_connections().iter().any(|(_, c)| {
            matches!(
                c.webrtc_connection_state(),
                WebrtcConnectionState::New | WebrtcConnectionState::Connecting
            )
        })
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

#[async_trait]
impl SwarmCallback for NodeCallback {
    async fn on_validate(
        &self,
        payload: &MessagePayload,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Here we are using on_validate to record messages.
        // When on_validate return error, the message will be ignored, which is not on purpose.
        // To prevent returning errors when sending fails, we choose to panic instead.
        self.message_tx.send(payload.clone()).unwrap();
        Ok(())
    }
}

pub async fn prepare_node(key: SecretKey) -> Node {
    let stun = "stun://stun.l.google.com:19302";
    let storage = Box::new(MemStorage::new());

    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let swarm = Arc::new(SwarmBuilder::new(0, stun, storage, session_sk).build());

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", swarm.did());

    Node::new(swarm)
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
    let did_names: DashMap<Did, String> = DashMap::new();
    let mut listeners = vec![];

    for (i, node) in nodes.into_iter().enumerate() {
        let name = format!("node{}", i + 1);
        did_names.insert(node.did(), name);

        listeners.push(async {
            let payload = node.listen_once().await.unwrap();
            format!(
                "{} should not receive any Msg, but got Msg {} -> {} [{} => {}] : {:?}",
                *did_names.get(&node.did()).unwrap(),
                *did_names.get(&payload.signer()).unwrap(),
                *did_names.get(&node.did()).unwrap(),
                *did_names.get(&payload.transaction.signer()).unwrap(),
                *did_names.get(&payload.transaction.destination).unwrap(),
                payload.transaction.data::<Message>().unwrap()
            )
        });
    }

    let mut listeners = FuturesUnordered::from_iter(listeners);

    tokio::select! {
        error_msg = listeners.next() => unreachable!("{}", error_msg.unwrap()),
        _ = sleep(Duration::from_secs(3)) => {}
    }
}

/// Wait until the nodes are quiescent, **state-driven, not on a wall clock**: every connection has
/// finished its handshake (none left in `New`/`Connecting`) and no buffered messages remain.
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
    // A snapshot of every node's DHT. Reaching `Connected` fires `on_data_channel_open -> join_dht`,
    // which mutates the DHT and emits more messages *after* the handshake finished — so true
    // quiescence also requires the DHT to have stopped changing, not just the handshakes to be done.
    let snapshot = || {
        nodes
            .iter()
            .map(|n| crate::inspect::DHTInspect::inspect(&n.dht()))
            .collect::<Vec<_>>()
    };

    // Diagnostics + hard failure if quiescence is never reached — never silently proceed, or later
    // assertions would run against unresolved async state (the bug this helper exists to catch).
    let ceiling = Duration::from_secs(30);
    let started = std::time::Instant::now();
    loop {
        let drained = drain().await;
        let before = snapshot();
        if !drained && !handshaking() {
            // Quiescent candidate: settle briefly, then require that across the gap nothing changed
            // — no message handed off, no handshake started, and no DHT mutation (join_dht /
            // stabilize chains). Any change means activity is still in flight; keep waiting.
            sleep(Duration::from_millis(500)).await;
            if !drain().await && !handshaking() && snapshot() == before {
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
            panic!(
                "wait_for_msgs did not reach quiescence within {ceiling:?}: still-handshaking \
                 nodes={handshaking_nodes:?}, last-loop drained={drained}"
            );
        }
    }
}
