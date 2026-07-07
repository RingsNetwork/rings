//! Request/response message types for the rings node RPC API.
//!
//! These were previously generated from `rings_node.proto` via prost, but the
//! wire format has always been JSON-RPC (never protobuf binary), so they are
//! now plain serde structs. Field names and types are kept identical to the
//! previous prost-generated output to preserve the on-the-wire JSON shape.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerInfo {
    pub did: String,
    pub state: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectPeerViaHttpRequest {
    pub url: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectPeerViaHttpResponse {
    pub did: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithDidRequest {
    pub did: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithDidResponse {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SeedPeer {
    pub did: String,
    pub url: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithSeedRequest {
    pub peers: Vec<SeedPeer>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ConnectWithSeedResponse {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeersRequest {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeersResponse {
    pub peers: Vec<PeerInfo>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct CreateOfferRequest {
    pub did: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct CreateOfferResponse {
    pub offer: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AnswerOfferRequest {
    pub offer: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AnswerOfferResponse {
    pub answer: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AcceptAnswerRequest {
    pub answer: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct AcceptAnswerResponse {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct DisconnectRequest {
    pub did: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct DisconnectResponse {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendBackendMessageRequest {
    pub destination_did: String,
    /// Protocol namespace the payload is routed to (the extension `Envelope` namespace).
    pub namespace: String,
    /// Payload bytes, **base64-encoded** (standard alphabet). The `Envelope` payload is
    /// binary (`Bytes`), so the RPC boundary base64-encodes it to stay binary-safe over the
    /// JSON wire — do not pass raw UTF-8 here.
    pub data: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendBackendMessageResponse {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eHandshakeRequest {
    pub destination_did: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eHandshakeResponse {
    pub tx_id: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eMessageRequest {
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

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SendE2eMessageResponse {
    pub stream_id: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PublishMessageToTopicRequest {
    pub topic: String,
    pub data: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PublishMessageToTopicResponse {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct FetchTopicMessagesRequest {
    pub topic: String,
    pub skip: i64,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct FetchTopicMessagesResponse {
    pub data: Vec<String>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct RegisterServiceRequest {
    pub name: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct RegisterServiceResponse {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupServiceRequest {
    pub name: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupServiceResponse {
    pub dids: Vec<String>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnlineNodesRequest {
    #[serde(default)]
    pub include_expired: bool,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum OnlineNodeTypeInfo {
    #[default]
    Native,
    Browser,
    Ffi,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnlineNodeDescriptorInfo {
    pub did: String,
    /// Verification public key encoded with the core serde shape.
    pub public_key: Value,
    /// Session encryption public key encoded with the core serde shape.
    pub session_public_key: Value,
    pub node_type: OnlineNodeTypeInfo,
    pub network_id: u32,
    pub storage_redundancy: u16,
    pub dht_virtual_nodes: u16,
    pub capabilities: Vec<String>,
    pub endpoint_hint: Option<String>,
    pub started_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub expires_at_ms: u64,
    pub version: String,
    /// Descriptor signature encoded with the core serde shape.
    pub signature: Value,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnlineNodesResponse {
    pub nodes: Vec<OnlineNodeDescriptorInfo>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum OnionExitTransportInfo {
    #[default]
    Tcp,
    Udp,
    WebTransport,
    RequestResponse,
    Https,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnionExitServiceInfo {
    pub name: String,
    pub transport: OnionExitTransportInfo,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnionExitPolicyInfo {
    pub allowed_targets: Vec<String>,
    pub denied_targets: Vec<String>,
    pub max_circuits: u32,
    pub max_streams_per_circuit: u32,
    pub max_bytes_per_minute: u64,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct OnionExitDescriptorInfo {
    pub did: String,
    /// Verification public key encoded with the core serde shape.
    pub public_key: Value,
    /// Session encryption public key encoded with the core serde shape.
    pub session_public_key: Value,
    pub node_type: OnlineNodeTypeInfo,
    pub network_id: u32,
    pub services: Vec<OnionExitServiceInfo>,
    pub policy: OnionExitPolicyInfo,
    pub started_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub expires_at_ms: u64,
    pub version: String,
    /// Descriptor signature encoded with the core serde shape.
    pub signature: Value,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnionExitsRequest {
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub include_expired: bool,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct LookupOnionExitsResponse {
    pub exits: Vec<OnionExitDescriptorInfo>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct BuildOnionRouteRequest {
    pub service: String,
    /// Desired hop count including the exit. `0` means node default.
    #[serde(default)]
    pub hop_count: u32,
    /// Allow route selection to return fewer hops when too few relays are live.
    #[serde(default)]
    pub allow_short_paths: bool,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct BuildOnionRouteResponse {
    /// Ordered DID hops, ending with the selected exit.
    pub hops: Vec<String>,
    pub service: String,
    pub exit: OnionExitDescriptorInfo,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeInfoRequest {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct FingerTableRange {
    pub did: Option<String>,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct DhtInfo {
    pub did: String,
    pub successors: Vec<String>,
    pub predecessor: Option<String>,
    pub finger_table_ranges: Vec<FingerTableRange>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct StorageValue {
    pub did: String,
    pub kind: String,
    pub data: Vec<String>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct StorageItem {
    pub key: String,
    pub value: Option<StorageValue>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct StorageInfo {
    pub items: Vec<StorageItem>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct SwarmInfo {
    pub peers: Vec<PeerInfo>,
    pub dht: Option<DhtInfo>,
    pub persistence_storage: Option<StorageInfo>,
    pub cache_storage: Option<StorageInfo>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeInfoResponse {
    pub version: String,
    pub swarm: Option<SwarmInfo>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementRequest {
    pub did: String,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeerMeasurementsRequest {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementCountersInfo {
    pub connected: u64,
    pub disconnected: u64,
    pub sent: u64,
    pub failed_to_send: u64,
    pub received: u64,
    pub failed_to_receive: u64,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementInfo {
    pub did: String,
    pub counters: PeerMeasurementCountersInfo,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ListPeerMeasurementsResponse {
    pub measurements: Vec<PeerMeasurementInfo>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct PeerMeasurementResponse {
    pub measurement: Option<PeerMeasurementInfo>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeDidRequest {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeDidResponse {
    pub did: String,
}
