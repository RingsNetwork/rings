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
use rings_core::message::Encoded;
use rings_core::message::Message;
use rings_rpc::jsonrpc::Client as RpcClient;
use rings_rpc::protos::rings_node::AcceptAnswerRequest;
use rings_rpc::protos::rings_node::AnswerOfferRequest;
use rings_rpc::protos::rings_node::ConnectPeerViaHttpRequest;
use rings_rpc::protos::rings_node::ConnectWithDidRequest;
use rings_rpc::protos::rings_node::CreateOfferRequest;
use rings_rpc::protos::rings_node::DisconnectRequest;
use rings_rpc::protos::rings_node::FetchTopicMessagesRequest;
use rings_rpc::protos::rings_node::ListPeersRequest;
use rings_rpc::protos::rings_node::LookupServiceRequest;
use rings_rpc::protos::rings_node::NodeInfoRequest;
use rings_rpc::protos::rings_node::PublishMessageToTopicRequest;
use rings_rpc::protos::rings_node::RegisterServiceRequest;
use rings_rpc::protos::rings_node::SendBackendMessageRequest;
use rings_rpc::protos::rings_node::SwarmInfo;

use crate::processor::handshake::decode_handshake_payload;
use crate::seed::Seed;
use crate::util::loader::ResourceLoader;

/// Alias about `Result<ClientOutput<T>, E>`.
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
    stderr_notice: Option<String>,
}

/// Whether the encoded payload returned by `answerOffer` is an answer (`ConnectNodeReport`)
/// or a counter-offer (`ConnectNodeSend`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfferReplyKind {
    /// A `ConnectNodeReport` answer for the offerer's `connect accept`.
    Answer,
    /// A `ConnectNodeSend` counter-offer: the offerer answers it with `connect answer` and
    /// returns that answer to this node's `connect accept`.
    CounterOffer,
}

impl OfferReplyKind {
    /// Classify an encoded `answerOffer` reply payload.
    pub fn from_encoded_reply(reply: &str) -> anyhow::Result<Self> {
        let payload = decode_handshake_payload(&Encoded::from(reply.to_owned()))?;
        match payload.transaction.data::<Message>()? {
            Message::ConnectNodeReport(_) => Ok(Self::Answer),
            Message::ConnectNodeSend(_) => Ok(Self::CounterOffer),
            other => anyhow::bail!("unexpected answerOffer reply message: {other}"),
        }
    }

    /// Operator notice printed to stderr so stdout stays the raw base58 payload for piping.
    pub const fn stderr_notice(self) -> &'static str {
        match self {
            Self::Answer => {
                "Answered peer's offer (answer); return this payload to the offerer for `rings connect accept`."
            }
            Self::CounterOffer => {
                "Counter-offer (offer, answered: false): this node's pending offer prevailed on the DID tie-break. \
                 Pass this counter-offer to the peer for `rings connect answer`, then complete on this node with `rings connect accept`."
            }
        }
    }
}

/// Build the `ClientOutput` for an encoded `answerOffer` reply.
pub fn format_answer_offer_output(reply: String) -> Output<String> {
    let notice = match OfferReplyKind::from_encoded_reply(reply.as_str()) {
        Ok(kind) => kind.stderr_notice().to_owned(),
        Err(_) => OfferReplyKind::Answer.stderr_notice().to_owned(),
    };
    ClientOutput::ok_with_stderr(reply.clone(), reply, notice)
}

/// Reject an encoded `ConnectNodeSend` counter-offer passed to `connect accept` with a clear
/// operator message pointing to `rings connect answer`.
pub fn ensure_accept_answer_is_not_counter_offer(answer: &str) -> anyhow::Result<()> {
    if matches!(
        OfferReplyKind::from_encoded_reply(answer),
        Ok(OfferReplyKind::CounterOffer)
    ) {
        anyhow::bail!(
            "payload is a counter-offer (`ConnectNodeSend`), not an answer (`ConnectNodeReport`); \
             answer it with `rings connect answer <counter_offer>` and pass the resulting answer \
             to the peer's `rings connect accept`"
        );
    }
    Ok(())
}

impl Client {
    /// Creates a new Client instance with the specified endpoint URL and signature.
    pub fn new(endpoint_url: &str) -> anyhow::Result<Self> {
        let rpc_client =
            RpcClient::new(endpoint_url).map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(Self { client: rpc_client })
    }

    /// Creates a client for an endpoint protected by the supplied API Bearer token.
    pub fn with_api_token(endpoint_url: &str, api_token: String) -> anyhow::Result<Self> {
        let rpc_client = RpcClient::new(endpoint_url)
            .and_then(|client| client.with_bearer_token(api_token))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(Self { client: rpc_client })
    }

