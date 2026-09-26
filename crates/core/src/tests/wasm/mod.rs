use std::sync::Arc;

use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::delegation::DelegateeKey;
use crate::ecc::SecretKey;
use crate::storage::idb::IdbStorage;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::activity::ActivityCallback;
use crate::tests::activity::ActivityObserver;

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

/// Build a browser test swarm whose observer and callback record activity,
/// so tests can probe its state on activity instead of on a timer.
async fn prepare_node_with_storage_mode(key: SecretKey, mode: TestStorageMode) -> Arc<Swarm> {
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(1000, uuid::Uuid::new_v4().to_string().as_str())
            .await
            .unwrap(),
    );

    let builder = SwarmBuilder::new(0, TEST_ICE_SERVERS, storage, delegatee_key)
        .observer(Arc::new(ActivityObserver));
    let builder = match mode {
        TestStorageMode::Default => builder,
        TestStorageMode::Repair => builder.dht_storage_redundancy(2).dht_virtual_nodes(0),
    };
    let swarm = Arc::new(builder.build());
    swarm.set_callback(Arc::new(ActivityCallback)).unwrap();

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

/// Budget arithmetic of this binary's hang guards (measured unloaded in headless Chrome, not on
/// CI or Firefox): CI runs the repair soak as its own invocation (see `qaci.yml`), so its 60 s
/// scenario hang guard never shares the 120 s runner budget with the rest. The rest runs in about
/// 55 s. If both real-transport handshake tests hung, they would add at most 15 s + 15 s, for
/// about 85 s, a margin of about 1.4x below 120 s, so a named guard fails first and the tests
/// after it still run.
pub use rings_test_support::with_hang_guard;
