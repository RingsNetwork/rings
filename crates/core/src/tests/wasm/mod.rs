use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::delegation::DelegateeKey;
use crate::ecc::SecretKey;
use crate::storage::idb::IdbStorage;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::utils::sleep;

mod test_ice_servers;
mod test_utils;
mod test_wasm_transport;

wasm_bindgen_test_configure!(run_in_browser);

enum TestStorageMode {
    Default,
    Repair,
}

async fn prepare_node_with_storage_mode(key: SecretKey, mode: TestStorageMode) -> Arc<Swarm> {
    let stun = "stun://stun.l.google.com:19302";
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap(),
    );

    let builder = SwarmBuilder::new(0, stun, storage, delegatee_key);
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

/// Run `test` under a per-test hang guard.
///
/// A browser test binary shares one wasm-bindgen-test budget, so a hung test would otherwise
/// time out the whole binary and starve every test after it. The guard fails with `name` after
/// `budget` instead. It is a failure bound only: a passing run proceeds on `test` alone.
pub async fn with_hang_guard<T>(name: &str, budget: Duration, test: impl Future<Output = T>) -> T {
    let test = test.fuse();
    let deadline = sleep(budget).fuse();
    futures::pin_mut!(test, deadline);
    futures::select! {
        value = test => value,
        () = deadline => panic!("{name} exceeded its {budget:?} hang guard"),
    }
}
