use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;

use crate::prelude::SessionSk;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;
#[cfg(feature = "snark")]
pub mod test_snark;

mod test_duplicate_namespace;

const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;

pub async fn prepare_processor() -> Processor {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
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
