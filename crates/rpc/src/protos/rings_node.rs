//! Request/response message types for the rings node RPC API.
//!
//! These were previously generated from `rings_node.proto` via prost, but the
//! wire format has always been JSON-RPC (never protobuf binary), so they are
//! now plain serde structs. Field names and types are kept identical to the
//! previous prost-generated output to preserve the on-the-wire JSON shape.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Summary of a peer connection known by the local node.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Decentralized identifier of the peer.
    pub did: String,
    /// Connection state reported by the swarm.
    pub state: String,
}

/// Request to connect to a peer through its HTTP RPC endpoint.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectPeerViaHttpRequest {
    /// HTTP endpoint URL exposed by the peer.
    pub url: String,
}

/// Response returned after connecting to an HTTP-reachable peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectPeerViaHttpResponse {
    /// Decentralized identifier resolved for the connected peer.
    pub did: String,
}

/// Request to connect to a peer by decentralized identifier.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithDidRequest {
    /// Decentralized identifier of the target peer.
    pub did: String,
}

/// Empty response returned after a DID-based connection request is accepted.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithDidResponse {}

/// Bootstrap peer descriptor used by seed connection requests.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SeedPeer {
    /// Decentralized identifier of the seed peer.
    pub did: String,
    /// HTTP endpoint URL for the seed peer.
    pub url: String,
}

/// Request to connect to one or more bootstrap peers.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithSeedRequest {
    /// Seed peers to connect through.
    pub peers: Vec<SeedPeer>,
}

/// Empty response returned after seed connection setup is accepted.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithSeedResponse {}

/// Request to list peers known by the local node.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeersRequest {}

/// Response containing peers known by the local node.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeersResponse {
    /// Known peer connection summaries.
    pub peers: Vec<PeerInfo>,
}

/// Request to create a WebRTC offer for a peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct CreateOfferRequest {
    /// Decentralized identifier of the peer receiving the offer.
    pub did: String,
}

/// Response containing a serialized WebRTC offer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct CreateOfferResponse {
    /// Serialized session description offer.
    pub offer: String,
}

/// Request to answer a serialized WebRTC offer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AnswerOfferRequest {
    /// Serialized session description offer.
    pub offer: String,
}

/// Response containing a serialized WebRTC answer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AnswerOfferResponse {
    /// Serialized session description answer.
    pub answer: String,
}

/// Request to accept a serialized WebRTC answer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AcceptAnswerRequest {
    /// Serialized session description answer.
    pub answer: String,
}

/// Empty response returned after an answer is accepted.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AcceptAnswerResponse {}

/// Request to disconnect from a peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct DisconnectRequest {
    /// Decentralized identifier of the peer to disconnect.
    pub did: String,
}

/// Empty response returned after a disconnect request is accepted.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct DisconnectResponse {}

/// Request to send a backend protocol message to another peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendBackendMessageRequest {
    /// Decentralized identifier of the destination peer.
    pub destination_did: String,
    /// Protocol namespace the payload is routed to (the extension `Envelope` namespace).
    pub namespace: String,
    /// Payload bytes, **base64-encoded** (standard alphabet). The `Envelope` payload is
    /// binary (`Bytes`), so the RPC boundary base64-encodes it to stay binary-safe over the
    /// JSON wire — do not pass raw UTF-8 here.
    pub data: String,
}

/// Empty response returned after a backend message is queued.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendBackendMessageResponse {}

/// Request to initiate an end-to-end encrypted handshake with a peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eHandshakeRequest {
    /// Decentralized identifier of the handshake target.
    pub destination_did: String,
}

/// Response returned after queuing an end-to-end handshake.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eHandshakeResponse {
    /// Transaction identifier assigned to the handshake message.
    pub tx_id: String,
}

/// Request to send an end-to-end encrypted message to a peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eMessageRequest {
    /// Decentralized identifier of the destination peer.
    pub destination_did: String,
    /// Recipient public key as a base58-check string. Hex is accepted by node implementations
    /// for development ergonomics.
    pub recipient_public_key: String,
    /// Plaintext bytes, base64-encoded for the JSON RPC boundary.
    pub data: String,
    /// Optional plaintext frame length. `0` means the core default.
    #[serde(default)]
    pub max_plaintext_frame_len: u32,
}

/// Response returned after queuing an end-to-end encrypted message.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eMessageResponse {
    /// Stream identifier used by the encrypted message transport.
    pub stream_id: String,
}

/// Request to publish a message to a DHT-backed topic.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PublishMessageToTopicRequest {
    /// Topic name receiving the message.
    pub topic: String,
    /// Message payload encoded for the JSON-RPC boundary.
    pub data: String,
}

/// Empty response returned after a topic message is queued.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PublishMessageToTopicResponse {}

