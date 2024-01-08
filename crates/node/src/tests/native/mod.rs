use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::SessionSk;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

pub async fn prepare_processor() -> (Processor, String) {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        "stun://stun.l.google.com:19302".to_string(),
        sm,
        200,
    ))
    .unwrap();

    let storage_path = PersistenceStorage::random_path("./tmp");
    let storage = PersistenceStorage::new_with_path(storage_path.as_str())
        .await
        .unwrap();

    let procssor_builder = ProcessorBuilder::from_serialized(&config)
        .unwrap()
        .storage(storage);

    (procssor_builder.build().unwrap(), storage_path)
}
