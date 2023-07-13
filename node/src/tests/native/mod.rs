use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::CallbackFn;
use crate::prelude::SessionManager;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

pub async fn prepare_processor(message_callback: Option<CallbackFn>) -> (Processor, String) {
    let key = SecretKey::random();
    let sm = SessionManager::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig {
        ice_servers: "stun://stun.l.google.com:19302".to_string(),
        external_address: None,
        session_manager: sm.dump().unwrap(),
        stabilize_timeout: 200,
    })
    .unwrap();

    let storage_path = PersistenceStorage::random_path("./tmp");
    let storage = PersistenceStorage::new_with_path(storage_path.as_str())
        .await
        .unwrap();

    let procssor = ProcessorBuilder::from_config(config)
        .unwrap()
        .storage(storage)
        .message_callback(message_callback)
        .build()
        .unwrap();

    (procssor, storage_path)
}
