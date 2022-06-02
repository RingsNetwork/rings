#![warn(missing_docs)]
///! JSONRpc server supports methods.
use crate::error::{Error, Result};

/// supported methods.
#[derive(Debug, Clone)]
pub enum Method {
    /// Connect peer with remote jsonrpc server url
    ConnectPeerViaHttp,
    /// Connect peer with remote peer's web3 address
    ConnectWithAddress,
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
    /// List all pending connections
    ListPendings,
    /// Close pending connect
    ClosePendingTransport,
}

impl Method {
    /// Return method's name as `&str`
    pub fn as_str(&self) -> &str {
        match self {
            Method::ConnectPeerViaHttp => "connectPeerViaHttp",
            Method::ConnectWithAddress => "connectWithAddress",
            Method::ListPeers => "listPeers",
            Method::CreateOffer => "createOffer",
            Method::AnswerOffer => "answerOffer",
            Method::SendTo => "sendTo",
            Method::Disconnect => "disconnect",
            Method::AcceptAnswer => "acceptAnswer",
            Method::ListPendings => "listPendings",
            Method::ClosePendingTransport => "closePendingTransport",
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
            "connectWithAddress" => Self::ConnectWithAddress,
            "listPeers" => Self::ListPeers,
            "createOffer" => Self::CreateOffer,
            "answerOffer" => Self::AnswerOffer,
            "sendTo" => Self::SendTo,
            "disconnect" => Self::Disconnect,
            "acceptAnswer" => Self::AcceptAnswer,
            "listPendings" => Self::ListPendings,
            "closePendingTransport" => Self::ClosePendingTransport,
            _ => return Err(Error::InvalidMethod),
        })
    }
}
