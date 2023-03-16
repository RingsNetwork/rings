#![warn(missing_docs)]
//! # ring-node-client
//!
//! ring-node-client is a command-line tool for interacting with the Ring Node backend API. It allows users to establish WebRTC connections with remote peers, send and receive messages, and publish and subscribe to topics.
//!
//! ## Usage
//!
//! To use ring-node-client, simply create a new instance of the Client struct, passing in the endpoint URL and signature as arguments. Then, use the various methods on the Client instance to perform the desired actions.
//!
//! # Features
//!
//! - Establish WebRTC connections with remote peers using HTTP as a signaling channel.
//! - Send and receive messages using WebRTC.
//! - Publish and subscribe to topics.
//! - Register and lookup DIDs of services.
//! - Send HTTP requests to remote peers.
//! - Load a seed file to establish a connection with a remote peer.

use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use bytes::Bytes;
use futures::pin_mut;
use futures::select;
use futures::FutureExt;
use futures::Stream;
use futures_timer::Delay;
use http::header;
use jsonrpc_core::Params;
use jsonrpc_core::Value;
use serde_json::json;

use crate::backend::types::HttpRequest;
use crate::backend::types::Timeout;
use crate::jsonrpc;
use crate::jsonrpc::method::Method;
use crate::jsonrpc::response::Peer;
use crate::jsonrpc::response::TransportAndIce;
use crate::jsonrpc_client::SimpleClient;
use crate::prelude::reqwest;
use crate::prelude::rings_core::inspect::SwarmInspect;
use crate::processor::NodeInfo;
use crate::seed::Seed;
use crate::util::loader::ResourceLoader;

/// Alias about Result<ClientOutput<T>, E>.
type Output<T> = anyhow::Result<ClientOutput<T>>;

/// Wrap json_client send request between nodes or browsers.
#[derive(Clone)]
pub struct Client {
    client: SimpleClient,
}

/// Wrap client output contain raw result and humanreadable display.
pub struct ClientOutput<T> {
    /// Output data.
    pub result: T,
    display: String,
}

impl Client {
    /// Creates a new Client instance with the specified endpoint URL and signature.
    pub async fn new(endpoint_url: &str, signature: &str) -> anyhow::Result<Self> {
        let mut default_headers = reqwest::header::HeaderMap::default();
        default_headers.insert("X-SIGNATURE", header::HeaderValue::from_str(signature)?);
        let client = SimpleClient::new(
            Arc::new(
                reqwest::Client::builder()
                    .default_headers(default_headers)
                    .build()?,
            ),
            endpoint_url,
        );
        Ok(Self { client })
    }

