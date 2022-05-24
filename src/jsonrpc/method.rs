use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub enum Method {
    ConnectPeerViaHttp,
    ConnectWithAddress,
    ListPeers,
    CreateOffer,
    AnswerOffer,
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
