use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::message::MessagePayload;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

mod test_connection;
mod test_message_handler;
mod test_stabilization;

pub struct Node {
    swarm: Arc<Swarm>,
    message_rx: tokio::sync::mpsc::UnboundedReceiver<MessagePayload>,
}

pub struct NodeCallback {
    message_tx: tokio::sync::mpsc::UnboundedSender<MessagePayload>,
}

impl Node {
    pub fn new(swarm: Arc<Swarm>) -> Self {
        let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
        let callback = NodeCallback { message_tx };
        swarm.set_callback(Arc::new(callback));
        Self { swarm, message_rx }
    }

    pub async fn listen_once(&mut self) -> Option<MessagePayload> {
        self.message_rx.recv().await
    }
}

#[async_trait]
impl SwarmCallback for NodeCallback {
    async fn on_relay(&self, payload: &MessagePayload) -> Result<(), Box<dyn std::error::Error>> {
        self.message_tx.send(payload.clone()).map_err(|e| e.into())
    }
}

pub async fn prepare_node(key: SecretKey) -> Node {
    let stun = "stun://stun.l.google.com:19302";
    let storage = Box::new(MemStorage::new());

    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let swarm = Arc::new(SwarmBuilder::new(stun, storage, session_sk).build());

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
