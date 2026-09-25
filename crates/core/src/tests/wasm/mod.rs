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

/// ICE servers of every browser fixture: none, so peers gather host candidates only.
///
/// Every peer of these tests lives in the same page. An external STUN server would put its
/// latency inside `create_offer`/`answer_offer`, which wait for ICE gathering to complete
/// (up to the transport's 60 s gather bound), and make a test's hang guard load-bearing.
pub(crate) const TEST_ICE_SERVERS: &str = "";

enum TestStorageMode {
    Default,
    Repair,
}

async fn prepare_node_with_storage_mode(key: SecretKey, mode: TestStorageMode) -> Arc<Swarm> {
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap(),
    );

    let builder = SwarmBuilder::new(0, TEST_ICE_SERVERS, storage, delegatee_key);
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

/// Run the test `name` under a per-test hang guard of `budget`.
///
/// A browser test binary shares one wasm-bindgen-test budget of 120 s, so a hung test would
/// otherwise time out the whole binary and starve every test after it. The guard fails with
/// `name` instead. For a test whose waits are all events, it is a failure bound only and the
/// passing run proceeds on `test` alone. A caller whose test polls on durations passes a
/// scenario budget instead, which that caller must name as such.
///
/// Budget arithmetic for this binary: the whole suite, repair soak included, runs in about
/// 80 s with host-only ICE. If both real-transport handshake tests hung, they would add at most
/// 15 s + 15 s, for about 110 s, still inside the runner's 120 s. So a named guard fails first,
/// and the tests after it still run.
pub async fn with_hang_guard<T>(name: &str, budget: Duration, test: impl Future<Output = T>) -> T {
    let test = test.fuse();
    let deadline = sleep(budget).fuse();
    futures::pin_mut!(test, deadline);
    futures::select! {
        value = test => value,
        () = deadline => panic!("{name} exceeded its {budget:?} hang guard"),
    }
}
