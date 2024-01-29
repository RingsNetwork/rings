#![warn(missing_docs)]
//! This module defines various message structures in the Rings network.
//! Most of the messages follow the Ping/Pong pattern, where there is a one-to-one correspondence between them,
//! such as xxxSend and xxxReport messages.

use serde::Deserialize;
use serde::Serialize;

use crate::chunk::Chunk;
use crate::dht::vnode::VNodeOperation;
use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::dht::TopoInfo;
use crate::error::Result;

/// The `Then` trait is used to associate a type with a "then" scenario.
pub trait Then {
    /// associated type
    type Then;
}

/// MessageType use to ask for connection, send to remote with transport_uuid and handshake_info.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConnectNodeSend {
    /// sdp offer of webrtc
    pub sdp: String,
}

/// MessageType report to origin with own transport_uuid and handshake_info.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    /// sdp answer of webrtc
    pub sdp: String,
}

/// MessageType use to find successor in a chord ring.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FindSuccessorSend {
    /// did of target
    pub did: Did,
    /// if strict is true, it will try to find the exactly did,
    /// else it will try to find the closest did.
    pub strict: bool,
    /// events should be triggered after found successor
    pub then: FindSuccessorThen,
}

/// MessageType use to report origin node with report message.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FindSuccessorReport {
    /// did of target
    pub did: Did,
    /// handler event after processed `then` of FindSuccessorSend.
    /// Usually it will contains `then` from FindSuccessorSend,
    /// And when sender received report, it should call related handler for the event
    pub handler: FindSuccessorReportHandler,
}

/// MessageType use notify the successor about the predecessor inferred by current node.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NotifyPredecessorSend {
    /// The did of predecessor.
    pub did: Did,
}

/// MessageType use to tell the real predecessor of current node.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NotifyPredecessorReport {
    /// The real predecessor of current node after compare.
    pub did: Did,
}

/// The reason of query successor's TopoInfo
#[derive(Debug, Deserialize, Serialize, Copy, Clone)]
pub enum QueryFor {
    /// For sync successor list from successor
    SyncSuccessor,
    /// For stabilization
    Stabilization,
}

/// MessageType for handle [crate::dht::PeerRingRemoteAction::QueryForSuccessorList]
#[derive(Debug, Deserialize, Serialize, Copy, Clone)]
pub struct QueryForTopoInfoSend {
    /// The did for query target
    pub did: Did,
    /// The reason of query successor's TopoInfo
    pub then: QueryFor,
}

/// MessageType for handle [crate::dht::PeerRingRemoteAction::QueryForSuccessorList]
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct QueryForTopoInfoReport {
    /// The did for query target
    pub info: TopoInfo,
    /// The reason of query successor's TopoInfo
    pub then: QueryFor,
}

impl QueryForTopoInfoSend {
    /// Create new instance with QueryFor::SyncSuccessor
    pub fn new_for_sync(did: Did) -> Self {
        Self {
            did,
            then: QueryFor::SyncSuccessor,
        }
    }

    /// Create new instance with QueryFor::Stabilization
    pub fn new_for_stab(did: Did) -> Self {
        Self {
            did,
            then: QueryFor::Stabilization,
        }
    }

    /// response a send with QueryForTopoInfoSend
    pub fn resp(&self, info: TopoInfo) -> QueryForTopoInfoReport {
        QueryForTopoInfoReport {
            info,
            then: self.then,
        }
    }
}

impl Then for QueryForTopoInfoReport {
    type Then = QueryFor;
}

impl Then for QueryForTopoInfoSend {
    type Then = QueryFor;
}

/// MessageType use to join chord ring, add did into fingers table.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct JoinDHT {
    /// The did for joining
    pub did: Did,
}

/// MessageType use to leave chord ring.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LeaveDHT {
    /// The did for dropping
    pub did: Did,
}

/// MessageType use to search virtual node.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SearchVNode {
    /// The virtual id of searching target
    pub vid: Did,
}

/// MessageType report to origin found virtual node.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FoundVNode {
    /// Response of [SearchVNode], containing response data
    pub data: Vec<VirtualNode>,
}

/// MessageType after `FindSuccessorSend` and syncing data.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SyncVNodeWithSuccessor {
    /// Data of virtual nodes for syncing.
    pub data: Vec<VirtualNode>,
}

/// MessageType use to customize message, will be handle by `custom_message` method.
#[derive(Deserialize, Serialize, Clone)]
pub struct CustomMessage(pub Vec<u8>);

/// MessageType enum Report contain FindSuccessorSend.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[non_exhaustive]
pub enum FindSuccessorThen {
    /// Just Report
    Report(FindSuccessorReportHandler),
}

/// MessageType enum handle when meet the last node.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[non_exhaustive]
pub enum FindSuccessorReportHandler {
    /// None: do nothing but return.
    None,
    /// - Connect: connect origin node.
    Connect,
    /// - FixFingerTable: update fingers table.
    FixFingerTable,
    /// - CustomCallback: custom callback handle by `custom_message` method.
    CustomCallback(u8),
}

/// A collection MessageType use for unified management.
#[derive(Debug, Deserialize, Serialize, Clone)]
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
    /// Remote message of notify a predecessor
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
    /// A chunk that can be deserialized to a payload.
    Chunk(Chunk),
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

impl std::fmt::Debug for CustomMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomMessage")
            .field("size", &self.0.len())
            .finish()
    }
}
