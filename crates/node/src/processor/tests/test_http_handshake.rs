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
use rings_rpc::protos::rings_node::NodeDidResponse;
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
    async fn node_did(&self) -> Result<NodeDidResponse> {
        self.processor
            .handle_rpc(NodeDidRequest {})
            .await
            .map_err(Error::InternalRpcError)
    }

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
    async fn node_did(&self) -> Result<NodeDidResponse> {
        self.processor
            .handle_rpc(NodeDidRequest {})
            .await
            .map_err(Error::InternalRpcError)
    }

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

/// When `remote_rpc_proxy` is configured on a processor, `connect_peer_via_http` dials a public
/// domain endpoint (`http://seed.example.test:<port>/`) through the HTTP proxy without local DNS
/// resolution and completes `nodeDid` + `answerOffer` end to end (#1162).
#[tokio::test]
async fn test_connect_peer_via_http_uses_configured_remote_rpc_proxy() -> Result<()> {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use crate::remote_endpoint::RemoteRpcEndpoint;
    use crate::remote_endpoint::RemoteRpcProxy;
    use crate::test_support::TestTask;
    use crate::tests::test_processor_builder;
    use crate::tests::test_processor_config;

    let _network_guard = network_test_guard().await;
    let (seed_key, dialer_key) = ordered_keys();
    let seed = Arc::new(prepare_processor_with_identity_key(seed_key).await);

    let proxy_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
    let proxy_addr = proxy_listener
        .local_addr()
        .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
    let proxy = RemoteRpcProxy::parse(format!("http://{proxy_addr}").as_str())?;

    let dialer_config = test_processor_config(&dialer_key, 0).remote_rpc_proxy(proxy);
    let dialer = test_processor_builder(&dialer_config).build()?;

    let seed_for_proxy = Arc::clone(&seed);
    let proxy_task = TestTask::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = proxy_listener
                .accept()
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            let mut buf = vec![0_u8; 16384];
            let mut total = 0_usize;
            let header_end = loop {
                let n = stream
                    .read(buf.get_mut(total..).unwrap_or_default())
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                if n == 0 {
                    return Err(Error::RemoteRpcError("unexpected EOF".to_string()));
                }
                total += n;
                if let Some(pos) = buf
                    .get(..total)
                    .unwrap_or_default()
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                {
                    break pos + 4;
                }
            };
            let headers =
                String::from_utf8_lossy(buf.get(..header_end).unwrap_or_default()).to_string();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while total < header_end + content_length {
                let n = stream
                    .read(buf.get_mut(total..).unwrap_or_default())
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                if n == 0 {
                    break;
                }
                total += n;
            }
            requests.push(headers.lines().next().unwrap_or_default().to_string());
            let body_bytes = buf
                .get(header_end..header_end + content_length)
                .unwrap_or_default();
            let rpc_call: serde_json::Value = serde_json::from_slice(body_bytes)
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            let id = rpc_call
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Number(0.into()));
            let method = rpc_call
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let result_value = match method {
                "nodeDid" => {
                    let resp = seed_for_proxy
                        .handle_rpc(NodeDidRequest {})
                        .await
                        .map_err(Error::InternalRpcError)?;
                    serde_json::to_value(resp)
                        .map_err(|error| Error::RemoteRpcError(error.to_string()))?
                }
                "answerOffer" => {
                    let req: AnswerOfferRequest =
                        serde_json::from_value(rpc_call.get("params").cloned().unwrap_or_default())
                            .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                    let resp = seed_for_proxy
                        .handle_rpc(req)
                        .await
                        .map_err(Error::InternalRpcError)?;
                    serde_json::to_value(resp)
                        .map_err(|error| Error::RemoteRpcError(error.to_string()))?
                }
                other => {
                    return Err(Error::RemoteRpcError(format!(
                        "unexpected RPC method: {other}"
                    )))
                }
            };
            let response_body = serde_json::json!({
                "jsonrpc": "2.0",
                "result": result_value,
                "id": id,
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
        }
        Ok::<Vec<String>, Error>(requests)
    });

    let endpoint = RemoteRpcEndpoint::parse("http://seed.example.test:50001/")?;
    let attempt = dialer
        .connect_peer_via_http(
            &endpoint,
            None,
            crate::processor::HandshakePeer::Pinned(seed.did()),
        )
        .await
        .map_err(Error::from)?;
    assert_eq!(attempt.peer(), seed.did());
    let requests = proxy_task
        .await
        .map_err(|error| Error::RemoteRpcError(error.to_string()))??;
    assert_eq!(requests, vec![
        "POST http://seed.example.test:50001/ HTTP/1.1".to_string(),
        "POST http://seed.example.test:50001/ HTTP/1.1".to_string(),
    ]);
    wait_processors_connected(&dialer, &seed).await;
    Ok(())
}
