//! The HTTP handshake meeting a peer whose own offer prevails (#947), over the dummy transport
//! with the peer's public API stubbed in-process.

use std::sync::Arc;
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::message::MessagePayload;

use super::common::network_test_guard;
use super::common::prepare_processor_with_identity_key;
use super::common::test_callback;
use super::common::wait_processors_connected;
use crate::error::Error;
use crate::error::Result;
use crate::processor::handshake::RemoteHandshake;
use crate::processor::HandshakeFailure;
use crate::processor::Processor;

/// The public API of a peer whose sent offer prevails on the tie-break: it replies to an offer
/// with its own offer, and accepts the answer to it.
struct PrevailingPeer {
    /// The peer.
    processor: Arc<Processor>,
    /// The peer's own offer to the dialing node, sent back once.
    own_offer: Mutex<Option<MessagePayload>>,
}

#[async_trait::async_trait]
impl RemoteHandshake for PrevailingPeer {
    async fn answer_offer(&self, _offer: &MessagePayload) -> Result<MessagePayload> {
        self.own_offer
            .lock()
            .map_err(|_| Error::InternalError(rings_core::error::Error::LockPoisoned))?
            .take()
            .ok_or(Error::DecodeError)
    }

    async fn accept_answer(&self, answer: &MessagePayload) -> Result<()> {
        self.processor
            .swarm
            .accept_answer(answer.clone())
            .await
            .map_err(Error::AcceptAnswer)
    }
}

/// Two identity keys, the one with the smaller DID first.
fn ordered_keys() -> (SecretKey, SecretKey) {
    let (first, second) = (SecretKey::random(), SecretKey::random());
    let did = |key: &SecretKey| -> Did { key.address().into() };
    if did(&first) < did(&second) {
        (first, second)
    } else {
        (second, first)
    }
}

/// A counter-offer in reply to an HTTP offer is answered and delivered back: the dialing node's
/// losing generation is released, the outcome is `InFlight` (the edge is the peer's handshake,
/// not a failure), and the edge is admitted on both ends.
#[tokio::test]
async fn test_http_counter_offer_completes_the_edge() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let (smaller, larger) = ordered_keys();
    let peer = Arc::new(prepare_processor_with_identity_key(smaller).await);
    let dialer = prepare_processor_with_identity_key(larger).await;
    peer.swarm
        .set_callback(test_callback())
        .map_err(Error::InternalError)?;
    dialer
        .swarm
        .set_callback(test_callback())
        .map_err(Error::InternalError)?;

    let (_, own_offer) = peer
        .swarm
        .offer_connection(dialer.did())
        .await
        .map_err(Error::CreateOffer)?;
    let remote = PrevailingPeer {
        processor: Arc::clone(&peer),
        own_offer: Mutex::new(Some(own_offer)),
    };
    let outcome = dialer.exchange_offer(&remote, peer.did()).await;
    assert!(
        matches!(outcome, Err(HandshakeFailure::InFlight { peer: owner }) if owner == peer.did()),
        "{outcome:?}"
    );
    wait_processors_connected(&dialer, &peer).await;
    Ok(())
}