/// Request to fetch messages from a DHT-backed topic.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct FetchTopicMessagesRequest {
    /// Topic name to read from.
    pub topic: String,
    /// Number of topic messages to skip from the start of the result set.
    pub skip: i64,
}

/// Response containing messages fetched from a topic.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct FetchTopicMessagesResponse {
    /// Topic message payloads encoded for the JSON-RPC boundary.
    pub data: Vec<String>,
}

/// Request to register the local DID as a provider for a named service.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct RegisterServiceRequest {
    /// Service name to register.
    pub name: String,
}

/// Empty response returned after service registration is accepted.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct RegisterServiceResponse {}

/// Request to resolve peers that provide a named service.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupServiceRequest {
    /// Service name to resolve.
    pub name: String,
}

/// Response containing DIDs that provide a named service.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupServiceResponse {
    /// Decentralized identifiers of service providers.
    pub dids: Vec<String>,
}

/// Request to list online node descriptors from the directory.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnlineNodesRequest {
    /// Whether expired descriptors should be included in the response.
    #[serde(default)]
    pub include_expired: bool,
}

/// Runtime class advertised by an online node descriptor.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum OnlineNodeTypeInfo {
    /// Native node runtime.
    #[default]
    Native,
    /// Browser node runtime.
    Browser,
    /// Foreign-function-interface hosted node runtime.
    Ffi,
}

/// Public descriptor for a node currently known by the online-node directory.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnlineNodeDescriptorInfo {
    /// Decentralized identifier of the advertised node.
    pub did: String,
    /// Verification public key encoded with the core serde shape.
    pub public_key: Value,
    /// Session encryption public key encoded with the core serde shape.
    pub session_public_key: Value,
    /// Runtime class of the advertised node.
    pub node_type: OnlineNodeTypeInfo,
    /// Overlay network identifier the descriptor belongs to.
    pub network_id: u32,
    /// Storage redundancy advertised by the node.
    pub storage_redundancy: u16,
    /// Number of virtual DHT nodes advertised by the node.
    pub dht_virtual_nodes: u16,
    /// Capability names advertised by the node.
    pub capabilities: Vec<String>,
    /// Optional endpoint hint clients may use for direct connection.
    pub endpoint_hint: Option<String>,
    /// Descriptor creation timestamp in Unix milliseconds.
    pub started_at_ms: u64,
    /// Last heartbeat timestamp in Unix milliseconds.
    pub heartbeat_at_ms: u64,
    /// Descriptor expiration timestamp in Unix milliseconds.
    pub expires_at_ms: u64,
    /// Rings node version that produced the descriptor.
    pub version: String,
    /// Descriptor signature encoded with the core serde shape.
    pub signature: Value,
}

/// Response containing online node descriptors.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnlineNodesResponse {
    /// Online node descriptors returned by the directory.
    pub nodes: Vec<OnlineNodeDescriptorInfo>,
}

/// Transport advertised by an onion exit service.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum OnionExitTransportInfo {
    /// TCP stream exit transport.
    #[default]
    Tcp,
    /// UDP datagram exit transport.
    Udp,
    /// WebTransport exit transport.
    WebTransport,
    /// Request-response protocol exit transport.
    RequestResponse,
    /// Legacy HTTPS exit marker retained for wire compatibility.
    Https,
}

/// Service name and transport pair advertised by an onion exit.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnionExitServiceInfo {
    /// Service name, such as `tcp` or `https`.
    pub name: String,
    /// Transport backing the service.
    pub transport: OnionExitTransportInfo,
}

/// Policy advertised by an onion exit descriptor.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnionExitPolicyInfo {
    /// Allowed target patterns for the exit.
    pub allowed_targets: Vec<String>,
    /// Denied target patterns for the exit.
    pub denied_targets: Vec<String>,
    /// Maximum concurrent circuits allowed by the exit.
    pub max_circuits: u32,
    /// Maximum concurrent streams allowed per circuit.
    pub max_streams_per_circuit: u32,
    /// Maximum bytes per minute allowed by the exit.
    pub max_bytes_per_minute: u64,
}

/// Public descriptor for a node that can serve as an onion exit.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnionExitDescriptorInfo {
    /// Decentralized identifier of the exit node.
    pub did: String,
    /// Verification public key encoded with the core serde shape.
    pub public_key: Value,
    /// Session encryption public key encoded with the core serde shape.
    pub session_public_key: Value,
    /// Runtime class of the exit node.
    pub node_type: OnlineNodeTypeInfo,
    /// Overlay network identifier the exit belongs to.
    pub network_id: u32,
    /// Services offered by the exit.
    pub services: Vec<OnionExitServiceInfo>,
    /// Target and resource policy enforced by the exit.
    pub policy: OnionExitPolicyInfo,
    /// Descriptor creation timestamp in Unix milliseconds.
    pub started_at_ms: u64,
    /// Last heartbeat timestamp in Unix milliseconds.
    pub heartbeat_at_ms: u64,
    /// Descriptor expiration timestamp in Unix milliseconds.
    pub expires_at_ms: u64,
    /// Rings node version that produced the descriptor.
    pub version: String,
    /// Descriptor signature encoded with the core serde shape.
    pub signature: Value,
}

