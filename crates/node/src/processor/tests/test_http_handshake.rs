//! The HTTP handshake meeting a peer that holds its own offer to the dialing node (#947), or a
//! peer of another wire version (#1104), over the dummy transport with the peer's public API
//! stubbed in-process.

use std::sync::Arc;
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::message::Decoder;
use rings_core::message::Encoder;
use rings_core::message::MessagePayload;
use rings_core::message::PeerWireVersion;
use rings_core::message::WireVersionMismatch;
use rings_core::message::WIRE_VERSION;
use rings_rpc::protos::rings_node::AnswerOfferRequest;
use rings_rpc::protos::rings_node::NodeDidRequest;
use rings_rpc::protos::rings_node_handler::HandleRpc;

use super::common::connect_processors;
use super::common::network_test_guard;
use super::common::prepare_processor_with_identity_key;
use super::common::wait_processors_connected;
use crate::error::Error;
use crate::error::Result;
use crate::error::INCOMPATIBLE_WIRE_VERSION_CODE;
use crate::processor::handshake::admit_endpoint_wire_version;
use crate::processor::handshake::RemoteHandshake;
use crate::processor::HandshakeFailure;
use crate::processor::Processor;

/// The public API of a peer of 0.34.2 or earlier whose sent offer prevails on the tie-break: it
/// replies to an offer with its own offer, and accepts the answer to it.
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

/// A counter-offer in reply to an HTTP offer, from a peer of 0.34.2 or earlier, is answered and
/// delivered back: the dialing node's losing generation is released, the outcome is `InFlight`
/// (the edge is the peer's handshake, not a failure), and the edge is admitted on both ends.
#[tokio::test]
async fn test_http_counter_offer_completes_the_edge() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let (smaller, larger) = ordered_keys();
    let peer = Arc::new(prepare_processor_with_identity_key(smaller).await);
    let dialer = prepare_processor_with_identity_key(larger).await;
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

/// The public half of a seed's HTTP API, as its external listener serves it to a caller without
/// the API token: `answerOffer` reaches the seed, and `acceptAnswer` is refused.
struct PublicSeed {
    /// The seed.
    processor: Arc<Processor>,
}

#[async_trait::async_trait]
impl RemoteHandshake for PublicSeed {
    async fn answer_offer(&self, offer: &MessagePayload) -> Result<MessagePayload> {
        Ok(self
            .processor
            .swarm
            .answer_offer(offer.clone())
            .await
            .map_err(Error::AnswerOffer)?
            .into_inner())
    }

    async fn accept_answer(&self, _answer: &MessagePayload) -> Result<()> {
        Err(Error::RemoteRpcError("401 Unauthorized".to_string()))
    }
}

/// A seed that dialed a node over the DHT, with an offer that was never delivered (the node was
/// offline), answers that node's HTTP join in both DID orders, through the public half of its
/// API alone: the join's offer prevails over the seed's lost one, which is released.
#[tokio::test]
async fn test_http_join_prevails_over_the_seed_lost_overlay_offer() -> Result<()> {
    let _network_guard = network_test_guard().await;
    for seed_is_smaller in [true, false] {
        let (smaller, larger) = ordered_keys();
        let (seed_key, joining_key) = if seed_is_smaller {
            (smaller, larger)
        } else {
            (larger, smaller)
        };
        let seed = Arc::new(prepare_processor_with_identity_key(seed_key).await);
        let joining = prepare_processor_with_identity_key(joining_key).await;
        let relay = prepare_processor_with_identity_key(SecretKey::random()).await;
        connect_processors(&seed, &relay).await;
        seed.swarm
            .connect(joining.did())
            .await
            .map_err(Error::ConnectError)?;
        assert!(
            seed.swarm
                .has_unadmitted_connection(joining.did())
                .map_err(Error::ConnectError)?,
            "the seed's offer left through the relay, which cannot deliver it"
        );

        let remote = PublicSeed {
            processor: Arc::clone(&seed),
        };
        let outcome = joining.exchange_offer(&remote, seed.did()).await;
        assert!(outcome.is_ok(), "{outcome:?}");
        wait_processors_connected(&joining, &seed).await;
    }
    Ok(())
}

