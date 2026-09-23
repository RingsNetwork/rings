//! rings-rpc client

/// Destination-policy and transport regression witnesses.
#[cfg(test)]
mod tests;
/// Credential transport policy and platform-specific redirect controls.
mod transport;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::method::Method;
use crate::prelude::reqwest::Client as HttpClient;
use crate::protos::rings_node::*;

/// Wrap json_client send request between nodes or browsers.
pub struct Client {
    /// Transport with redirects disabled on native targets.
    client: HttpClient,
    /// Caller-selected endpoint, validated before attaching a credential.
    endpoint_url: String,
    /// Credential attached only after endpoint policy validation.
    bearer_token: Option<String>,
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
    /// The URL cannot be parsed as an authenticated RPC endpoint.
    #[error("Invalid authenticated RPC endpoint")]
    InvalidAuthenticatedEndpoint,
    /// Credentials require HTTPS or a literal loopback HTTP address.
    #[error("Authenticated RPC requires HTTPS or HTTP on a literal loopback IP address")]
    InsecureAuthenticatedEndpoint,
    /// A transport could not be constructed.
    #[error("Failed to build RPC transport: {0}")]
    TransportBuild(#[source] reqwest::Error),
    /// Redirects are not permitted for RPC requests.
    #[error("RPC redirects are not permitted")]
    RedirectRejected,
    /// Browser Fetch rejected the request without exposing a secret-bearing error.
    #[cfg(target_family = "wasm")]
    #[error("Authenticated browser RPC request failed")]
    BrowserTransport,
    /// Not rpc specific errors.
    #[error("{0}")]
    Other(Box<dyn std::error::Error + Send>),
}

/// A wrap `Result` contains ClientError.
type Result<T> = std::result::Result<T, RpcError>;

impl Client {
    /// Creates an RPC client with redirects disabled on native targets.
    ///
    /// Native transports bypass proxies so the loopback HTTP exception cannot send
    /// credentials through an ambient proxy. Browser credentials use Fetch with
    /// redirect mode `error` at the send boundary.
    pub fn new(endpoint_url: &str) -> Result<Self> {
        Self::with_http_client_builder(endpoint_url, HttpClient::builder())
    }

    /// Builds an RPC transport while retaining caller DNS pins, timeouts and TLS roots.
    ///
    /// Native proxy and redirect settings are always overridden: authenticated
    /// traffic must stay on the endpoint whose policy was checked. Accepting a
    /// builder, rather than an opaque client, makes those controls enforceable.
    /// Authenticated browser requests use browser-controlled Fetch instead of this transport.
    pub fn with_http_client_builder(
        endpoint_url: &str,
        builder: reqwest::ClientBuilder,
    ) -> Result<Self> {
        Ok(Self {
            client: transport::build_client(builder)?,
            endpoint_url: endpoint_url.to_string(),
            bearer_token: None,
        })
    }

    /// Attaches a credential only to HTTPS or literal loopback HTTP endpoints.
    ///
    /// HTTP hostnames, including `localhost`, are rejected rather than trusting
    /// DNS to preserve the loopback exception. URL userinfo is not accepted.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Result<Self> {
        transport::authenticated_endpoint(&self.endpoint_url)?;
        self.bearer_token = Some(token.into());
        Ok(self)
    }

    /// Sends a typed JSON-RPC request and decodes the typed response body.
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

        // The platform adapter owns endpoint validation and redirect enforcement.
        let resp = transport::send(
            &self.client,
            &self.endpoint_url,
            self.bearer_token.as_deref(),
            body,
        )
        .await?;

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
    /// The function sends ICE candidates and Delegation Description Protocol (SDP) messages over HTTP as a form of signaling to establish the connection.
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

    /// Creates a WebRTC offer for a manual peer handshake.
    pub async fn create_offer(&self, req: &CreateOfferRequest) -> Result<CreateOfferResponse> {
        self.call_method(Method::CreateOffer, req).await
    }

    /// Answers a WebRTC offer with a local session description.
    pub async fn answer_offer(&self, req: &AnswerOfferRequest) -> Result<AnswerOfferResponse> {
        self.call_method(Method::AnswerOffer, req).await
    }

    /// Accepts a WebRTC answer and completes the manual handshake.
    pub async fn accept_answer(&self, req: &AcceptAnswerRequest) -> Result<AcceptAnswerResponse> {
        self.call_method(Method::AcceptAnswer, req).await
    }

    /// Disconnects from the peer with the specified DID.
    pub async fn disconnect(&self, req: &DisconnectRequest) -> Result<DisconnectResponse> {
        self.call_method(Method::Disconnect, req).await
    }

    /// Sends a namespace-scoped backend message to a destination DID.
    pub async fn send_backend_message(
        &self,
        req: &SendBackendMessageRequest,
    ) -> Result<SendBackendMessageResponse> {
        self.call_method(Method::SendBackendMessage, req).await
    }

    /// Starts an end-to-end encrypted handshake with a destination DID.
    pub async fn send_e2e_handshake(
        &self,
        req: &SendE2eHandshakeRequest,
    ) -> Result<SendE2eHandshakeResponse> {
        self.call_method(Method::SendE2eHandshake, req).await
    }

    /// Sends an encrypted end-to-end message stream to a destination DID.
    pub async fn send_e2e_message(
        &self,
        req: &SendE2eMessageRequest,
    ) -> Result<SendE2eMessageResponse> {
        self.call_method(Method::SendE2eMessage, req).await
    }

    /// Publishes a message to the specified topic.
    pub async fn publish_message_to_topic(
        &self,
        req: &PublishMessageToTopicRequest,
    ) -> Result<PublishMessageToTopicResponse> {
        self.call_method(Method::PublishMessageToTopic, req).await
    }

    /// Fetches stored messages for a topic after the requested offset.
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

    /// Looks up signed online-node descriptors.
    pub async fn lookup_online_nodes(
        &self,
        req: &LookupOnlineNodesRequest,
    ) -> Result<LookupOnlineNodesResponse> {
        self.call_method(Method::LookupOnlineNodes, req).await
    }

    /// Looks up signed onion-exit descriptors.
    pub async fn lookup_onion_exits(
        &self,
        req: &LookupOnionExitsRequest,
    ) -> Result<LookupOnionExitsResponse> {
        self.call_method(Method::LookupOnionExits, req).await
    }

    /// Query for swarm inspect info.
    pub async fn node_info(&self, req: &NodeInfoRequest) -> Result<NodeInfoResponse> {
        self.call_method(Method::NodeInfo, req).await
    }

    /// Query local measurement counters for a peer.
    pub async fn peer_measurement(
        &self,
        req: &PeerMeasurementRequest,
    ) -> Result<PeerMeasurementResponse> {
        self.call_method(Method::PeerMeasurement, req).await
    }

    /// Query local measurement counters for all connected peers.
    pub async fn list_peer_measurements(
        &self,
        req: &ListPeerMeasurementsRequest,
    ) -> Result<ListPeerMeasurementsResponse> {
        self.call_method(Method::ListPeerMeasurements, req).await
    }

    /// Returns the DID of the local node.
    pub async fn node_did(&self, req: &NodeDidRequest) -> Result<NodeDidResponse> {
        self.call_method(Method::NodeDid, req).await
    }
}
