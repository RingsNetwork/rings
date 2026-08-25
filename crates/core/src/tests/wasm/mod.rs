use std::sync::Arc;

use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::ecc::SecretKey;
use crate::session::SessionSk;
use crate::storage::idb::IdbStorage;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

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

enum TestStorageMode {
    Default,
    Repair,
}

async fn prepare_node_with_storage_mode(key: SecretKey, mode: TestStorageMode) -> Arc<Swarm> {
    let stun = "stun://stun.l.google.com:19302";
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap(),
    );

    let builder = SwarmBuilder::new(0, stun, storage, session_sk);
    let builder = match mode {
        TestStorageMode::Default => builder,
        TestStorageMode::Repair => builder.dht_storage_redundancy(2).dht_virtual_nodes(0),
    };
    let swarm = Arc::new(builder.build());

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", swarm.did());

    swarm
}

pub async fn prepare_node(key: SecretKey) -> Arc<Swarm> {
    prepare_node_with_storage_mode(key, TestStorageMode::Default).await
}

pub async fn prepare_repair_node(key: SecretKey) -> Arc<Swarm> {
    prepare_node_with_storage_mode(key, TestStorageMode::Repair).await
}
