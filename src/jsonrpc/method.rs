use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub enum Method {
    ConnectPeerViaHttp,
    ConnectPeerViaIce,
    ConnectWithAddress,
    ListPeers,
    CreateOffer,
    SendTo,
    Disconnect,
    AcceptAnswer,
    ListPendings,
    ClosePendingTransport,
}

impl Method {
    pub fn as_str(&self) -> &str {
        match self {
            Method::ConnectPeerViaHttp => "connectPeerViaHttp",
            Method::ConnectPeerViaIce => "connectPeerViaIce",
            Method::ConnectWithAddress => "connectWithAddress",
            Method::ListPeers => "listPeers",
            Method::CreateOffer => "createOffer",
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
            "connectPeerViaIce" => Self::ConnectPeerViaIce,
            "connectWithAddress" => Self::ConnectWithAddress,
            "listPeers" => Self::ListPeers,
            "createOffer" => Self::CreateOffer,
            "sendTo" => Self::SendTo,
            "disconnect" => Self::Disconnect,
            "acceptAnswer" => Self::AcceptAnswer,
            "listPendings" => Self::ListPendings,
            "closePendingTransport" => Self::ClosePendingTransport,
            _ => return Err(Error::InvalidMethod),
        })
    }
}
