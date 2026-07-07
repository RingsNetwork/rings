use std::sync::Arc;

use super::prepare_processor;
use crate::extension::protocols::echo::Echo;
use crate::extension::protocols::echo::EchoShell;
use crate::provider::Provider;

/// Namespace collision: registering two protocols on the same namespace is rejected, so a
/// later extension cannot silently shadow an earlier one.
#[tokio::test]
async fn duplicate_namespace_registration_is_rejected() {
    let provider = Provider::from_processor(Arc::new(prepare_processor().await));
    provider
        .register_protocol(Echo, EchoShell)
        .expect("first registration on a fresh namespace succeeds");
    assert!(
        provider.register_protocol(Echo, EchoShell).is_err(),
        "a second registration on the same namespace must error, not silently overwrite"
    );
}
