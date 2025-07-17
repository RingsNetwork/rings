use std::sync::Arc;

use async_trait::async_trait;
use jsonrpc_core::types::error::Error;
use jsonrpc_core::types::error::ErrorCode;
use jsonrpc_core::Result;

use super::rings_node::*;
use crate::method::Method;

/// Used for processor to match rpc request and response.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait HandleRpc<Req, Resp> {
    /// Handle rpc request and return response.
    async fn handle_rpc(&self, req: Req) -> Result<Resp>;
}

/// Provide handle_request method for internal rpc api.
#[derive(Clone, Copy)]
pub struct InternalRpcHandler;

/// Provide handle_request method for external rpc api.
#[derive(Clone, Copy)]
pub struct ExternalRpcHandler;

impl InternalRpcHandler {
    /// Handle rpc request.
    pub async fn handle_request<P>(
        &self,
        processor: Arc<P>,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value>
    where
        P: HandleRpc<ConnectPeerViaHttpRequest, ConnectPeerViaHttpResponse>
            + HandleRpc<ConnectWithDidRequest, ConnectWithDidResponse>
            + HandleRpc<ConnectWithSeedRequest, ConnectWithSeedResponse>
            + HandleRpc<ListPeersRequest, ListPeersResponse>
            + HandleRpc<CreateOfferRequest, CreateOfferResponse>
            + HandleRpc<AnswerOfferRequest, AnswerOfferResponse>
            + HandleRpc<AcceptAnswerRequest, AcceptAnswerResponse>
            + HandleRpc<DisconnectRequest, DisconnectResponse>
            + HandleRpc<SendCustomMessageRequest, SendCustomMessageResponse>
            + HandleRpc<SendBackendMessageRequest, SendBackendMessageResponse>
            + HandleRpc<PublishMessageToTopicRequest, PublishMessageToTopicResponse>
            + HandleRpc<FetchTopicMessagesRequest, FetchTopicMessagesResponse>
            + HandleRpc<RegisterServiceRequest, RegisterServiceResponse>
            + HandleRpc<LookupServiceRequest, LookupServiceResponse>
            + HandleRpc<NodeInfoRequest, NodeInfoResponse>
            + HandleRpc<NodeDidRequest, NodeDidResponse>,
    {
        let method = Method::try_from(method.as_str()).map_err(|_| Error {
            code: ErrorCode::MethodNotFound,
            message: format!("method {method} is not found"),
            data: None,
        })?;

        match method {
            Method::ConnectPeerViaHttp => {
                let req = serde_json::from_value::<ConnectPeerViaHttpRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::ConnectWithDid => {
                let req = serde_json::from_value::<ConnectWithDidRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::ConnectWithSeed => {
                let req = serde_json::from_value::<ConnectWithSeedRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::ListPeers => {
                let req = serde_json::from_value::<ListPeersRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::CreateOffer => {
                let req = serde_json::from_value::<CreateOfferRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::AnswerOffer => {
                let req = serde_json::from_value::<AnswerOfferRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::AcceptAnswer => {
                let req = serde_json::from_value::<AcceptAnswerRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::Disconnect => {
                let req = serde_json::from_value::<DisconnectRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::SendCustomMessage => {
                let req = serde_json::from_value::<SendCustomMessageRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::SendBackendMessage => {
                let req = serde_json::from_value::<SendBackendMessageRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::PublishMessageToTopic => {
                let req = serde_json::from_value::<PublishMessageToTopicRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::FetchTopicMessages => {
                let req = serde_json::from_value::<FetchTopicMessagesRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::RegisterService => {
                let req = serde_json::from_value::<RegisterServiceRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::LookupService => {
                let req = serde_json::from_value::<LookupServiceRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::NodeInfo => {
                let req = serde_json::from_value::<NodeInfoRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::NodeDid => {
                let req = serde_json::from_value::<NodeDidRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
        }
    }
}

impl ExternalRpcHandler {
    /// Handle rpc request.
    pub async fn handle_request<P>(
        &self,
        processor: Arc<P>,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value>
    where
        P: HandleRpc<AnswerOfferRequest, AnswerOfferResponse>
            + HandleRpc<NodeInfoRequest, NodeInfoResponse>
            + HandleRpc<NodeDidRequest, NodeDidResponse>,
    {
        let method = Method::try_from(method.as_str()).map_err(|_| Error {
            code: ErrorCode::MethodNotFound,
            message: format!("method {method} is not found"),
            data: None,
        })?;

        match method {
            Method::AnswerOffer => {
                let req = serde_json::from_value::<AnswerOfferRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::NodeInfo => {
                let req = serde_json::from_value::<NodeInfoRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            Method::NodeDid => {
                let req = serde_json::from_value::<NodeDidRequest>(params)
                    .map_err(|e| Error::invalid_params(e.to_string()))?;
                let resp = processor.handle_rpc(req).await?;
                serde_json::to_value(resp).map_err(|_| Error::new(ErrorCode::ParseError))
            }
            _ => Err(Error {
                code: ErrorCode::InvalidRequest,
                message: format!("method {} is not allowed", method.as_str()),
                data: None,
            }),
        }
    }
}
