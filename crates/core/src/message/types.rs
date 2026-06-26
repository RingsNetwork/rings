#![warn(missing_docs)]
//! This module defines various message structures in the Rings network.
//! Most of the messages follow the Ping/Pong pattern, where there is a one-to-one correspondence between them,
//! such as xxxSend and xxxReport messages.

use serde::Deserialize;
use serde::Serialize;

use crate::chunk::Chunk;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::PlacementMiss;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::Did;
use crate::dht::TopoInfo;
use crate::error::Error;
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
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    pub network_id: u32,
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

impl FindSuccessorSend {
    /// Returns whether this query allows `local` to report its local successor.
    pub(crate) fn accepts_local_successor(&self, local: Did) -> bool {
        !self.strict || self.did == local
    }
}

impl FindSuccessorReport {
    /// Returns whether the reported successor is remote from `local`.
    pub(crate) fn reports_remote_successor(&self, local: Did) -> bool {
        self.did != local
    }
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

    /// Returns whether this query targets `local`.
    pub(crate) fn targets(&self, local: Did) -> bool {
        self.did == local
    }
}

impl Then for QueryForTopoInfoReport {
    type Then = QueryFor;
}

impl Then for QueryForTopoInfoSend {
    type Then = QueryFor;
}

/// MessageType used to search a DHT storage entry.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SearchEntry {
    /// Entry identity being searched.
    pub resource: Did,
    /// Placement key being interrogated.
    pub placement: Did,
    /// Redundancy used by the requester for read-repair after a hit.
    pub redundancy: u16,
}

/// MessageType used to report found DHT storage entries to the origin.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FoundEntry {
    /// Response of [SearchEntry], containing response data
    pub data: Vec<Entry>,
    /// Placement misses observed while answering [SearchEntry].
    pub misses: Vec<PlacementMiss>,
    /// Entry identity searched by the requester.
    pub resource: Did,
    /// Redundancy used by the requester for read-repair after this hit.
    pub redundancy: u16,
}

impl FoundEntry {
    /// Returns the single found entry carried by this response.
    ///
    /// Post: `Ok(None)` iff this is a miss-only response.
    /// Post: `Ok(Some(_))` iff this response carries exactly one entry.
    /// Error: more than one entry violates the `SearchEntry -> FoundEntry`
    /// single-resource response model.
    pub(crate) fn single_entry(&self) -> Result<Option<&Entry>> {
        match self.data.as_slice() {
            [] => Ok(None),
            [entry] => Ok(Some(entry)),
            _ => Err(Error::InvalidMessage(
                "FoundEntry carries more than one entry".to_string(),
            )),
        }
    }
}

/// MessageType after `FindSuccessorSend` and syncing data.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SyncEntriesWithSuccessor {
    /// Entries to sync to the new successor, paired with their placement keys.
    pub data: Vec<PlacedEntry>,
}

/// MessageType used to acknowledge durable storage of synced entries.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SyncEntriesWithSuccessorReport {
    /// Placement keys and exact values durably persisted by the sync receiver.
    pub acks: Vec<SyncedEntryAck>,
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
    /// - FixFingerTable: update one finger table slot.
    FixFingerTable {
        /// Finger slot that the original lookup was fixing.
        index: usize,
    },
    /// - CustomCallback: custom callback handle by `custom_message` method.
    CustomCallback(u8),
}

/// A collection MessageType use for unified management.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[non_exhaustive]
pub enum Message {
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
    /// Remote message for searching an entry.
    SearchEntry(SearchEntry),
    /// Response when entries are found.
    FoundEntry(FoundEntry),
    /// Remote message for entry operations.
    OperateEntry(EntryOperation),
    /// Remote message for entry syncing.
    SyncEntriesWithSuccessor(SyncEntriesWithSuccessor),
    /// Response after synced entries are durably persisted.
    SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport),
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
        write!(f, "{self:?}")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    fn random_did() -> Did {
        SecretKey::random().address().into()
    }

    #[test]
    fn find_successor_send_predicate_names_local_report_rule() {
        let local = random_did();
        let remote = random_did();
        let then = FindSuccessorThen::Report(FindSuccessorReportHandler::None);

        let strict_local = FindSuccessorSend {
            did: local,
            strict: true,
            then: then.clone(),
        };
        assert!(strict_local.accepts_local_successor(local));

        let strict_remote = FindSuccessorSend {
            did: remote,
            strict: true,
            then: then.clone(),
        };
        assert!(!strict_remote.accepts_local_successor(local));

        let relaxed_remote = FindSuccessorSend {
            did: remote,
            strict: false,
            then,
        };
        assert!(relaxed_remote.accepts_local_successor(local));
    }

    #[test]
    fn find_successor_report_predicate_names_remote_successor() {
        let local = random_did();
        let remote = random_did();

        let local_report = FindSuccessorReport {
            did: local,
            handler: FindSuccessorReportHandler::Connect,
        };
        assert!(!local_report.reports_remote_successor(local));

        let remote_report = FindSuccessorReport {
            did: remote,
            handler: FindSuccessorReportHandler::Connect,
        };
        assert!(remote_report.reports_remote_successor(local));
    }

    #[test]
    fn topo_info_query_predicate_names_target_node() {
        let local = random_did();
        let remote = random_did();

        assert!(QueryForTopoInfoSend::new_for_sync(local).targets(local));
        assert!(!QueryForTopoInfoSend::new_for_sync(remote).targets(local));
    }
}
