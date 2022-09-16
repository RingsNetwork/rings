use std::sync::Arc;

use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::message::MessageHandler;
use crate::storage::PersistenceStorage;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

mod test_channel;
mod test_ice_servers;
mod test_idb_storage;
mod test_wasm_transport;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    console_log::init_with_level(log::Level::Trace).unwrap();
    log::debug!("test")
}

pub async fn prepare_node(key: SecretKey) -> (Did, Arc<PeerRing>, Arc<Swarm>, MessageHandler) {
    let stun = "stun://stun.l.google.com:19302";
    let did = key.address().into();
    let storage =
        PersistenceStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap();

    let swarm = Arc::new(SwarmBuilder::new(stun, storage).key(key).build().unwrap());
    let dht = swarm.dht();
    let node = MessageHandler::new(swarm.clone());

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", did);

    (did, dht, swarm, node)
}
