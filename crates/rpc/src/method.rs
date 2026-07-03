//! Rpc methods.
#![warn(missing_docs)]

use super::error::Error;
use super::error::Result;

/// supported methods.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Method {
    /// Connect peer with remote jsonrpc server url
    ConnectPeerViaHttp,
    /// Connect peer with remote peer's did
    ConnectWithDid,
    /// Connect peers from a seed file
    ConnectWithSeed,
    /// List all connected peers
    ListPeers,
    /// Create offer for manually handshake
    CreateOffer,
    /// Answer offer for manually handshake
    AnswerOffer,
    /// Accept Answer for manually handshake
    AcceptAnswer,
    /// Disconnect a peer
    Disconnect,
    /// SendBackendMessage
    SendBackendMessage,
    /// Send an E2E public-key handshake request
    SendE2eHandshake,
    /// Send an encrypted E2E message stream
    SendE2eMessage,
    /// Append data to topic
    PublishMessageToTopic,
    /// Fetch data of topic
    FetchTopicMessages,
    /// Register service
    RegisterService,
    /// Lookup service
    LookupService,
    /// Lookup online-node registry descriptors
    LookupOnlineNodes,
    /// Retrieve Node info
    NodeInfo,
    /// Retrieve local measurement counters for a peer
    PeerMeasurement,
    /// Retrieve local measurement counters for connected peers
    ListPeerMeasurements,
    /// Retrieve Node DID
    NodeDid,
}

impl Method {
    /// Return method's name as `&str`
    pub fn as_str(&self) -> &str {
        match self {
            Method::ConnectPeerViaHttp => "connectPeerViaHttp",
            Method::ConnectWithDid => "connectWithDid",
            Method::ConnectWithSeed => "connectWithSeed",
            Method::ListPeers => "listPeers",
            Method::CreateOffer => "createOffer",
            Method::AnswerOffer => "answerOffer",
            Method::Disconnect => "disconnect",
            Method::AcceptAnswer => "acceptAnswer",
            Method::SendBackendMessage => "sendBackendMessage",
            Method::SendE2eHandshake => "sendE2eHandshake",
            Method::SendE2eMessage => "sendE2eMessage",
            Method::PublishMessageToTopic => "publishMessageToTopic",
            Method::FetchTopicMessages => "fetchTopicMessages",
            Method::RegisterService => "registerService",
            Method::LookupService => "lookupService",
            Method::LookupOnlineNodes => "lookupOnlineNodes",
            Method::NodeInfo => "nodeInfo",
            Method::PeerMeasurement => "peerMeasurement",
            Method::ListPeerMeasurements => "listPeerMeasurements",
            Method::NodeDid => "nodeDid",
        }
    }
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for Method {
    fn to_string(&self) -> String {
        self.as_str().to_owned()
    }
}

impl TryFrom<&str> for Method {
    type Error = crate::error::Error;

    fn try_from(value: &str) -> Result<Self> {
        Ok(match value {
            "connectPeerViaHttp" => Self::ConnectPeerViaHttp,
            "connectWithDid" => Self::ConnectWithDid,
            "connectWithSeed" => Self::ConnectWithSeed,
            "listPeers" => Self::ListPeers,
            "createOffer" => Self::CreateOffer,
            "answerOffer" => Self::AnswerOffer,
            "disconnect" => Self::Disconnect,
            "acceptAnswer" => Self::AcceptAnswer,
            "sendBackendMessage" => Self::SendBackendMessage,
            "sendE2eHandshake" => Self::SendE2eHandshake,
            "sendE2eMessage" => Self::SendE2eMessage,
            "publishMessageToTopic" => Method::PublishMessageToTopic,
            "fetchTopicMessages" => Method::FetchTopicMessages,
            "registerService" => Method::RegisterService,
            "lookupService" => Method::LookupService,
            "lookupOnlineNodes" => Method::LookupOnlineNodes,
            "nodeInfo" => Method::NodeInfo,
            "peerMeasurement" => Method::PeerMeasurement,
            "listPeerMeasurements" => Method::ListPeerMeasurements,
            "nodeDid" => Method::NodeDid,
            _ => return Err(Error::InvalidMethod),
        })
    }
}
