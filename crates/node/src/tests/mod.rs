#[cfg(feature = "node")]
use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;

#[cfg(feature = "node")]
use crate::prelude::DelegateeKey;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

/// Overlay every test fixture is published for and verified against.
pub(crate) const TEST_NETWORK_ID: u32 = 1;

/// ICE servers of every processor and provider fixture, native and browser: none, so peers
/// gather host candidates only.
///
/// All peers of these tests run in one process or one page, so host candidates connect them.
/// An external STUN server would only add a network dependency whose latency no test
/// controls. In the browser it would also sit inside `createOffer`/`answerOffer`, which wait
/// for ICE gathering to complete, ahead of every awaited event.
pub(crate) const TEST_ICE_SERVERS: &str = "";

/// Finger-table slots of every test processor.
pub(crate) const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;

/// Stabilize interval of [`test_processor_config`].
#[cfg(feature = "node")]
const TEST_STABILIZE_INTERVAL: u64 = 3;

/// The config of a native test processor signing with `key` on overlay `network_id`: the
/// [`TEST_ICE_SERVERS`] and a stabilize interval of [`TEST_STABILIZE_INTERVAL`]. A browser
/// processor stabilizes less often and builds its own.
#[cfg(feature = "node")]
pub(crate) fn test_processor_config(key: &SecretKey, network_id: u32) -> ProcessorConfig {
    let delegatee_key = DelegateeKey::new_with_seckey(key).expect("test delegatee key");
    ProcessorConfig::new(
        network_id,
        TEST_ICE_SERVERS.to_string(),
        delegatee_key,
        TEST_STABILIZE_INTERVAL,
    )
}

/// An in-memory replay store: the named choice of a fixture whose replay windows need not
/// survive its process. Production never builds one (#1022): a processor without an
/// authoritative replay store refuses every replay transition instead.
pub(crate) fn volatile_replay_storage() -> rings_core::message::ReplayStorage {
    Box::new(MemStorage::<rings_core::message::ReplayRecord>::new())
}

/// The builder of a test processor over `config`: memory storage and replay store,
/// [`TEST_DHT_FINGER_TABLE_SIZE`] finger slots and the [`activity::activity_observer`]. Each
/// fixture chains its one difference; a later `storage` replaces the memory one.
pub(crate) fn test_processor_builder(config: &ProcessorConfig) -> ProcessorBuilder {
    ProcessorBuilder::from_config(config)
        .expect("test processor config")
        .storage(Box::new(MemStorage::new()))
        .replay_storage(volatile_replay_storage())
        .dht_finger_table_size(TEST_DHT_FINGER_TABLE_SIZE)
        .observer(activity::activity_observer())
}

pub(crate) mod activity;
pub(crate) mod exit_descriptor;

#[cfg(feature = "node")]
pub mod native;
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub mod wasm;
