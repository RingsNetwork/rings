//! A JSONRPC `method` enum.
#![warn(missing_docs)]
use super::error::Error;
use super::error::Result;

/// supported methods.
#[derive(Debug, Clone)]
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
    /// Send custom message to peer
    SendTo,
    /// Disconnect a peer
    Disconnect,
    /// SendCustomMessage,
    SendCustomMessage,
    /// SendBackendMessage
    SendBackendMessage,
    /// Append data to topic
    PublishMessageToTopic,
    /// Fetch data of topic
    FetchMessagesOfTopic,
    /// Register service
    RegisterService,
    /// Lookup service
    LookupService,
    /// Retrieve Node info
    NodeInfo,
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
            Method::SendTo => "sendTo",
            Method::Disconnect => "disconnect",
            Method::AcceptAnswer => "acceptAnswer",
            Method::SendCustomMessage => "sendCustomMessage",
            Method::SendBackendMessage => "sendBackendMessage",
            Method::PublishMessageToTopic => "publishMessageToTopic",
            Method::FetchMessagesOfTopic => "fetchMessagesOfTopic",
            Method::RegisterService => "registerService",
            Method::LookupService => "lookupService",
            Method::NodeInfo => "nodeInfo",
            Method::NodeDid => "nodeDid",
        }
    }
}

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
            "sendTo" => Self::SendTo,
            "disconnect" => Self::Disconnect,
            "acceptAnswer" => Self::AcceptAnswer,
            "sendBackendMessage" => Self::SendBackendMessage,
            "sendCustomMessage" => Self::SendCustomMessage,
            "publishMessageToTopic" => Method::PublishMessageToTopic,
            "fetchMessagesOfTopic" => Method::FetchMessagesOfTopic,
            "registerService" => Method::RegisterService,
            "lookupService" => Method::LookupService,
            "nodeInfo" => Method::NodeInfo,
            "nodeDid" => Method::NodeDid,
            _ => return Err(Error::InvalidMethod),
        })
    }
}
