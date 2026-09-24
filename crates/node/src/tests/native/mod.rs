use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;

use crate::onion::OnionExitOffer;
use crate::onion::OnionRole;
use crate::prelude::DelegateeKey;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

mod test_duplicate_namespace;

const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;

pub async fn prepare_processor() -> Processor {
    prepare_processor_with_onion_role(OnionRole::Client).await
}

/// Prepare a test processor registering the onion symbols of `role`.
pub async fn prepare_processor_with_onion_role(role: OnionRole<OnionExitOffer>) -> Processor {
    let key = SecretKey::random();
    let sm = DelegateeKey::new_with_seckey(&key).unwrap();

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
        .dht_finger_table_size(TEST_DHT_FINGER_TABLE_SIZE)
        .onion_role(role);

    procssor_builder.build().unwrap()
}