/// Request to lookup live or stored onion exit descriptors.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnionExitsRequest {
    /// Service name filter. Empty means all services.
    #[serde(default)]
    pub service: String,
    /// Whether expired descriptors should be included in the response.
    #[serde(default)]
    pub include_expired: bool,
}

/// Response containing onion exit descriptors.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnionExitsResponse {
    /// Onion exit descriptors returned by the directory.
    pub exits: Vec<OnionExitDescriptorInfo>,
}

/// Request to build an onion route for a service.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct BuildOnionRouteRequest {
    /// Service name requested by the route.
    pub service: String,
    /// Desired hop count including the exit. `0` means node default.
    #[serde(default)]
    pub hop_count: u32,
    /// Allow route selection to return fewer hops when too few relays are live.
    #[serde(default)]
    pub allow_short_paths: bool,
}

/// Response containing a selected onion route and exit.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct BuildOnionRouteResponse {
    /// Ordered DID hops, ending with the selected exit.
    pub hops: Vec<String>,
    /// Service name satisfied by the selected route.
    pub service: String,
    /// Onion exit selected for the route.
    pub exit: OnionExitDescriptorInfo,
}

/// Request to inspect the local node.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeInfoRequest {}

/// Inclusive key range covered by a DHT finger table entry.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct FingerTableRange {
    /// DID owning the range, if a peer is known.
    pub did: Option<String>,
    /// Start of the key range.
    pub start: u64,
    /// End of the key range.
    pub end: u64,
}

/// DHT inspection data for the local node.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct DhtInfo {
    /// Decentralized identifier of the local node.
    pub did: String,
    /// Successor DIDs known by the local node.
    pub successors: Vec<String>,
    /// Predecessor DID known by the local node.
    pub predecessor: Option<String>,
    /// Finger table ranges observed by the local node.
    pub finger_table_ranges: Vec<FingerTableRange>,
}

/// Stored value returned by node inspection.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct StorageValue {
    /// DID associated with the stored value.
    pub did: String,
    /// Storage record kind.
    pub kind: String,
    /// Stored payload values encoded for the JSON-RPC boundary.
    pub data: Vec<String>,
}

/// Storage item returned by node inspection.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct StorageItem {
    /// Storage key.
    pub key: String,
    /// Stored value when the key is present.
    pub value: Option<StorageValue>,
}

/// Storage inspection data for a node storage backend.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct StorageInfo {
    /// Storage items observed in the backend.
    pub items: Vec<StorageItem>,
}

/// Combined swarm, DHT, and storage inspection data.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SwarmInfo {
    /// Peer connection summaries.
    pub peers: Vec<PeerInfo>,
    /// DHT inspection data when the DHT is available.
    pub dht: Option<DhtInfo>,
    /// Persistent storage inspection data when available.
    pub persistence_storage: Option<StorageInfo>,
    /// Cache storage inspection data when available.
    pub cache_storage: Option<StorageInfo>,
}

/// Response returned by node inspection.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeInfoResponse {
    /// Rings node version.
    pub version: String,
    /// Swarm inspection data when available.
    pub swarm: Option<SwarmInfo>,
}

/// Request to fetch measurements for a single peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementRequest {
    /// Decentralized identifier of the peer being measured.
    pub did: String,
}

/// Request to list measurements for all peers.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeerMeasurementsRequest {}

/// Counter set for peer transport measurements.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementCountersInfo {
    /// Number of successful connection events.
    pub connected: u64,
    /// Number of disconnection events.
    pub disconnected: u64,
    /// Number of successful send events.
    pub sent: u64,
    /// Number of failed send events.
    pub failed_to_send: u64,
    /// Number of successful receive events.
    pub received: u64,
    /// Number of failed receive events.
    pub failed_to_receive: u64,
}

/// Measurements collected for a single peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementInfo {
    /// Decentralized identifier of the measured peer.
    pub did: String,
    /// Transport measurement counters for the peer.
    pub counters: PeerMeasurementCountersInfo,
}

/// Response containing measurements for all measured peers.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeerMeasurementsResponse {
    /// Per-peer measurement entries.
    pub measurements: Vec<PeerMeasurementInfo>,
}

/// Response containing measurements for one peer.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementResponse {
    /// Measurement entry for the requested peer, if present.
    pub measurement: Option<PeerMeasurementInfo>,
}

/// Request to read the local node DID.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeDidRequest {}

/// Response containing the local node DID.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeDidResponse {
    /// Decentralized identifier of the local node.
    pub did: String,
}
