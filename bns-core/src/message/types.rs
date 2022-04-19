use crate::dht::Did;
use crate::message::Encoded;
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
pub struct ConnectedNode {
    pub answer_id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct FindSuccessor {
    pub id: Did,
    pub for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FoundSuccessor {
    pub id: Did,
    pub for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NotifyPredecessor {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NotifiedPredecessor {
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
    pub target_id: Did,
    pub data: Vec<Encoded>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub enum Message {
    None,
    JoinDHT(JoinDHT),
    ConnectNode(ConnectNode),
    AlreadyConnected(AlreadyConnected),
    ConnectedNode(ConnectedNode),
    FindSuccessor(FindSuccessor),
    FoundSuccessor(FoundSuccessor),
    NotifyPredecessor(NotifyPredecessor),
    NotifiedPredecessor(NotifiedPredecessor),
    SearchVNode(SearchVNode),
    FoundVNode(FoundVNode),
    StoreVNode(StoreVNode),
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}