/// `offer` as a peer of another wire version would send it: the same frame under the header of
/// `theirs`, or, for `None`, under the opening of a peer before wire versioning.
fn reframed(offer: &MessagePayload, theirs: Option<u16>) -> Result<String> {
    let frame = Vec::<u8>::from_encoded(&offer.encode().map_err(Error::CoreError)?)
        .map_err(Error::CoreError)?;
    let frame = match theirs {
        Some(version) => [&frame[..6], &version.to_be_bytes(), &frame[8..]].concat(),
        None => [b"RINGS-PAYLOAD\0".as_slice(), &frame[9..]].concat(),
    };
    Ok(frame.encode().map_err(Error::CoreError)?.to_string())
}

/// The answerer's half of the wire-version law (#1104): `answerOffer` refuses an offer of
/// another wire version, and one from a peer before wire versioning, before decoding it, with
/// the stable code and both versions as data; an offer of this version is answered.
#[tokio::test]
async fn test_answer_offer_refuses_an_offer_of_another_wire_version() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let (answerer_key, dialer_key) = ordered_keys();
    let answerer = prepare_processor_with_identity_key(answerer_key).await;
    let dialer = prepare_processor_with_identity_key(dialer_key).await;
    let offer = dialer
        .swarm
        .create_offer(answerer.did())
        .await
        .map_err(Error::CreateOffer)?;

    for (theirs, data) in [
        (
            Some(WIRE_VERSION + 1),
            serde_json::Value::from(WIRE_VERSION + 1),
        ),
        (None, serde_json::Value::Null),
    ] {
        let refused = answerer
            .handle_rpc(AnswerOfferRequest {
                offer: reframed(&offer, theirs)?,
            })
            .await;
        let Err(refusal) = refused else {
            panic!("an offer of wire version {theirs:?} was answered");
        };
        assert_eq!(
            refusal.code,
            jsonrpc_core::ErrorCode::ServerError(INCOMPATIBLE_WIRE_VERSION_CODE.into())
        );
        assert_eq!(
            refusal.data,
            Some(serde_json::json!({ "ours": WIRE_VERSION, "theirs": data }))
        );
    }
    assert!(answerer
        .handle_rpc(AnswerOfferRequest {
            offer: offer.encode().map_err(Error::CoreError)?.to_string(),
        })
        .await
        .is_ok());
    Ok(())
}

/// The offerer's half of the wire-version law (#1104): `nodeDid` states this node's version,
/// and an endpoint that states another, or none (a node before wire versioning), is refused
/// before any offer is created.
#[tokio::test]
async fn test_the_offerer_refuses_an_endpoint_of_another_wire_version() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let node = prepare_processor_with_identity_key(SecretKey::random()).await;
    let reported = node
        .handle_rpc(NodeDidRequest {})
        .await
        .map_err(Error::InternalRpcError)?
        .wire_version;
    assert_eq!(reported, Some(WIRE_VERSION));
    assert!(admit_endpoint_wire_version(reported).is_ok());

    for (stated, theirs) in [
        (
            Some(WIRE_VERSION + 1),
            PeerWireVersion::Versioned(WIRE_VERSION + 1),
        ),
        (None, PeerWireVersion::BeforeVersioning),
    ] {
        assert!(matches!(
            admit_endpoint_wire_version(stated),
            Err(Error::IncompatibleWireVersion(WireVersionMismatch { ours: WIRE_VERSION, theirs: refused }))
                if refused == theirs
        ));
    }
    Ok(())
}

/// A mismatch fails on both sides at once (#1104): the refusal an answerer of version `v`
/// returns over JSON-RPC reaches the offerer as its own mismatch, naming `v` as the peer's.
#[test]
fn test_a_remote_refusal_reaches_the_offerer_as_its_own_mismatch() {
    let answerers = WIRE_VERSION + 1;
    let answerer_refusal =
        jsonrpc_core::Error::from(Error::IncompatibleWireVersion(WireVersionMismatch {
            ours: answerers,
            theirs: PeerWireVersion::Versioned(WIRE_VERSION),
        }));
    let offerer_error = Error::from(rings_rpc::jsonrpc::RpcError::JsonClientError(
        answerer_refusal,
    ));
    assert!(matches!(
        offerer_error,
        Error::IncompatibleWireVersion(WireVersionMismatch {
            ours: WIRE_VERSION,
            theirs: PeerWireVersion::Versioned(version),
        }) if version == answerers
    ));
}
