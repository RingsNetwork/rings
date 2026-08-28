//! RPC handler for both feature=browser and feature=node.
//! We support handling the RPC request in either native or browser environment by `InternalRpcHandler` and `ExternalRpcHandler` from rings_rpc crate.
//! For the native environment, we use jsonrpc_core to handle requests.
//! For the browser environment, we use `InternalRpcHandler` to process the requests.

use std::collections::HashSet;
#[cfg(all(feature = "node", not(target_family = "wasm")))]
use std::net::IpAddr;
#[cfg(all(feature = "node", not(target_family = "wasm")))]
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::str::FromStr;

use async_trait::async_trait;
use futures::future::join_all;
use jsonrpc_core::types::error::Error;
use jsonrpc_core::types::error::ErrorCode;
use jsonrpc_core::Result;
use rings_core::dht::entry::Entry;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::message::e2e;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;
use rings_rpc::protos::rings_node::*;
use rings_rpc::protos::rings_node_handler::HandleRpc;

use crate::error::Error as ServerError;
use crate::processor::Processor;
use crate::seed::Seed;

const DEFAULT_PEER_MEASUREMENT_PAGE_SIZE: u32 = 100;
const MAX_PEER_MEASUREMENT_PAGE_SIZE: u32 = 1_000;

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<ConnectPeerViaHttpRequest, ConnectPeerViaHttpResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: ConnectPeerViaHttpRequest,
    ) -> Result<ConnectPeerViaHttpResponse> {
        #[cfg(all(feature = "node", not(target_family = "wasm")))]
        let client = {
            let targets = admit_http_signaling_underlay(self, &req.url).await?;
            let http_client = pinned_signaling_http_client(&req.url, &targets)?;
            rings_rpc::jsonrpc::Client::with_http_client(&req.url, http_client)
        };
        #[cfg(not(all(feature = "node", not(target_family = "wasm"))))]
        let client = rings_rpc::jsonrpc::Client::new(&req.url);

        let did = client
            .node_did(&NodeDidRequest {})
            .await
            .map_err(|e| ServerError::RemoteRpcError(e.to_string()))?
            .did;

        let offer = self
            .handle_rpc(CreateOfferRequest { did: did.clone() })
            .await?
            .offer;

        let answer = client
            .answer_offer(&AnswerOfferRequest { offer })
            .await
            .map_err(|e| ServerError::RemoteRpcError(e.to_string()))?
            .answer;

        self.handle_rpc(AcceptAnswerRequest { answer }).await?;

        Ok(ConnectPeerViaHttpResponse { did })
    }
}

#[cfg(all(feature = "node", not(target_family = "wasm")))]
async fn admit_http_signaling_underlay(
    processor: &Processor,
    endpoint: &str,
) -> Result<Vec<IpAddr>> {
    let targets = resolve_http_signaling_targets(endpoint).await?;
    processor
        .swarm
        .admit_underlay_targets(&targets)
        .await
        .map_err(|error| {
            Error::from(ServerError::RemoteRpcError(format!(
                "cannot admit signaling endpoint {endpoint:?} before HTTP bootstrap: {error}"
            )))
        })?;
    Ok(targets)
}

#[cfg(all(feature = "node", not(target_family = "wasm")))]
async fn resolve_http_signaling_targets(endpoint: &str) -> Result<Vec<IpAddr>> {
    let url = reqwest::Url::parse(endpoint).map_err(|error| {
        ServerError::RemoteRpcError(format!(
            "invalid HTTP signaling endpoint {endpoint:?}: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ServerError::RemoteRpcError(format!(
            "signaling endpoint {endpoint:?} must use http or https"
        ))
        .into());
    }
    let host = url.host_str().ok_or_else(|| {
        ServerError::RemoteRpcError(format!("HTTP signaling endpoint {endpoint:?} has no host"))
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        ServerError::RemoteRpcError(format!(
            "HTTP signaling endpoint {endpoint:?} has no usable port"
        ))
    })?;

    if let Ok(address) = host.parse::<IpAddr>() {
        return ipv4_signaling_targets(endpoint, [address]);
    }

    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| {
            ServerError::RemoteRpcError(format!(
                "cannot resolve signaling endpoint {endpoint:?} before HTTP bootstrap; named \
                 endpoints require gateway DNS bypass: {error}"
            ))
        })?;
    ipv4_signaling_targets(endpoint, resolved.map(|address| address.ip()))
}

