use std::sync::OnceLock;

use rings_core::ecc::SecretKey;

use crate::onion::OnionExitOffer;
use crate::onion::OnionRole;
use crate::processor::Processor;
use crate::tests::test_processor_builder;
use crate::tests::test_processor_config;

mod test_duplicate_namespace;

/// Native WebRTC tests share process-global ICE/UDP resources and timing-sensitive connection
/// callbacks; they run serially under this lock, so one test's candidates or callbacks cannot
/// add pressure to another test's handshake.
static NETWORK_TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// Hold the native WebRTC test lock for the rest of a test.
pub(crate) async fn network_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    NETWORK_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

pub async fn prepare_processor() -> Processor {
    prepare_processor_with_onion_role(OnionRole::Client).await
}

/// Prepare a test processor registering the onion symbols of `role`.
pub async fn prepare_processor_with_onion_role(role: OnionRole<OnionExitOffer>) -> Processor {
    test_processor_builder(&test_processor_config(&SecretKey::random(), 0))
        .onion_role(role)
        .build()
        .unwrap()
}