    /// Establishes a WebRTC connection with a remote peer using HTTP as the signaling channel.
    ///
    /// This function allows two peers to establish a WebRTC connection using HTTP,
    /// which can be useful in scenarios where a direct peer-to-peer connection is not possible due to firewall restrictions or other network issues.
    /// The function sends ICE candidates and Delegation Description Protocol (SDP) messages over HTTP as a form of signaling to establish the connection.
    ///
    /// Takes a URL for an HTTP server that will be used as the signaling channel to exchange ICE candidates and SDP with the remote peer.
    /// Returns a Did that can be used to refer to this connection in subsequent WebRTC operations.
    pub async fn connect_peer_via_http(&mut self, url: &str) -> Output<String> {
        self.connect_peer_via_http_with_token(url, None).await
    }

    /// Establishes a WebRTC connection through a remote API, presenting a Bearer token only
    /// when the remote gates its otherwise public handshake.
    pub async fn connect_peer_via_http_with_token(
        &mut self,
        url: &str,
        remote_api_token: Option<String>,
    ) -> Output<String> {
        let peer_did = self
            .client
            .connect_peer_via_http(&ConnectPeerViaHttpRequest {
                url: url.to_string(),
                api_token: remote_api_token,
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .did;

        ClientOutput::ok(format!("Remote did: {peer_did}"), peer_did)
    }

    /// Attempts to connect to a peer using a seed file located at the specified source path.
    pub async fn connect_with_seed(&mut self, source: &str) -> Output<()> {
        let seed = Seed::load(source).await?;
        let req = seed.into_connect_with_seed_request();

        self.client
            .connect_with_seed(&req)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Successful!".to_string(), ())
    }

    /// Attempts to connect to a peer using a DID stored in a Distributed Hash Table (DHT).
    pub async fn connect_with_did(&mut self, did: &str) -> Output<()> {
        self.client
            .connect_with_did(&ConnectWithDidRequest {
                did: did.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Successful!".to_owned(), ())
    }

    /// Creates a local WebRTC offer targeting `did` for a manual, out-of-band handshake.
    ///
    /// Returns the base58-check encoded offer payload, which the remote peer feeds to
    /// [`answer_offer`](Self::answer_offer). Use this when the peers cannot signal over
    /// HTTP and the offer/answer strings are exchanged by other means (copy/paste, QR, chat).
    pub async fn create_offer(&mut self, did: &str) -> Output<String> {
        let offer = self
            .client
            .create_offer(&CreateOfferRequest {
                did: did.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .offer;

        ClientOutput::ok(offer.clone(), offer)
    }

    /// Answers a peer's offer, producing the encoded answer (or counter-offer) to return to the
    /// offerer.
    ///
    /// Consumes the encoded offer emitted by [`create_offer`](Self::create_offer) and returns the
    /// base58-check encoded reply payload. When the reply is an answer (`ConnectNodeReport`), the
    /// offerer feeds it to [`accept_answer`](Self::accept_answer). When the reply is a
    /// counter-offer (`ConnectNodeSend`), the peer answers it with `answer_offer` and returns
    /// that answer to this node's [`accept_answer`](Self::accept_answer).
    pub async fn answer_offer(&mut self, offer: &str) -> Output<String> {
        let answer = self
            .client
            .answer_offer(&AnswerOfferRequest {
                offer: offer.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .answer;

        format_answer_offer_output(answer)
    }

    /// Accepts a peer's answer, completing the manual handshake initiated by
    /// [`create_offer`](Self::create_offer).
    ///
    /// Refuses a counter-offer (`ConnectNodeSend`) before sending the RPC request, pointing the
    /// operator to `rings connect answer` instead.
    pub async fn accept_answer(&mut self, answer: &str) -> Output<()> {
        ensure_accept_answer_is_not_counter_offer(answer)?;
        self.client
            .accept_answer(&AcceptAnswerRequest {
                answer: answer.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Done.".into(), ())
    }

    /// Lists all connected peers and their status.
    ///
    /// Returns an Output containing a formatted string representation of the list of peers if successful, or an anyhow::Error if an error occurred.
    pub async fn list_peers(&mut self) -> Output<()> {
        let peers = self
            .client
            .list_peers(&ListPeersRequest {})
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .peers;

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
            .disconnect(&DisconnectRequest {
                did: did.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok("Done.".into(), ())
    }

    /// Sends a namespaced message to the specified peer, routed to the peer's protocol
    /// registered under `namespace` (the extension `Envelope` model).
    pub async fn send_message(&self, did: &str, namespace: &str, data: &str) -> Output<()> {
        self.client
            .send_backend_message(&SendBackendMessageRequest {
                destination_did: did.to_string(),
                namespace: namespace.to_string(),
                // The wire field is base64 (binary-safe); encode the raw input bytes.
                data: base64::encode(data.as_bytes()),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Registers a new service with the given name.
    pub async fn register_service(&self, name: &str) -> Output<()> {
        self.client
            .register_service(&RegisterServiceRequest {
                name: name.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        ClientOutput::ok("Done.".into(), ())
    }

    /// Looks up the DIDs of services registered with the given name.
    pub async fn lookup_service(&self, name: &str) -> Output<()> {
        let dids = self
            .client
            .lookup_service(&LookupServiceRequest {
                name: name.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .dids;

        ClientOutput::ok(dids.join("\n"), ())
    }

    /// Publishes a message to the specified topic.
    pub async fn publish_message_to_topic(&self, topic: &str, data: &str) -> Output<()> {
        self.client
            .publish_message_to_topic(&PublishMessageToTopicRequest {
                topic: topic.to_string(),
                data: data.to_string(),
            })
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
        let mut skip = 0usize;

        stream! {
            loop {
                let timeout = rings_runtime::sleep(Duration::from_secs(5)).fuse();
                pin_mut!(timeout);

                select! {
                    _ = timeout => {
                        let result = self
                            .client
                            .fetch_topic_messages(&FetchTopicMessagesRequest {
                                topic: topic.clone(),
                                skip: skip as i64,
                            })
                            .await;

                        let messages = match result {
                            Ok(result) => result.data,
                            Err(e) => {
                                tracing::error!("Failed to fetch messages of topic: {}, {}", topic, e);
                                continue;
                            }
                        };
                        for msg in messages.iter().cloned() {
                            yield msg
                        }
                        skip += messages.len();
                    }
                }
            }
        }
    }

    /// Query for swarm inspect info.
    pub async fn inspect(&self) -> Output<SwarmInfo> {
        let swarm_info = self
            .client
            .node_info(&NodeInfoRequest {})
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .swarm
            .ok_or_else(|| anyhow::anyhow!("node_info response did not include swarm info"))?;

        let display =
            serde_json::to_string_pretty(&swarm_info).map_err(|e| anyhow::anyhow!("{}", e))?;

        ClientOutput::ok(display, swarm_info)
    }
}

impl<T> ClientOutput<T> {
    /// Put display ahead to avoid moved value error.
    pub fn ok(display: String, result: T) -> anyhow::Result<Self> {
        Ok(Self {
            result,
            display,
            stderr_notice: None,
        })
    }

    /// Construct a `ClientOutput` with an additional operator notice printed to stderr.
    pub fn ok_with_stderr(
        display: String,
        result: T,
        stderr_notice: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            result,
            display,
            stderr_notice: Some(stderr_notice),
        })
    }

    /// Human-readable stdout string.
    pub fn display_text(&self) -> &str {
        self.display.as_str()
    }

    /// Optional operator notice printed to stderr.
    pub fn stderr_notice(&self) -> Option<&str> {
        self.stderr_notice.as_deref()
    }

    /// Prints the display value of this ClientOutput instance to the console, and any operator
    /// notice to stderr.
    pub fn display(&self) {
        if let Some(notice) = &self.stderr_notice {
            eprintln!("{notice}");
        }
        println!("{}", self.display);
    }
}

/// CLI constructors must preserve the shared RPC credential transport boundary.
#[cfg(test)]
mod credential_transport_tests {
    use rings_core::delegation::DelegateeKey;
    use rings_core::dht::delivery::NextHop;
    use rings_core::dht::Did;
    use rings_core::dht::RelayCustody;
    use rings_core::ecc::SecretKey;
    use rings_core::message::ConnectNodeReport;
    use rings_core::message::ConnectNodeSend;
    use rings_core::message::DhtProtocolMode;
    use rings_core::message::Encoder;
    use rings_core::message::Message;
    use rings_core::message::MessagePayload;
    use rings_core::message::MessageSigner;

    use super::ensure_accept_answer_is_not_counter_offer;
    use super::format_answer_offer_output;
    use super::Client;
    use super::OfferReplyKind;

    fn encoded_handshake_payload(message: Message) -> String {
        let key = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey(&key).expect("test delegatee key");
        let signer = MessageSigner::new(&delegatee_key, 1);
        let destination: Did = SecretKey::random().address().into();
        let payload = MessagePayload::new_send_with_sequence(
            message,
            signer,
            NextHop::toward(destination),
            destination,
            0,
            None,
        )
        .expect("signed handshake payload");
        payload.encode().expect("encoded payload").to_string()
    }

    fn encoded_answer_fixture() -> String {
        encoded_handshake_payload(Message::ConnectNodeReport(ConnectNodeReport {
            sdp: "{\"type\":\"answer\"}".to_owned(),
            dht_protocol_mode: DhtProtocolMode::new(1, 1, 1),
            relay_custody: RelayCustody::Holds,
        }))
    }

    fn encoded_counter_offer_fixture() -> String {
        encoded_handshake_payload(Message::ConnectNodeSend(ConnectNodeSend {
            sdp: "{\"type\":\"offer\"}".to_owned(),
            dht_protocol_mode: DhtProtocolMode::new(1, 1, 1),
            relay_custody: RelayCustody::Holds,
        }))
    }

    /// An endpoint override cannot carry the API credential over remote cleartext HTTP.
    #[test]
    fn authenticated_cli_rejects_remote_http() {
        for endpoint in [
            "http://example.com",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
        ] {
            assert!(Client::with_api_token(endpoint, "fixture-token".to_owned()).is_err());
        }
    }

    /// Explicit loopback IP endpoints and HTTPS remain available through the CLI wrapper.
    #[test]
    fn authenticated_cli_accepts_https_and_literal_loopback() {
        for endpoint in [
            "https://example.com",
            "http://127.0.0.1:50000",
            "http://[::1]:50000",
        ] {
            assert!(Client::with_api_token(endpoint, "fixture-token".to_owned()).is_ok());
        }
    }

    /// `answer_offer` output for an answer (`ConnectNodeReport`, `answered: true`) keeps the
    /// raw payload on stdout and labels the reply as an answer for `connect accept` on stderr.
    #[test]
    fn answer_offer_formats_answer_reply() {
        let answer = encoded_answer_fixture();
        assert_eq!(
            OfferReplyKind::from_encoded_reply(answer.as_str()).expect("classify answer"),
            OfferReplyKind::Answer
        );

        let output = format_answer_offer_output(answer.clone()).expect("format answer output");
        assert_eq!(output.result, answer);
        assert_eq!(output.display_text(), answer);
        let notice = output.stderr_notice().expect("stderr notice for answer");
        assert!(notice.contains("answer"), "unexpected notice: {notice}");
        assert!(
            notice.contains("rings connect accept"),
            "unexpected notice: {notice}"
        );
    }

    /// `answer_offer` output for a counter-offer (`ConnectNodeSend`, `answered: false`) keeps
    /// the raw payload on stdout and explains on stderr that the peer must run `connect answer`
    /// and return that answer to this node's `connect accept`.
    #[test]
    fn answer_offer_formats_counter_offer_reply() {
        let counter_offer = encoded_counter_offer_fixture();
        assert_eq!(
            OfferReplyKind::from_encoded_reply(counter_offer.as_str())
                .expect("classify counter-offer"),
            OfferReplyKind::CounterOffer
        );

        let output =
            format_answer_offer_output(counter_offer.clone()).expect("format counter-offer output");
        assert_eq!(output.result, counter_offer);
        assert_eq!(output.display_text(), counter_offer);
        let notice = output
            .stderr_notice()
            .expect("stderr notice for counter-offer");
        assert!(
            notice.contains("Counter-offer"),
            "unexpected notice: {notice}"
        );
        assert!(
            notice.contains("answered: false"),
            "unexpected notice: {notice}"
        );
        assert!(
            notice.contains("rings connect answer"),
            "unexpected notice: {notice}"
        );
        assert!(
            notice.contains("rings connect accept"),
            "unexpected notice: {notice}"
        );
    }

    /// `connect accept` refuses a counter-offer (`ConnectNodeSend`) with an explanation pointing
    /// to `rings connect answer`, while allowing an answer (`ConnectNodeReport`).
    #[test]
    fn accept_answer_refuses_counter_offer_with_clear_instruction() {
        let answer = encoded_answer_fixture();
        assert!(ensure_accept_answer_is_not_counter_offer(answer.as_str()).is_ok());

        let counter_offer = encoded_counter_offer_fixture();
        let err = ensure_accept_answer_is_not_counter_offer(counter_offer.as_str())
            .expect_err("counter-offer must be refused by connect accept");
        let msg = err.to_string();
        assert!(msg.contains("counter-offer"), "unexpected error: {msg}");
        assert!(
            msg.contains("rings connect answer"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("rings connect accept"),
            "unexpected error: {msg}"
        );
    }
}
