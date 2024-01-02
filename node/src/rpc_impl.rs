#![warn(missing_docs)]
//! JSON-RPC handler for both feature=browser and feature=node.
//! We support running the JSON-RPC server in either native or browser environment.
//! For the native environment, we use jsonrpc_core to handle requests.
//! For the browser environment, we utilize a Simple MessageHandler to process the requests.
use std::collections::HashSet;
use std::str::FromStr;

use async_trait::async_trait;
use futures::future::join_all;
use jsonrpc_core::types::error::Error;
use jsonrpc_core::types::error::ErrorCode;
use jsonrpc_core::Result;
use rings_core::dht::Did;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessagePayload;
use rings_core::prelude::vnode::VirtualNode;
use rings_core::swarm::impls::ConnectionHandshake;
use rings_rpc::protos::rings_node::*;
use rings_rpc::protos::rings_node_handler::HandleRpc;
use rings_transport::core::transport::ConnectionInterface;

use crate::error::Error as ServerError;
use crate::processor::Processor;
use crate::seed::Seed;

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<ConnectPeerViaHttpRequest, ConnectPeerViaHttpResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: ConnectPeerViaHttpRequest,
    ) -> Result<ConnectPeerViaHttpResponse> {
        let client = rings_rpc::jsonrpc::Client::new(&req.url);

        let did = client
            .node_did(&NodeDidRequest {})
            .await
            .map_err(|e| ServerError::RemoteRpcError(e.to_string()))?
            .did;

        let offer = self.handle_rpc(CreateOfferRequest { did }).await?.offer;
        let encoded_offer = offer.encode().map_err(|_| ServerError::EncodeError)?;

        let answer = client
            .answer_offer(&AnswerOfferRequest {
                offer: encoded_offer.to_string(),
            })
            .await
            .map_err(|e| ServerError::RemoteRpcError(e.to_string()))?
            .answer;

        let peer = self.handle_rpc(AcceptAnswerRequest { answer }).await?.peer;

        Ok(ConnectPeerViaHttpResponse { peer })
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<ConnectWithDidRequest, ConnectWithDidResponse> for Processor {
    async fn handle_rpc(&self, req: ConnectWithDidRequest) -> Result<ConnectWithDidResponse> {
        let did = s2d(&req.did)?;
        self.connect_with_did(did, true)
            .await
            .map_err(Error::from)?;

        Ok(ConnectWithDidResponse {})
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<ConnectWithSeedRequest, ConnectWithSeedResponse> for Processor {
    async fn handle_rpc(&self, req: ConnectWithSeedRequest) -> Result<ConnectWithSeedResponse> {
        let seed: Seed = Seed::try_from(req)?;

        let mut connected: HashSet<Did> = HashSet::from_iter(self.swarm.get_connection_ids());
        connected.insert(self.swarm.did());

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
            err.map_err(Error::from)?;
        }

        Ok(ConnectWithSeedResponse {})
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<ListPeersRequest, ListPeersResponse> for Processor {
    async fn handle_rpc(&self, _req: ListPeersRequest) -> Result<ListPeersResponse> {
        let peers = self.swarm.get_connections().into_iter().map(dc2p).collect();
        Ok(ListPeersResponse { peers })
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<CreateOfferRequest, CreateOfferResponse> for Processor {
    async fn handle_rpc(&self, req: CreateOfferRequest) -> Result<CreateOfferResponse> {
        let did = s2d(&req.did)?;
        let (_, offer_payload) = self
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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<AnswerOfferRequest, AnswerOfferResponse> for Processor {
    async fn handle_rpc(&self, req: AnswerOfferRequest) -> Result<AnswerOfferResponse> {
        if req.offer.is_empty() {
            return Err(Error::invalid_params("Offer is empty"));
        }
        let encoded: Encoded = <Encoded as From<String>>::from(req.offer);

        let offer_payload =
            MessagePayload::from_encoded(&encoded).map_err(|_| ServerError::DecodeError)?;

        let (_, answer_payload) = self
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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<AcceptAnswerRequest, AcceptAnswerResponse> for Processor {
    async fn handle_rpc(&self, req: AcceptAnswerRequest) -> Result<AcceptAnswerResponse> {
        if req.answer.is_empty() {
            return Err(Error::invalid_params("Answer is empty"));
        }
        let encoded = Encoded::from(req.answer);

        let answer_payload =
            MessagePayload::from_encoded(&encoded).map_err(|_| ServerError::DecodeError)?;

        let dc = self
            .swarm
            .accept_answer(answer_payload)
            .await
            .map_err(ServerError::AcceptAnswer)?;

        Ok(AcceptAnswerResponse {
            peer: Some(dc2p(dc)),
        })
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<DisconnectRequest, DisconnectResponse> for Processor {
    async fn handle_rpc(&self, req: DisconnectRequest) -> Result<DisconnectResponse> {
        let did = s2d(&req.did)?;
        self.disconnect(did).await?;
        Ok(DisconnectResponse {})
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<SendCustomMessageRequest, SendCustomMessageResponse> for Processor {
    async fn handle_rpc(&self, req: SendCustomMessageRequest) -> Result<SendCustomMessageResponse> {
        let destination = s2d(&req.destination_did)?;
        let data = base64::decode(req.data)
            .map_err(|_| Error::invalid_params("Base64 decode data failed"))?;
        self.send_message(destination, &data).await?;
        Ok(SendCustomMessageResponse {})
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<SendBackendMessageRequest, SendBackendMessageResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: SendBackendMessageRequest,
    ) -> Result<SendBackendMessageResponse> {
        let destination = s2d(&req.destination_did)?;
        let data = serde_json::from_str(&req.data)
            .map_err(|_| Error::invalid_params("Serialize data as json failed"))?;
        self.send_backend_message(destination, data).await?;
        Ok(SendBackendMessageResponse {})
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<FetchTopicMessagesRequest, FetchTopicMessagesResponse> for Processor {
    async fn handle_rpc(
        &self,
        req: FetchTopicMessagesRequest,
    ) -> Result<FetchTopicMessagesResponse> {
        let vid = VirtualNode::gen_did(&req.topic)
            .map_err(|_| Error::invalid_params("Failed to get id of topic"))?;

        self.storage_fetch(vid).await?;
        let result = self.storage_check_cache(vid).await;

        let Some(vnode) = result else {
            return Ok(FetchTopicMessagesResponse { data: vec![] });
        };

        let data = vnode
            .data
            .iter()
            .skip(req.skip as usize)
            .map(|v| v.decode())
            .filter_map(|v| v.ok())
            .collect::<Vec<String>>();

        Ok(FetchTopicMessagesResponse { data })
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<RegisterServiceRequest, RegisterServiceResponse> for Processor {
    async fn handle_rpc(&self, req: RegisterServiceRequest) -> Result<RegisterServiceResponse> {
        self.register_service(&req.name).await?;
        Ok(RegisterServiceResponse {})
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<LookupServiceRequest, LookupServiceResponse> for Processor {
    async fn handle_rpc(&self, req: LookupServiceRequest) -> Result<LookupServiceResponse> {
        let vid = VirtualNode::gen_did(&req.name)
            .map_err(|_| Error::invalid_params("Failed to get id of topic"))?;

        self.storage_fetch(vid).await?;
        let result = self.storage_check_cache(vid).await;

        let Some(vnode) = result else {
            return Ok(LookupServiceResponse { dids: vec![] });
        };

        let dids = vnode
            .data
            .iter()
            .map(|v| v.decode())
            .filter_map(|v| v.ok())
            .collect::<Vec<String>>();

        Ok(LookupServiceResponse { dids })
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<NodeInfoRequest, NodeInfoResponse> for Processor {
    async fn handle_rpc(&self, _req: NodeInfoRequest) -> Result<NodeInfoResponse> {
        self.get_node_info()
            .await
            .map_err(|_| Error::new(ErrorCode::InternalError))
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<NodeDidRequest, NodeDidResponse> for Processor {
    async fn handle_rpc(&self, _req: NodeDidRequest) -> Result<NodeDidResponse> {
        let did = self.did();
        Ok(NodeDidResponse {
            did: did.to_string(),
        })
    }
}

/// Convert did and connection to Peer
fn dc2p((did, conn): (Did, impl ConnectionInterface)) -> PeerInfo {
    PeerInfo {
        did: did.to_string(),
        state: format!("{:?}", conn.webrtc_connection_state()),
    }
}

/// Get did from string or return InvalidParam Error
fn s2d(s: &str) -> Result<Did> {
    Did::from_str(s).map_err(|_| Error::invalid_params(format!("Invalid Did: {s}")))
}
