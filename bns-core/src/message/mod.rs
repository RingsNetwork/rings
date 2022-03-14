pub mod handler;

mod encoder;
pub use encoder::Encoded;

mod payload;
pub use payload::MessageRelay;
pub use payload::MessageRelayMethod;

use crate::dht::Did;
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
    for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FoundSuccessor {
    successor: Did,
    for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotifyPredecessor {
    pub predecessor: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotifiedPredecessor {
    pub predecessor: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Message {
    None,
    ConnectNode(ConnectNode),
    AlreadyConnected(AlreadyConnected),
    ConnectedNode(ConnectedNode),
    FindSuccessor(FindSuccessor),
    FoundSuccessor(FoundSuccessor),
    NotifyPredecessor(NotifyPredecessor),
    NotifiedPredecessor(NotifiedPredecessor),
}
