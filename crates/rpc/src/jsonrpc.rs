//! rings-rpc client

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::method::Method;
use crate::prelude::reqwest::Client as HttpClient;
use crate::protos::rings_node::*;

/// Wrap json_client send request between nodes or browsers.
pub struct Client {
    client: HttpClient,
    endpoint_url: String,
}

/// The errors returned by the client.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// An error returned by the server.
    #[error("Server returned rpc error {0}")]
    JsonClientError(jsonrpc_core::Error),
    /// Failure to parse server response.
    #[error("Failed to parse server response as {0}: {1}")]
    ParseError(String, Box<dyn std::error::Error + Send>),
    /// Request timed out.
    #[error("Request timed out")]
    Timeout,
    /// A general client error.
    #[error("Client error: {0}")]
    Client(String),
    /// Not rpc specific errors.
    #[error("{0}")]
    Other(Box<dyn std::error::Error + Send>),
}

/// A wrap `Result` contains ClientError.
type Result<T> = std::result::Result<T, RpcError>;

impl Client {
    /// Creates a new Client instance with the specified endpoint URL
    pub fn new(endpoint_url: &str) -> Self {
        Self {
            client: HttpClient::default(),
            endpoint_url: endpoint_url.to_string(),
        }
    }

    pub async fn call_method<T>(&self, method: Method, req: &impl Serialize) -> Result<T>
    where T: DeserializeOwned {
        use jsonrpc_core::*;

        let params = serde_json::to_value(req)
            .map_err(|e| RpcError::Client(e.to_string()))?
            .as_object()
            .ok_or(RpcError::Client("params should be an object".to_string()))?
            .clone();

        let jsonrpc_request = Request::Single(Call::MethodCall(MethodCall {
            jsonrpc: Some(Version::V2),
            method: method.to_string(),
            params: Params::Map(params),
            id: Id::Num(1),
        }));

        let result = self.do_jsonrpc_request(&jsonrpc_request).await?;
        serde_json::from_value(result).map_err(|e| RpcError::ParseError(e.to_string(), Box::new(e)))
    }

    async fn do_jsonrpc_request(&self, req: &jsonrpc_core::Request) -> Result<serde_json::Value> {
        let body = serde_json::to_string(req).map_err(|e| RpcError::Client(e.to_string()))?;

        let req = self
            .client
            .post(self.endpoint_url.as_str())
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(body);

        let resp = req
            .send()
            .await
            .map_err(|e| RpcError::Client(e.to_string()))?
            .error_for_status()
            .map_err(|e| RpcError::Client(e.to_string()))?
            .bytes()
            .await
            .map_err(|e| RpcError::ParseError(e.to_string(), Box::new(e)))?;

        let jsonrpc_resp = jsonrpc_core::Response::from_json(&String::from_utf8_lossy(&resp))
            .map_err(|e| RpcError::ParseError(e.to_string(), Box::new(e)))?;

        match jsonrpc_resp {
            jsonrpc_core::Response::Single(resp) => match resp {
                jsonrpc_core::Output::Success(success) => Ok(success.result),
                jsonrpc_core::Output::Failure(failure) => {
                    Err(RpcError::JsonClientError(failure.error))
                }
            },
            jsonrpc_core::Response::Batch(_) => Err(RpcError::Client(
                "Batch response is not supported".to_string(),
            )),
        }
    }

    /// Establishes a WebRTC connection with a remote peer using HTTP as the signaling channel.
    ///
    /// This function allows two peers to establish a WebRTC connection using HTTP,
    /// which can be useful in scenarios where a direct peer-to-peer connection is not possible due to firewall restrictions or other network issues.
    /// The function sends ICE candidates and Session Description Protocol (SDP) messages over HTTP as a form of signaling to establish the connection.
    ///
    /// Takes a URL for an HTTP server that will be used as the signaling channel to exchange ICE candidates and SDP with the remote peer.
    /// Returns a Did that can be used to refer to this connection in subsequent WebRTC operations.
    pub async fn connect_peer_via_http(
        &self,
        req: &ConnectPeerViaHttpRequest,
    ) -> Result<ConnectPeerViaHttpResponse> {
        self.call_method(Method::ConnectPeerViaHttp, req).await
    }

    /// Attempts to connect to a peer using a DID stored in a Distributed Hash Table (DHT).
    pub async fn connect_with_did(
        &self,
        req: &ConnectWithDidRequest,
    ) -> Result<ConnectWithSeedResponse> {
        self.call_method(Method::ConnectWithDid, req).await
    }

    /// Attempts to connect to a peer using a seed file located at the specified source path.
    pub async fn connect_with_seed(
        &self,
        req: &ConnectWithSeedRequest,
    ) -> Result<ConnectWithSeedResponse> {
        self.call_method(Method::ConnectWithSeed, req).await
    }

    /// Lists all connected peers and their status.
    ///
    /// Returns an Output containing a formatted string representation of the list of peers if successful, or an anyhow::Error if an error occurred.
    pub async fn list_peers(&self, req: &ListPeersRequest) -> Result<ListPeersResponse> {
        self.call_method(Method::ListPeers, req).await
    }

    pub async fn create_offer(&self, req: &CreateOfferRequest) -> Result<CreateOfferResponse> {
        self.call_method(Method::CreateOffer, req).await
    }

    pub async fn answer_offer(&self, req: &AnswerOfferRequest) -> Result<AnswerOfferResponse> {
        self.call_method(Method::AnswerOffer, req).await
    }

    pub async fn accept_answer(&self, req: &AcceptAnswerRequest) -> Result<AcceptAnswerResponse> {
        self.call_method(Method::AcceptAnswer, req).await
    }

    /// Disconnects from the peer with the specified DID.
    pub async fn disconnect(&self, req: &DisconnectRequest) -> Result<DisconnectResponse> {
        self.call_method(Method::Disconnect, req).await
    }

    /// Sends a custom message to the specified peer.
    pub async fn send_custom_message(
        &self,
        req: &SendCustomMessageRequest,
    ) -> Result<SendCustomMessageResponse> {
        self.call_method(Method::SendCustomMessage, req).await
    }

    pub async fn send_backend_message(
        &self,
        req: &SendBackendMessageRequest,
    ) -> Result<SendBackendMessageResponse> {
        self.call_method(Method::SendBackendMessage, req).await
    }

    /// Publishes a message to the specified topic.
    pub async fn publish_message_to_topic(
        &self,
        req: &PublishMessageToTopicRequest,
    ) -> Result<PublishMessageToTopicResponse> {
        self.call_method(Method::PublishMessageToTopic, req).await
    }

    pub async fn fetch_topic_messages(
        &self,
        req: &FetchTopicMessagesRequest,
    ) -> Result<FetchTopicMessagesResponse> {
        self.call_method(Method::FetchTopicMessages, req).await
    }

    /// Registers a new service with the given name.
    pub async fn register_service(
        &self,
        req: &RegisterServiceRequest,
    ) -> Result<RegisterServiceResponse> {
        self.call_method(Method::RegisterService, req).await
    }

    /// Looks up the DIDs of services registered with the given name.
    pub async fn lookup_service(
        &self,
        req: &LookupServiceRequest,
    ) -> Result<LookupServiceResponse> {
        self.call_method(Method::LookupService, req).await
    }

    /// Query for swarm inspect info.
    pub async fn node_info(&self, req: &NodeInfoRequest) -> Result<NodeInfoResponse> {
        self.call_method(Method::NodeInfo, req).await
    }

    pub async fn node_did(&self, req: &NodeDidRequest) -> Result<NodeDidResponse> {
        self.call_method(Method::NodeDid, req).await
    }
}
