use serde::Deserialize;
use serde::Serialize;

use crate::dht::vnode::VNodeOperation;
use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::error::Result;
use crate::types::ice_transport::HandshakeInfo;

/// MessageType use to ask for connection, send to remote with transport_uuid and handshake_info.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeSend {
    pub transport_uuid: String,
    pub offer: HandshakeInfo,
}

/// MessageType report to origin with own transport_uuid and handshake_info.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    pub transport_uuid: String,
    pub answer: HandshakeInfo,
}

/// MessageType use to find successor in a chord ring.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct FindSuccessorSend {
    pub did: Did,
    pub strict: bool,
    pub then: FindSuccessorThen,
}

/// MessageType use to report origin node with report message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FindSuccessorReport {
    pub did: Did,
    pub handler: FindSuccessorReportHandler,
}

/// MessageType use to notify predecessor, ask for update finger tables.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NotifyPredecessorSend {
    pub did: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType report to origin node.
pub struct NotifyPredecessorReport {
    pub did: Did,
}

/// MessageType use to join chord ring, add did into fingers table.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JoinDHT {
    pub did: Did,
}

/// MessageType use to leave chord ring.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LeaveDHT {
    pub did: Did,
}

/// MessageType use to search virtual node.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchVNode {
    pub vid: Did,
}

/// MessageType report to origin found virtual node.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FoundVNode {
    pub data: Vec<VirtualNode>,
}

/// MessageType after `FindSuccessorSend` and syncing data.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SyncVNodeWithSuccessor {
    pub data: Vec<VirtualNode>,
}

/// MessageType use to customize message, will be handle by `custom_message` method.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CustomMessage(pub Vec<u8>);

/// MessageType enum Report contain FindSuccessorSend.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorThen {
    Report(FindSuccessorReportHandler),
}

/// MessageType enum handle when meet the last node.
/// - None: do nothing but return.
/// - Connect: connect origin node.
/// - FixFingerTable: update fingers table.
/// - SyncStorage: syncing data in virtual node.
/// - CustomCallback: custom callback handle by `custom_message` method.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorReportHandler {
    None,
    Connect,
    FixFingerTable,
    SyncStorage,
    CustomCallback(u8),
}

/// A collection MessageType use for unified management.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    JoinDHT(JoinDHT),
    LeaveDHT(LeaveDHT),
    ConnectNodeSend(ConnectNodeSend),
    ConnectNodeReport(ConnectNodeReport),
    FindSuccessorSend(FindSuccessorSend),
    FindSuccessorReport(FindSuccessorReport),
    NotifyPredecessorSend(NotifyPredecessorSend),
    NotifyPredecessorReport(NotifyPredecessorReport),
    SearchVNode(SearchVNode),
    FoundVNode(FoundVNode),
    OperateVNode(VNodeOperation),
    SyncVNodeWithSuccessor(SyncVNodeWithSuccessor),
    CustomMessage(CustomMessage),
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Message {
    pub fn custom(msg: &[u8]) -> Result<Message> {
        Ok(Message::CustomMessage(CustomMessage(msg.to_vec())))
    }
}
