use std::sync::Arc;

use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::ecc::SecretKey;
use crate::session::SessionSk;
use crate::storage::PersistenceStorage;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

mod test_channel;
mod test_fn_macro;
mod test_ice_servers;
mod test_idb_storage;
mod test_utils;
mod test_wasm_transport;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    tracing_wasm::set_as_global_default();
    tracing::debug!("test")
}

pub async fn prepare_node(key: SecretKey) -> Arc<Swarm> {
    let stun = "stun://stun.l.google.com:19302";
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let storage =
        PersistenceStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap();

    let swarm = Arc::new(SwarmBuilder::new(stun, storage, session_sk).build());

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", swarm.did());

    swarm
}
