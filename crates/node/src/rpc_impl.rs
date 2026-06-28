#![warn(missing_docs)]

//! RPC handler for both feature=browser and feature=node.
//! We support handling the RPC request in either native or browser environment by `InternalRpcHandler` and `ExternalRpcHandler` from rings_rpc crate.
//! For the native environment, we use jsonrpc_core to handle requests.
//! For the browser environment, we use `InternalRpcHandler` to process the requests.

use std::collections::HashSet;
use std::str::FromStr;

use async_trait::async_trait;
use futures::future::join_all;
use jsonrpc_core::types::error::Error;
use jsonrpc_core::types::error::ErrorCode;
use jsonrpc_core::Result;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::message::e2e;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;
use rings_core::prelude::entry::Entry;
use rings_rpc::protos::rings_node::*;
use rings_rpc::protos::rings_node_handler::HandleRpc;

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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<ConnectWithDidRequest, ConnectWithDidResponse> for Processor {
    async fn handle_rpc(&self, req: ConnectWithDidRequest) -> Result<ConnectWithDidResponse> {
        let did = s2d(&req.did)?;
        self.connect_with_did(did).await.map_err(Error::from)?;
        Ok(ConnectWithDidResponse {})
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
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
        answer_payload.transaction.signer();

        self.swarm
            .accept_answer(answer_payload)
            .await
            .map_err(ServerError::AcceptAnswer)?;

        Ok(AcceptAnswerResponse {})
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

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl HandleRpc<SendE2eHandshakeRequest, SendE2eHandshakeResponse> for Processor {
    async fn handle_rpc(&self, req: SendE2eHandshakeRequest) -> Result<SendE2eHandshakeResponse> {
        let destination = s2d(&req.destination_did)?;
        let tx_id = self.send_e2e_handshake(destination).await?;
        Ok(SendE2eHandshakeResponse {
            tx_id: tx_id.to_string(),
        })
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
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

/// Get did from string or return InvalidParam Error
fn s2d(s: &str) -> Result<Did> {
    Did::from_str(s).map_err(|_| Error::invalid_params(format!("Invalid Did: {s}")))
}

fn s2pk(s: &str) -> Result<PublicKey<33>> {
    PublicKey::try_from_b58m(s)
        .or_else(|_| PublicKey::from_hex_string(s))
        .map_err(|_| Error::invalid_params("Invalid secp256k1 public key"))
}
