pub mod handler;

mod encoder;
pub use encoder::Encoded;

mod payload;
pub use payload::MessagePayload;

mod msrp;
pub use msrp::{MsrpReport, MsrpSend};

use crate::routing::Did;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectNode {
    pub sender_id: Did,
    pub target_id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct AlreadyConnected {
    pub answer_id: Did,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeResponse {
    pub answer_id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct FindSuccessor {
    id: Did,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct FindSuccessorResponse;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct NotifyPredecessor;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
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
