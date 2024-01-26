use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;

use crate::prelude::SessionSk;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;
pub mod snark;

pub async fn prepare_processor() -> Processor {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        "stun://stun.l.google.com:19302".to_string(),
        sm,
        200,
    ))
    .unwrap();

    let storage = Box::new(MemStorage::new());

    let procssor_builder = ProcessorBuilder::from_serialized(&config)
        .unwrap()
        .storage(storage);

    procssor_builder.build().unwrap()
}
