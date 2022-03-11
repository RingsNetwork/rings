pub mod handler;

mod encoder;
pub use encoder::Encoded;

mod payload;
pub use payload::MessagePayload;

mod msrp;
pub use msrp::{Msrp, MsrpReport, MsrpSend};

use crate::dht::Did;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct ConnectNode {
    pub id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct AlreadyConnected;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct ConnectNodeResponse {
    pub already_connected: bool,
    pub handshake_info: Option<String>,
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct FindSuccessor {
    id: Did,
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct FindSuccessorResponse;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct NotifyPredecessor;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct NotifyPredecessorResponse;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub enum Message {
    ConnectNode(MsrpSend, ConnectNode),
    AlreadyConnected(MsrpReport, AlreadyConnected),
    ConnectNodeResponse(MsrpReport, ConnectNodeResponse),
    FindSuccessor(MsrpSend, FindSuccessor),
    FindSuccessorResponse(MsrpReport, FindSuccessorResponse),
    NotifyPredecessor(MsrpSend, NotifyPredecessor),
    NotifyPredecessorResponse(MsrpReport, NotifyPredecessorResponse),
}
