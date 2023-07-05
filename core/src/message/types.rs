#![warn(missing_docs)]
//! This module defines various message structures in the Rings network.
//! Most of the messages follow the Ping/Pong pattern, where there is a one-to-one correspondence between them,
//! such as xxxSend and xxxReport messages.

use serde::Deserialize;
use serde::Serialize;

use crate::dht::vnode::VNodeOperation;
use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::error::Result;
use crate::dht::TopoInfo;
use crate::types::ice_transport::HandshakeInfo;

/// MessageType use to ask for connection, send to remote with transport_uuid and handshake_info.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeSend {
    /// uuid of transport
    pub transport_uuid: String,
    /// sdp offer of webrtc
    pub offer: HandshakeInfo,
}

/// MessageType report to origin with own transport_uuid and handshake_info.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    /// uuid of transport
    pub transport_uuid: String,
    /// sdp answer of webrtc
    pub answer: HandshakeInfo,
}

/// MessageType use to find successor in a chord ring.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct FindSuccessorSend {
    /// did of target
    pub did: Did,
    /// if strict is true, it will try to find the exactly did,
    /// eles it will try to find the closest did.
    pub strict: bool,
    /// events should be triggerd after found successor
    pub then: FindSuccessorThen,
}

/// MessageType use to report origin node with report message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FindSuccessorReport {
    /// did of target
    pub did: Did,
    /// handler event after processed `then` of FindSuccessorSend.
    /// Usually it will contains `then` from FindSuccessorSend,
    /// And when sender recieved report, it should call related handler for the event
    pub handler: FindSuccessorReportHandler,
}

/// MessageType use to notify predecessor, ask for update finger tables.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NotifyPredecessorSend {
    /// The did for notify target
    pub did: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType report to origin node.
pub struct NotifyPredecessorReport {
    /// The did for notify target
    pub did: Did,
}


/// MessageType for handle [RemoteAction::Queryforsuccessorlist]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct QueryForTopoInfoSend {
    /// The did for query target
    pub did: Did,
}

/// MessageType for handle [RemoteAction::Queryforsuccessorlist]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct QueryForTopoInfoReport {
    /// The did for query target
    pub info: TopoInfo,
}

/// MessageType use to join chord ring, add did into fingers table.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JoinDHT {
    /// The did for joining
    pub did: Did,
}

/// MessageType use to leave chord ring.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LeaveDHT {
    /// The did for dropping
    pub did: Did,
}

/// MessageType use to search virtual node.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchVNode {
    /// The virtual id of seraching target
    pub vid: Did,
}

/// MessageType report to origin found virtual node.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FoundVNode {
    /// Resposne of [SearchVNode], containing response data
    pub data: Vec<VirtualNode>,
}

/// MessageType after `FindSuccessorSend` and syncing data.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SyncVNodeWithSuccessor {
    /// Data of virtual nodes for syncing.
    pub data: Vec<VirtualNode>,
}

/// MessageType use to customize message, will be handle by `custom_message` method.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CustomMessage(pub Vec<u8>);

/// MessageType enum Report contain FindSuccessorSend.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorThen {
    /// Just Report
    Report(FindSuccessorReportHandler),
}

/// MessageType enum handle when meet the last node.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorReportHandler {
    /// None: do nothing but return.
    None,
    /// - Connect: connect origin node.
    Connect,
    /// - FixFingerTable: update fingers table.
    FixFingerTable,
    /// - SyncStorage: syncing data in virtual node.
    SyncStorage,
    /// - CustomCallback: custom callback handle by `custom_message` method.
    CustomCallback(u8),
}

/// A collection MessageType use for unified management.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// Local message of Join a node to DHT
    JoinDHT(JoinDHT),
    /// Local message of drop a node from DHT
    LeaveDHT(LeaveDHT),
    /// Remote message of try connecting a node.
    ConnectNodeSend(ConnectNodeSend),
    /// Response of ConnectNodeSend
    ConnectNodeReport(ConnectNodeReport),
    /// Remote message of find successor
    FindSuccessorSend(FindSuccessorSend),
    /// Response of FindSuccessorSend
    FindSuccessorReport(FindSuccessorReport),
    /// Remote message of notify a precessor
    NotifyPredecessorSend(NotifyPredecessorSend),
    /// Response of NotifyPredecessorSend
    NotifyPredecessorReport(NotifyPredecessorReport),
    /// Remote message of search a virtual node.
    SearchVNode(SearchVNode),
    /// Response when found a virtual node.
    FoundVNode(FoundVNode),
    /// Remote message of operations of virtual node.
    OperateVNode(VNodeOperation),
    /// Remote message for virtual node syncing.
    SyncVNodeWithSuccessor(SyncVNodeWithSuccessor),
    /// Custom messages
    CustomMessage(CustomMessage),
    /// Remote message of query topological info of a node.
    QueryForTopoInfoSend(QueryForTopoInfoSend),
    /// Response of QueryForTopoInfoSend
    QueryForTopoInfoReport(QueryForTopoInfoReport),
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Message {
    /// Wrap a data of message into CustomMessage.
    pub fn custom(msg: &[u8]) -> Result<Message> {
        Ok(Message::CustomMessage(CustomMessage(msg.to_vec())))
    }
}
