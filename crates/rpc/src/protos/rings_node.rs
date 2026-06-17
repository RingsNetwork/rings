//! Request/response message types for the rings node RPC API.
//!
//! These were previously generated from `rings_node.proto` via prost, but the
//! wire format has always been JSON-RPC (never protobuf binary), so they are
//! now plain serde structs. Field names and types are kept identical to the
//! previous prost-generated output to preserve the on-the-wire JSON shape.

use serde::Deserialize;
use serde::Serialize;

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
pub struct NodeDidRequest {}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct NodeDidResponse {
    pub did: String,
}
