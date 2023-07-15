pub mod browser;
pub mod processor;

use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::logging::browser::init_logging;
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::prelude::uuid;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::CallbackFn;
use crate::prelude::SessionManager;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    init_logging(tracing::Level::INFO);
    tracing::debug!("test")
}

pub async fn prepare_processor(message_callback: Option<CallbackFn>) -> Processor {
    let key = SecretKey::random();
    let sm = SessionManager::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig {
        ice_servers: "stun://stun.l.google.com:19302".to_string(),
        external_address: None,
        session_manager: sm.dump().unwrap(),
        stabilize_timeout: 200,
    })
    .unwrap();

    let storage_path = uuid::Uuid::new_v4().to_simple().to_string();
    let storage = PersistenceStorage::new_with_cap_and_path(50000, storage_path.as_str())
        .await
        .unwrap();

    let mut processor_builder = ProcessorBuilder::from_config(config)
        .unwrap()
        .storage(storage);

    if let Some(callback) = message_callback {
        processor_builder = processor_builder.message_callback(callback);
    }

    processor_builder.build().unwrap()
}
