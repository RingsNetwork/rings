pub mod browser;
pub mod processor;
pub mod snark;
use rings_core::ecc::SecretKey;
use rings_core::prelude::uuid;
use rings_core::session::SessionSk;
use rings_core::storage::idb::IdbStorage;
use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::logging::browser::init_logging;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    init_logging(crate::logging::LogLevel::Info);
    tracing::debug!("test")
}

pub async fn prepare_processor() -> Processor {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        "stun://stun.l.google.com:19302".to_string(),
        sm,
        200,
    ))
    .unwrap();

    let storage_name = uuid::Uuid::new_v4().to_simple().to_string();
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(50000, &storage_name)
            .await
            .unwrap(),
    );

    ProcessorBuilder::from_serialized(&config)
        .unwrap()
        .storage(storage)
        .build()
        .unwrap()
}
