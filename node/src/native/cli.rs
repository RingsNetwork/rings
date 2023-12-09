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

use std::time::Duration;

use async_stream::stream;
use futures::pin_mut;
use futures::select;
use futures::FutureExt;
use futures::Stream;
use futures_timer::Delay;
use serde_json::json;

use crate::backend::types::BackendMessage;
use crate::backend::types::HttpRequest;
use crate::backend::types::ServiceMessage;
use crate::prelude::rings_core::inspect::SwarmInspect;
use crate::prelude::rings_core::session::SessionSk;
use crate::prelude::rings_rpc::client::Client as RpcClient;
use crate::seed::Seed;
use crate::util::loader::ResourceLoader;

/// Alias about Result<ClientOutput<T>, E>.
type Output<T> = anyhow::Result<ClientOutput<T>>;

/// Wrap json_client send request between nodes or browsers.
pub struct Client {
    client: RpcClient,
}

/// Wrap client output contain raw result and humanreadable display.
pub struct ClientOutput<T> {
    /// Output data.
    pub result: T,
    display: String,
}

impl Client {
    /// Creates a new Client instance with the specified endpoint URL and signature.
    pub fn new(endpoint_url: &str, session_sk: SessionSk) -> anyhow::Result<Self> {
        let rpc_client = RpcClient::new(endpoint_url, Some(session_sk));
        Ok(Self { client: rpc_client })
    }

    /// Establishes a WebRTC connection with a remote peer using HTTP as the signaling channel.
    ///
    /// This function allows two peers to establish a WebRTC connection using HTTP,
    /// which can be useful in scenarios where a direct peer-to-peer connection is not possible due to firewall restrictions or other network issues.
    /// The function sends ICE candidates and Session Description Protocol (SDP) messages over HTTP as a form of signaling to establish the connection.
    ///
    /// Takes a URL for an HTTP server that will be used as the signaling channel to exchange ICE candidates and SDP with the remote peer.
    /// Returns a Did that can be used to refer to this connection in subsequent WebRTC operations.
    pub async fn connect_peer_via_http(&mut self, http_url: &str) -> Output<String> {
        let peer_did = self
            .client
            .connect_peer_via_http(http_url)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(format!("Remote did: {}", peer_did), peer_did)
    }

    /// Attempts to connect to a peer using a seed file located at the specified source path.
    pub async fn connect_with_seed(&mut self, source: &str) -> Output<()> {
        let seed = Seed::load(source).await?;
        let seed_v = serde_json::to_value(seed).map_err(|_| anyhow::anyhow!("serialize failed"))?;

        self.client
            .connect_with_seed(&[seed_v])
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Successful!".to_string(), ())
    }

    /// Attempts to connect to a peer using a DID stored in a Distributed Hash Table (DHT).
    pub async fn connect_with_did(&mut self, did: &str) -> Output<()> {
        self.client
            .connect_with_did(did)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Successful!".to_owned(), ())
    }

    /// Lists all connected peers and their status.
    ///
    /// Returns an Output containing a formatted string representation of the list of peers if successful, or an anyhow::Error if an error occurred.
    pub async fn list_peers(&mut self) -> Output<()> {
        let peers = self
            .client
            .list_peers()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let mut display = String::new();
        display.push_str("Did, TransportId, Status\n");
        display.push_str(
            peers
                .iter()
                .map(|peer| format!("{}, {}, {}", peer.did, peer.did, peer.state))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        );

        ClientOutput::ok(display, ())
    }

    /// Disconnects from the peer with the specified DID.
    pub async fn disconnect(&mut self, did: &str) -> Output<()> {
        self.client
            .disconnect(did)
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
            .send_message(did, text)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends a custom message to the specified peer.
    pub async fn send_custom_message(&self, did: &str, data: &str) -> Output<()> {
        self.client
            .send_custom_message(did, data)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends an HTTP request message to the specified peer.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_http_request_message(
        &self,
        did: &str,
        service: &str,
        method: http::Method,
        path: &str,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
        rid: Option<String>,
    ) -> Output<()> {
        let req = HttpRequest {
            service: service.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            headers,
            body,
            rid,
        };

        let msg = BackendMessage::ServiceMessage(ServiceMessage::HttpRequest(req));

        let data = bincode::serialize(&msg).map_err(|e| {
            anyhow::anyhow!("Failed to serialize HttpRequest message to binary format: {e}",)
        })?;
        let data_b64 = base64::encode(&data);

        self.client
            .send_custom_message(did, &data_b64)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends a plain text message to the specified peer.
    pub async fn send_plain_text_message(&self, did: &str, text: &str) -> Output<()> {
        let msg = BackendMessage::PlainText(text.to_string());

        let data = bincode::serialize(&msg).map_err(|e| {
            anyhow::anyhow!("Failed to serialize PlainText message to binary format: {e}",)
        })?;
        let data_b64 = base64::encode(&data);

        self.client
            .send_custom_message(did, &data_b64)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Done.".into(), ())
    }

    /// Registers a new service with the given name.
    pub async fn register_service(&self, name: &str) -> Output<()> {
        self.client
            .register_service(name)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Looks up the DIDs of services registered with the given name.
    pub async fn lookup_service(&self, name: &str) -> Output<()> {
        let dids = self
            .client
            .lookup_service(name)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(dids.join("\n"), ())
    }

    /// Publishes a message to the specified topic.
    pub async fn publish_message_to_topic(&self, topic: &str, data: &str) -> Output<()> {
        self.client
            .publish_message_to_topic(topic, data)
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
                        .fetch_topic_messages(topic.as_str(), index)
                        .await;

                    if let Err(e) = result {
                        tracing::error!("Failed to fetch messages of topic: {}, {}", topic, e);
                        continue;
                    }
                    let messages = result.unwrap();
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
        let info = self
            .client
            .inspect()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

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
