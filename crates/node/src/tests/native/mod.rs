use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;

use crate::prelude::DelegateeKey;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

mod test_duplicate_namespace;

const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;

/// ICE servers of every native processor fixture: none, so peers gather host candidates only.
///
/// All peers of these tests run in this process, so loopback host candidates connect them; an
/// external STUN server would only add a network dependency whose latency no test controls.
pub(crate) const TEST_ICE_SERVERS: &str = "";

pub async fn prepare_processor() -> Processor {
    let key = SecretKey::random();
    let sm = DelegateeKey::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        0,
        TEST_ICE_SERVERS.to_string(),
        sm,
        3,
    ))
    .unwrap();

    let storage = Box::new(MemStorage::new());

    let procssor_builder = ProcessorBuilder::from_serialized(&config)
        .unwrap()
        .storage(storage)
        .dht_finger_table_size(TEST_DHT_FINGER_TABLE_SIZE);

    procssor_builder.build().unwrap()
}
