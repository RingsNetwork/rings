use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub enum Method {
    ConnectPeerViaHttp,
    ConnectPeerViaIce,
    ListPeers,
    CreateOffer,
    SendTo,
    Disconnect,
    AcceptAnswer,
}

impl Method {
    pub fn as_str(&self) -> &str {
        match self {
            Method::ConnectPeerViaHttp => "connectPeerViaHttp",
            Method::ConnectPeerViaIce => "connectPeerViaIce",
            Method::ListPeers => "listPeers",
            Method::CreateOffer => "createOffer",
            Method::SendTo => "sendTo",
            Method::Disconnect => "disconnect",
            Method::AcceptAnswer => "acceptAnswer",
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
            "listPeers" => Self::ListPeers,
            "createOffer" => Self::CreateOffer,
            "sendTo" => Self::SendTo,
            "disconnect" => Self::Disconnect,
            "acceptAnswer" => Self::AcceptAnswer,
            _ => return Err(Error::InvalidMethod),
        })
    }
}
