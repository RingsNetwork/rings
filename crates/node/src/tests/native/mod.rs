use std::sync::OnceLock;

use rings_core::ecc::SecretKey;
use rings_core::error::Error as CoreError;
use rings_core::swarm::OfferReply;

use crate::onion::OnionExitOffer;
use crate::onion::OnionRole;
use crate::processor::handshake::HandshakeFailure;
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

/// Signal one WebRTC edge from `first` to `second` with a manual offer/answer exchange.
///
/// `second` replies with an answer, or, when its own sent overlay offer prevails on the
/// tie-break, with that offer, which `first` answers (#947). Chord stabilization may already be
/// connecting the pair, so a refusal because a concurrent handshake owns the edge
/// ([`HandshakeFailure::is_concurrent`]) satisfies the request: the edge forms from that
/// handshake.
pub(crate) async fn signal_edge(first: &Processor, second: &Processor) -> Result<(), CoreError> {
    match exchange_offer(first, second).await {
        Err(error) if HandshakeFailure::is_concurrent(&error) => Ok(()),
        outcome => outcome,
    }
}

/// One offer/answer exchange from `first` to `second`, as [`signal_edge`] describes.
async fn exchange_offer(first: &Processor, second: &Processor) -> Result<(), CoreError> {
    let (attempt, offer) = first.swarm.offer_connection(second.did()).await?;
    match second.swarm.answer_offer(offer).await? {
        OfferReply::Answer(answer) => first.swarm.accept_answer_for(attempt, answer).await,
        OfferReply::CounterOffer(counter_offer) => {
            let answer = first
                .swarm
                .answer_offer(counter_offer)
                .await?
                .answer()
                .ok_or(CoreError::AlreadyConnected)?;
            second.swarm.accept_answer(answer).await
        }
    }
}
