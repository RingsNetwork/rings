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
        0,
        "stun://stun.l.google.com:19302".to_string(),
        sm,
        3,
    ))
    .unwrap();

    let storage = Box::new(MemStorage::new());

    let procssor_builder = ProcessorBuilder::from_serialized(&config)
        .unwrap()
        .storage(storage);

    procssor_builder.build().unwrap()
}

/// Namespace collision: registering two protocols on the same namespace is rejected, so a
/// later extension cannot silently shadow an earlier one (one of the reviewer-requested
/// lifecycle guards).
#[tokio::test]
async fn duplicate_namespace_registration_is_rejected() {
    use std::sync::Arc;

    use crate::extension::protocols::echo::Echo;
    use crate::extension::protocols::echo::EchoShell;
    use crate::provider::Provider;

    let provider = Provider::from_processor(Arc::new(prepare_processor().await));
    provider
        .register_protocol(Echo, EchoShell)
        .expect("first registration on a fresh namespace succeeds");
    assert!(
        provider.register_protocol(Echo, EchoShell).is_err(),
        "a second registration on the same namespace must error, not silently overwrite"
    );
}
