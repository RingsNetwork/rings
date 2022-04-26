use crate::dht::Did;
use crate::message::Encoded;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeSend {
    pub sender_id: Did,
    pub target_id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct AlreadyConnected {
    pub answer_id: Did,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    pub answer_id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct FindSuccessorSend {
    pub id: Did,
    pub for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FindSuccessorReport {
    pub id: Did,
    pub for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NotifyPredecessorSend {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NotifyPredecessorReport {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JoinDHT {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SearchVNode {
    pub sender_id: Did,
    pub target_id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FoundVNode {
    pub target_id: Did,
    pub data: Vec<Encoded>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct StoreVNode {
    pub sender_id: Did,
    pub data: Vec<Encoded>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MultiCall {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub enum Message {
    None,
    MultiCall(MultiCall),
    JoinDHT(JoinDHT),
    ConnectNodeSend(ConnectNodeSend),
    AlreadyConnected(AlreadyConnected),
    ConnectNodeReport(ConnectNodeReport),
    FindSuccessorSend(FindSuccessorSend),
    FindSuccessorReport(FindSuccessorReport),
    NotifyPredecessorSend(NotifyPredecessorSend),
    NotifyPredecessorReport(NotifyPredecessorReport),
    SearchVNode(SearchVNode),
    FoundVNode(FoundVNode),
    StoreVNode(StoreVNode),
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}