    /// Establishes a WebRTC connection with a remote peer using HTTP as the signaling channel.
    ///
    /// This function allows two peers to establish a WebRTC connection using HTTP,
    /// which can be useful in scenarios where a direct peer-to-peer connection is not possible due to firewall restrictions or other network issues.
    /// The function sends ICE candidates and Session Description Protocol (SDP) messages over HTTP as a form of signaling to establish the connection.
    ///
    /// Takes a URL for an HTTP server that will be used as the signaling channel to exchange ICE candidates and SDP with the remote peer.
    /// Returns a transport ID that can be used to refer to this connection in subsequent WebRTC operations.
    pub async fn connect_peer_via_http(&mut self, http_url: &str) -> Output<String> {
        let resp = self
            .client
            .call_method(
                Method::ConnectPeerViaHttp.as_str(),
                Params::Array(vec![Value::String(http_url.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let transport_id = resp
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Unexpected response"))?;

        ClientOutput::ok(
            format!("Your transport_id: {}", transport_id),
            transport_id.to_string(),
        )
    }

    /// Attempts to connect to a peer using a seed file located at the specified source path.
    pub async fn connect_with_seed(&mut self, source: &str) -> Output<()> {
        let seed = Seed::load(source).await?;
        let seed_v = serde_json::to_value(seed).map_err(|_| anyhow::anyhow!("serialize failed"))?;

        self.client
            .call_method(
                Method::ConnectWithSeed.as_str(),
                Params::Array(vec![seed_v]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Successful!".to_string(), ())
    }

    /// Answers a WebRTC offer by providing ICE candidate information to the remote peer.
    ///
    /// The ice_info parameter is a string containing the ICE candidate information provided by the remote peer as part of the offer.
    ///
    /// Returns an Output containing a TransportAndIce struct if successful, or an anyhow::Error if an error occurred.
    pub async fn answer_offer(&mut self, ice_info: &str) -> Output<TransportAndIce> {
        let resp = self
            .client
            .call_method(
                Method::AnswerOffer.as_str(),
                Params::Array(vec![Value::String(ice_info.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(
            format!("transport_id: {}\nice: {}", info.transport_id, info.ice,),
            info,
        )
    }

    /// Attempts to connect to a peer using a DID stored in a Distributed Hash Table (DHT).
    pub async fn connect_with_did(&mut self, did: &str) -> Output<()> {
        self.client
            .call_method(
                Method::ConnectWithDid.as_str(),
                Params::Array(vec![Value::String(did.to_owned())]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Successful!".to_owned(), ())
    }

    /// Creates a WebRTC offer to establish a connection with a remote peer.
    pub async fn create_offer(&mut self) -> Output<TransportAndIce> {
        let resp = self
            .client
            .call_method(Method::CreateOffer.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(
            format!("\ntransport_id: {}\nice: {}", info.transport_id, info.ice),
            info,
        )
    }

    /// Accepts a WebRTC answer from a remote peer, providing the transport_id and ICE candidate information needed to establish the connection.
    pub async fn accept_answer(&mut self, transport_id: &str, ice: &str) -> Output<Peer> {
        let resp = self
            .client
            .call_method(
                Method::AcceptAnswer.as_str(),
                Params::Array(vec![json!(transport_id), json!(ice)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let peer: Peer = serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(format!("transport_id: {}", peer.transport_id), peer)
    }

    /// Lists all connected peers and their status.
    ///
    /// Returns an Output containing a formatted string representation of the list of peers if successful, or an anyhow::Error if an error occurred.
    pub async fn list_peers(&mut self) -> Output<()> {
        let resp = self
            .client
            .call_method(Method::ListPeers.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let peers: Vec<Peer> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        let mut display = String::new();
        display.push_str("Did, TransportId, Status\n");
        display.push_str(
            peers
                .iter()
                .map(|peer| format!("{}, {}, {}", peer.did, peer.transport_id, peer.state))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        );

        ClientOutput::ok(display, ())
    }

    /// Disconnects from the peer with the specified DID.
    pub async fn disconnect(&mut self, did: &str) -> Output<()> {
        self.client
            .call_method(Method::Disconnect.as_str(), Params::Array(vec![json!(did)]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Done.".into(), ())
    }

    /// Lists all pending transports and their status.
    pub async fn list_pendings(&self) -> Output<()> {
        let resp = self
            .client
            .call_method(Method::ListPendings.as_str(), Params::Array(vec![]))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let resp: Vec<jsonrpc::response::TransportInfo> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut display = String::new();
        display.push_str("TransportId, Status\n");
        for item in resp.iter() {
            display.push_str(format!("{}, {}", item.transport_id, item.state).as_str())
        }
        ClientOutput::ok(display, ())
    }

    /// Closes the pending transport with the specified transport ID.
    pub async fn close_pending_transport(&self, transport_id: &str) -> Output<()> {
        self.client
            .call_method(
                Method::ClosePendingTransport.as_str(),
                Params::Array(vec![json!(transport_id)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends a message to the specified peer.
    pub async fn send_message(&self, did: &str, text: &str) -> Output<()> {
        let mut params = serde_json::Map::new();
        params.insert("destination".to_owned(), json!(did));
        params.insert("text".to_owned(), json!(text));
        self.client
            .call_method(Method::SendTo.as_str(), Params::Map(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends a custom message to the specified peer.
    pub async fn send_custom_message(
        &self,
        did: &str,
        message_type: u16,
        data: &str,
    ) -> Output<()> {
        self.client
            .call_method(
                Method::SendCustomMessage.as_str(),
                Params::Array(vec![json!(did), json!(message_type), json!(data)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends an HTTP request message to the specified peer.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_http_request_message(
        &self,
        did: &str,
        name: &str,
        method: http::Method,
        url: &str,
        timeout: Timeout,
        headers: &[(&str, &str)],
        body: Option<String>,
    ) -> Output<()> {
        let http_request: HttpRequest =
            HttpRequest::new(name, method, url, timeout, headers, body.map(Bytes::from));
        let params2 = serde_json::to_value(http_request).map_err(|e| anyhow::anyhow!(e))?;
        self.client
            .call_method(
                Method::SendHttpRequestMessage.as_str(),
                Params::Array(vec![json!(did), params2]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends a simple text message to the specified peer.
    pub async fn send_simple_text_message(&self, did: &str, text: &str) -> Output<()> {
        self.client
            .call_method(
                Method::SendSimpleText.as_str(),
                Params::Array(vec![json!(did), json!(text)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Registers a new service with the given name.
    pub async fn register_service(&self, name: &str) -> Output<()> {
        self.client
            .call_method(
                Method::RegisterService.as_str(),
                Params::Array(vec![json!(name)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Looks up the DIDs of services registered with the given name.
    pub async fn lookup_service(&self, name: &str) -> Output<()> {
        let resp = self
            .client
            .call_method(
                Method::LookupService.as_str(),
                Params::Array(vec![json!(name)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let dids: Vec<String> =
            serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(dids.join("\n"), ())
    }

    /// Publishes a message to the specified topic.
    pub async fn publish_message_to_topic(&self, topic: &str, data: &str) -> Output<()> {
        self.client
            .call_method(
                Method::PublishMessageToTopic.as_str(),
                Params::Array(vec![json!(topic), json!(data)]),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Subscribes to the specified topic and returns a stream of messages published to the topic.
    pub async fn subscribe_topic<'a, 'b>(
        &'a self,
        topic: String,
    ) -> impl Stream<Item = String> + 'b
    where
        'a: 'b,
    {
        let mut index = 0;

        stream! {
        loop {
            let timeout = Delay::new(Duration::from_secs(5)).fuse();
            pin_mut!(timeout);

            select! {
                _ = timeout => {
                    let result = self
                        .client
                        .call_method(
                            Method::FetchMessagesOfTopic.as_str(),
                            Params::Array(vec![json!(topic), json!(index)]),
                        )
                        .await;

                    if let Err(e) = result {
                        tracing::error!("Failed to fetch messages of topic: {}, {}", topic, e);
                        continue;
                    }
                    let resp = result.unwrap();

                    let messages = serde_json::from_value(resp);
                    if let Err(e) = messages {
                        tracing::error!("Failed to parse messages of topic: {}, {}", topic, e);
                        continue;
                    }
                    let messages: Vec<String> = messages.unwrap();

                    for msg in messages.iter().cloned() {
                        yield msg
                    }
                    index += messages.len();
                    }
                }
            }
        }
    }

    /// Query for swarm inspect info.
    pub async fn inspect(&self) -> Output<SwarmInspect> {
        let resp = self
            .client
            .call_method(Method::NodeInfo.as_str(), Params::None)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let info: NodeInfo = serde_json::from_value(resp).map_err(|e| anyhow::anyhow!("{}", e))?;
        let display =
            serde_json::to_string_pretty(&info.swarm).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(display, info.swarm)
    }
}

impl<T> ClientOutput<T> {
    /// Put display ahead to avoid moved value error.
    pub fn ok(display: String, result: T) -> anyhow::Result<Self> {
        Ok(Self { result, display })
    }

    /// Prints the display value of this ClientOutput instance to the console.
    pub fn display(&self) {
        println!("{}", self.display);
    }
}
