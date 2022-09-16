use std::sync::Arc;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::message::MessageHandler;
use crate::storage::PersistenceStorage;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

mod test_message_handler;
mod test_stabilize;

pub async fn prepare_node(
    key: SecretKey,
) -> (Did, Arc<PeerRing>, Arc<Swarm>, MessageHandler, String) {
    let stun = "stun://stun.l.google.com:19302";
    let did = key.address().into();
    let path = PersistenceStorage::random_path("./tmp");
    let storage = PersistenceStorage::new_with_path(path.as_str())
        .await
        .unwrap();

    let swarm = Arc::new(SwarmBuilder::new(stun, storage).key(key).build().unwrap());
    let dht = swarm.dht();
    let node = MessageHandler::new(swarm.clone());

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", did);

    (did, dht, swarm, node, path)
}