#[cfg(all(feature = "node", not(target_family = "wasm")))]
fn ipv4_signaling_targets(
    endpoint: &str,
    addresses: impl IntoIterator<Item = IpAddr>,
) -> Result<Vec<IpAddr>> {
    let mut targets = addresses
        .into_iter()
        .filter(IpAddr::is_ipv4)
        .collect::<Vec<_>>();
    targets.sort_unstable();
    targets.dedup();
    if targets.is_empty() {
        return Err(ServerError::RemoteRpcError(format!(
            "HTTP signaling endpoint {endpoint:?} resolved to no IPv4 targets; the native \
             gateway milestone is IPv4-only"
        ))
        .into());
    }
    Ok(targets)
}

#[cfg(all(feature = "node", not(target_family = "wasm")))]
fn pinned_signaling_http_client(endpoint: &str, targets: &[IpAddr]) -> Result<reqwest::Client> {
    let url = reqwest::Url::parse(endpoint).map_err(|error| {
        ServerError::RemoteRpcError(format!(
            "invalid HTTP signaling endpoint {endpoint:?}: {error}"
        ))
    })?;
    let host = url.host_str().ok_or_else(|| {
        ServerError::RemoteRpcError(format!("HTTP signaling endpoint {endpoint:?} has no host"))
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        ServerError::RemoteRpcError(format!(
            "HTTP signaling endpoint {endpoint:?} has no usable port"
        ))
    })?;

    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if host.parse::<IpAddr>().is_err() {
        let addresses = targets
            .iter()
            .copied()
            .map(|address| SocketAddr::new(address, port))
            .collect::<Vec<_>>();
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    builder.build().map_err(|error| {
        ServerError::RemoteRpcError(format!(
            "cannot construct pinned HTTP client for signaling endpoint {endpoint:?}: {error}"
        ))
        .into()
    })
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<ConnectWithDidRequest, ConnectWithDidResponse> for Processor {
    async fn handle_rpc(&self, req: ConnectWithDidRequest) -> Result<ConnectWithDidResponse> {
        let did = s2d(&req.did)?;
        self.connect_with_did(did).await.map_err(Error::from)?;
        Ok(ConnectWithDidResponse {})
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<ConnectWithSeedRequest, ConnectWithSeedResponse> for Processor {
    async fn handle_rpc(&self, req: ConnectWithSeedRequest) -> Result<ConnectWithSeedResponse> {
        let seed: Seed = Seed::try_from(req)?;

        let mut connected: HashSet<String> =
            HashSet::from_iter(self.swarm.peers().into_iter().map(|peer| peer.did));
        connected.insert(self.swarm.did().to_string());

        let tasks = seed
            .peers
            .iter()
            .filter(|&x| !connected.contains(&x.did))
            .map(|x| {
                self.handle_rpc(ConnectPeerViaHttpRequest {
                    url: x.url.to_string(),
                })
            });

        let results = join_all(tasks).await;

        let first_err = results.into_iter().find(|x| x.is_err());
        if let Some(err) = first_err {
            err?;
        }

        Ok(ConnectWithSeedResponse {})
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<ListPeersRequest, ListPeersResponse> for Processor {
    async fn handle_rpc(&self, _req: ListPeersRequest) -> Result<ListPeersResponse> {
        let peers = self
            .swarm
            .peers()
            .into_iter()
            .map(|peer| peer.into())
            .collect();
        Ok(ListPeersResponse { peers })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<CreateOfferRequest, CreateOfferResponse> for Processor {
    async fn handle_rpc(&self, req: CreateOfferRequest) -> Result<CreateOfferResponse> {
        let did = s2d(&req.did)?;
        let offer_payload = self
            .swarm
            .create_offer(did)
            .await
            .map_err(ServerError::CreateOffer)
            .map_err(Error::from)?;

        let encoded = offer_payload
            .encode()
            .map_err(|_| ServerError::EncodeError)?;

        Ok(CreateOfferResponse {
            offer: encoded.to_string(),
        })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<AnswerOfferRequest, AnswerOfferResponse> for Processor {
    async fn handle_rpc(&self, req: AnswerOfferRequest) -> Result<AnswerOfferResponse> {
        if req.offer.is_empty() {
            return Err(Error::invalid_params("Offer is empty"));
        }
        let encoded: Encoded = <Encoded as From<String>>::from(req.offer);

        let offer_payload =
            MessagePayload::from_encoded(&encoded).map_err(|_| ServerError::DecodeError)?;

        let answer_payload = self
            .swarm
            .answer_offer(offer_payload)
            .await
            .map_err(ServerError::AnswerOffer)
            .map_err(Error::from)?;

        tracing::debug!("connect_peer_via_ice response: {:?}", answer_payload);
        let encoded = answer_payload
            .encode()
            .map_err(|_| ServerError::EncodeError)?;

        Ok(AnswerOfferResponse {
            answer: encoded.to_string(),
        })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<AcceptAnswerRequest, AcceptAnswerResponse> for Processor {
    async fn handle_rpc(&self, req: AcceptAnswerRequest) -> Result<AcceptAnswerResponse> {
        if req.answer.is_empty() {
            return Err(Error::invalid_params("Answer is empty"));
        }
        let encoded = Encoded::from(req.answer);

        let answer_payload =
            MessagePayload::from_encoded(&encoded).map_err(|_| ServerError::DecodeError)?;
        answer_payload.transaction.signer();

        self.swarm
            .accept_answer(answer_payload)
            .await
            .map_err(ServerError::AcceptAnswer)?;

        Ok(AcceptAnswerResponse {})
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<DisconnectRequest, DisconnectResponse> for Processor {
    async fn handle_rpc(&self, req: DisconnectRequest) -> Result<DisconnectResponse> {
        let did = s2d(&req.did)?;
        self.disconnect(did).await?;
        Ok(DisconnectResponse {})
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<SendBackendMessageRequest, SendBackendMessageResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: SendBackendMessageRequest,
    ) -> Result<SendBackendMessageResponse> {
        let destination = s2d(&req.destination_did)?;
        let payload = base64::decode(req.data.as_str())
            .map_err(|e| Error::invalid_params(format!("data is not valid base64: {e:?}")))?;
        let envelope =
            crate::extension::ext::Envelope::new(req.namespace, bytes::Bytes::from(payload));
        self.send_envelope(destination, &envelope).await?;
        Ok(SendBackendMessageResponse {})
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<SendE2eHandshakeRequest, SendE2eHandshakeResponse> for Processor {
    async fn handle_rpc(&self, req: SendE2eHandshakeRequest) -> Result<SendE2eHandshakeResponse> {
        let destination = s2d(&req.destination_did)?;
        let tx_id = self.send_e2e_handshake(destination).await?;
        Ok(SendE2eHandshakeResponse {
            tx_id: tx_id.to_string(),
        })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<SendE2eMessageRequest, SendE2eMessageResponse> for Processor {
    async fn handle_rpc(&self, req: SendE2eMessageRequest) -> Result<SendE2eMessageResponse> {
        let destination = s2d(&req.destination_did)?;
        let recipient_public_key = s2pk(&req.recipient_public_key)?;
        let payload = base64::decode(req.data.as_str())
            .map_err(|e| Error::invalid_params(format!("data is not valid base64: {e:?}")))?;
        let frame_len = if req.max_plaintext_frame_len == 0 {
            e2e::DEFAULT_E2E_PLAINTEXT_FRAME_LEN
        } else {
            req.max_plaintext_frame_len as usize
        };

        let stream_id = self
            .send_e2e_message_with_frame_len(destination, recipient_public_key, &payload, frame_len)
            .await?;
        Ok(SendE2eMessageResponse {
            stream_id: stream_id.to_string(),
        })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<PublishMessageToTopicRequest, PublishMessageToTopicResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: PublishMessageToTopicRequest,
    ) -> Result<PublishMessageToTopicResponse> {
        let encoded = req
            .data
            .encode()
            .map_err(|e| Error::invalid_params(format!("Failed to encode data: {e:?}")))?;
        self.storage_append_data(&req.topic, encoded).await?;
        Ok(PublishMessageToTopicResponse {})
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<FetchTopicMessagesRequest, FetchTopicMessagesResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: FetchTopicMessagesRequest,
    ) -> Result<FetchTopicMessagesResponse> {
        let entry_key = Entry::gen_did(&req.topic)
            .map_err(|_| Error::invalid_params("Failed to get id of topic"))?;

        self.storage_fetch(entry_key).await?;
        let result = self.storage_check_cache(entry_key).await;

        let Some(entry) = result else {
            return Ok(FetchTopicMessagesResponse { data: vec![] });
        };

        let data = entry
            .data
            .iter()
            .skip(req.skip as usize)
            .map(|v| v.decode())
            .filter_map(|v| v.ok())
            .collect::<Vec<String>>();

        Ok(FetchTopicMessagesResponse { data })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<RegisterServiceRequest, RegisterServiceResponse> for Processor {
    async fn handle_rpc(&self, req: RegisterServiceRequest) -> Result<RegisterServiceResponse> {
        self.register_service(&req.name).await?;
        Ok(RegisterServiceResponse {})
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<LookupServiceRequest, LookupServiceResponse> for Processor {
    async fn handle_rpc(&self, req: LookupServiceRequest) -> Result<LookupServiceResponse> {
        let entry_key = Entry::gen_did(&req.name)
            .map_err(|_| Error::invalid_params("Failed to get id of topic"))?;

        self.storage_fetch(entry_key).await?;
        let result = self.storage_check_cache(entry_key).await;

        let Some(entry) = result else {
            return Ok(LookupServiceResponse { dids: vec![] });
        };

        let dids = entry
            .data
            .iter()
            .map(|v| v.decode())
            .filter_map(|v| v.ok())
            .collect::<Vec<String>>();

        Ok(LookupServiceResponse { dids })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<LookupOnlineNodesRequest, LookupOnlineNodesResponse> for Processor {
    async fn handle_rpc(&self, req: LookupOnlineNodesRequest) -> Result<LookupOnlineNodesResponse> {
        let nodes = self
            .lookup_online_nodes(req.include_expired)
            .await
            .map_err(Error::from)?;
        Ok(LookupOnlineNodesResponse {
            nodes: crate::rpc_dto::online_node_descriptor_infos(nodes)?,
        })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<LookupOnionExitsRequest, LookupOnionExitsResponse> for Processor {
    async fn handle_rpc(&self, req: LookupOnionExitsRequest) -> Result<LookupOnionExitsResponse> {
        let exits = self
            .lookup_onion_exits(&req.service, req.include_expired)
            .await
            .map_err(Error::from)?;
        Ok(LookupOnionExitsResponse {
            exits: crate::rpc_dto::onion_exit_descriptor_infos(exits)?,
        })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<BuildOnionRouteRequest, BuildOnionRouteResponse> for Processor {
    async fn handle_rpc(&self, req: BuildOnionRouteRequest) -> Result<BuildOnionRouteResponse> {
        let route = self
            .build_onion_route(req.service, req.hop_count as usize, req.allow_short_paths)
            .await
            .map_err(Error::from)?;
        crate::rpc_dto::onion_route_response(route).map_err(Error::from)
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<NodeInfoRequest, NodeInfoResponse> for Processor {
    async fn handle_rpc(&self, _req: NodeInfoRequest) -> Result<NodeInfoResponse> {
        self.get_node_info()
            .await
            .map_err(|_| Error::new(ErrorCode::InternalError))
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<PeerMeasurementRequest, PeerMeasurementResponse> for Processor {
    async fn handle_rpc(&self, req: PeerMeasurementRequest) -> Result<PeerMeasurementResponse> {
        let did = s2d(&req.did)?;
        Ok(PeerMeasurementResponse {
            measurement: crate::rpc_dto::optional_peer_measurement_info(
                self.peer_measurement(did).await,
            )?,
        })
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<ListPeerMeasurementsRequest, ListPeerMeasurementsResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: ListPeerMeasurementsRequest,
    ) -> Result<ListPeerMeasurementsResponse> {
        let (after, limit) = peer_measurement_page_request(&req)?;
        let page = self.peer_measurements_page(after, limit).await;
        Ok(ListPeerMeasurementsResponse {
            measurements: crate::rpc_dto::peer_measurement_infos(page.measurements)?,
            next_cursor: page.next_cursor.map(|did| did.to_string()),
        })
    }
}

fn peer_measurement_page_request(
    request: &ListPeerMeasurementsRequest,
) -> Result<(Option<Did>, NonZeroUsize)> {
    let requested = request.limit.unwrap_or(DEFAULT_PEER_MEASUREMENT_PAGE_SIZE);
    let limit = usize::try_from(requested)
        .ok()
        .and_then(NonZeroUsize::new)
        .filter(|limit| limit.get() <= MAX_PEER_MEASUREMENT_PAGE_SIZE as usize)
        .ok_or_else(|| {
            Error::invalid_params(format!(
                "measurement page limit must be between 1 and {MAX_PEER_MEASUREMENT_PAGE_SIZE}"
            ))
        })?;
    let after = request.cursor.as_deref().map(s2d).transpose()?;
    Ok((after, limit))
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl HandleRpc<NodeDidRequest, NodeDidResponse> for Processor {
    async fn handle_rpc(&self, _req: NodeDidRequest) -> Result<NodeDidResponse> {
        let did = self.did();
        Ok(NodeDidResponse {
            did: did.to_string(),
        })
    }
}

/// Get did from string or return InvalidParam Error
fn s2d(s: &str) -> Result<Did> {
    Did::from_str(s).map_err(|_| Error::invalid_params(format!("Invalid Did: {s}")))
}

fn s2pk(s: &str) -> Result<PublicKey<33>> {
    PublicKey::try_from_b58m(s)
        .or_else(|_| PublicKey::from_hex_string(s))
        .map_err(|_| Error::invalid_params("Invalid secp256k1 public key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_measurement_page_request_applies_defaults_and_parses_cursor() {
        let request = ListPeerMeasurementsRequest {
            limit: None,
            cursor: Some(Did::from(2_u32).to_string()),
        };
        let (after, limit) = peer_measurement_page_request(&request)
            .unwrap_or_else(|error| panic!("bounded page request must parse: {error}"));
        assert_eq!(after, Some(Did::from(2_u32)));
        assert_eq!(limit.get(), DEFAULT_PEER_MEASUREMENT_PAGE_SIZE as usize);
    }

    #[test]
    fn peer_measurement_page_rejects_unbounded_or_zero_limit() {
        let too_large = ListPeerMeasurementsRequest {
            limit: Some(MAX_PEER_MEASUREMENT_PAGE_SIZE + 1),
            cursor: None,
        };
        let zero = ListPeerMeasurementsRequest {
            limit: Some(0),
            cursor: None,
        };
        assert!(peer_measurement_page_request(&too_large).is_err());
        assert!(peer_measurement_page_request(&zero).is_err());
    }
}

#[cfg(all(test, feature = "node", not(target_family = "wasm")))]
mod signaling_underlay_tests {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn literal_ipv4_signaling_endpoint_needs_no_dns() {
        let targets = resolve_http_signaling_targets("http://203.0.113.7:50000")
            .await
            .expect("literal IPv4 signaling target");

        assert_eq!(targets, vec![IpAddr::V4([203, 0, 113, 7].into())]);
    }

    #[tokio::test]
    async fn signaling_endpoint_rejects_non_http_schemes() {
        assert!(resolve_http_signaling_targets("stun://203.0.113.7:3478")
            .await
            .is_err());
    }

    #[test]
    fn signaling_targets_are_ipv4_only_sorted_and_deduplicated() {
        let targets = ipv4_signaling_targets("http://seed.invalid", [
            IpAddr::V4([203, 0, 113, 9].into()),
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            IpAddr::V4([203, 0, 113, 7].into()),
            IpAddr::V4([203, 0, 113, 9].into()),
        ])
        .expect("IPv4 targets");

        assert_eq!(targets, vec![
            IpAddr::V4([203, 0, 113, 7].into()),
            IpAddr::V4([203, 0, 113, 9].into())
        ]);
    }

    #[tokio::test]
    async fn named_signaling_client_uses_only_the_pinned_target() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test listener");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept pinned request");
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.expect("read request");
            let body = r#"{"jsonrpc":"2.0","result":{"did":"pinned-target"},"id":1}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        let endpoint = format!("http://seed.example.invalid:{port}/rpc");
        let targets = [IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];
        let http_client =
            pinned_signaling_http_client(&endpoint, &targets).expect("pinned signaling client");
        let client = rings_rpc::jsonrpc::Client::with_http_client(&endpoint, http_client);

        let response = client
            .node_did(&NodeDidRequest {})
            .await
            .expect("RPC through pinned address");

        assert_eq!(response.did, "pinned-target");
        server.await.expect("test server task");
    }

    #[tokio::test]
    async fn signaling_client_rejects_unadmitted_redirect_destination() {
        let destination = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("redirect destination listener");
        let destination_port = destination
            .local_addr()
            .expect("redirect destination address")
            .port();
        let origin = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("redirect origin listener");
        let origin_port = origin.local_addr().expect("redirect origin address").port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.expect("accept origin request");
            let mut request = [0_u8; 2048];
            let _ = stream
                .read(&mut request)
                .await
                .expect("read origin request");
            let response = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{destination_port}/rpc\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write redirect");
        });
        let endpoint = format!("http://seed.example.invalid:{origin_port}/rpc");
        let targets = [IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];
        let http_client =
            pinned_signaling_http_client(&endpoint, &targets).expect("pinned signaling client");
        let client = rings_rpc::jsonrpc::Client::with_http_client(&endpoint, http_client);

        assert!(client.node_did(&NodeDidRequest {}).await.is_err());
        server.await.expect("redirect server task");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), destination.accept())
                .await
                .is_err()
        );
    }
}
