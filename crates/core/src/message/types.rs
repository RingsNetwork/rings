#![deny(missing_docs)]
//! This module defines various message structures in the Rings network.
//! Most of the messages follow the Ping/Pong pattern, where there is a one-to-one correspondence between them,
//! such as xxxSend and xxxReport messages.

use serde::Deserialize;
use serde::Serialize;

use crate::chunk::Chunk;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::Did;
use crate::dht::StorageSyncDelivery;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::message::e2e::E2eHandshakeRequest;
use crate::message::e2e::E2eHandshakeResponse;
use crate::message::e2e::E2eStreamFrame;

/// DHT protocol mode that must match before two peers join the same DHT.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq)]
pub struct DhtProtocolMode {
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    pub network_id: u32,
    /// Storage redundancy required by this DHT protocol mode.
    pub storage_redundancy: u16,
    /// Storage virtual-node positions required by this DHT protocol mode.
    pub dht_virtual_nodes: u16,
}

impl DhtProtocolMode {
    /// Build a DHT protocol mode descriptor.
    pub const fn new(network_id: u32, storage_redundancy: u16, dht_virtual_nodes: u16) -> Self {
        Self {
            network_id,
            storage_redundancy,
            dht_virtual_nodes,
        }
    }

    /// Return whether both descriptors name the same DHT protocol.
    pub fn matches(self, expected: Self) -> bool {
        self == expected
    }
}

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
    /// Storage redundancy required by this DHT protocol mode.
    pub storage_redundancy: u16,
    /// Storage virtual-node positions required by this DHT protocol mode.
    pub dht_virtual_nodes: u16,
}

/// MessageType report to origin with own transport_uuid and handshake_info.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    /// sdp answer of webrtc
    pub sdp: String,
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    pub network_id: u32,
    /// Storage redundancy required by this DHT protocol mode.
    pub storage_redundancy: u16,
    /// Storage virtual-node positions required by this DHT protocol mode.
    pub dht_virtual_nodes: u16,
}

macro_rules! impl_dht_protocol_advertisement {
    ($message:ty, $mode_doc:literal, $match_doc:literal) => {
        impl $message {
            #[doc = $mode_doc]
            pub const fn dht_protocol_mode(&self) -> DhtProtocolMode {
                DhtProtocolMode::new(
                    self.network_id,
                    self.storage_redundancy,
                    self.dht_virtual_nodes,
                )
            }

            #[doc = $match_doc]
            pub fn matches_dht_protocol(&self, expected: DhtProtocolMode) -> bool {
                self.dht_protocol_mode().matches(expected)
            }
        }
    };
}

impl_dht_protocol_advertisement!(
    ConnectNodeSend,
    "Return the DHT protocol mode advertised by this offer.",
    "Return whether this offer belongs to the receiver's DHT protocol mode."
);
impl_dht_protocol_advertisement!(
    ConnectNodeReport,
    "Return the DHT protocol mode advertised by this answer.",
    "Return whether this answer belongs to the initiator's DHT protocol mode."
);

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

/// Overlay liveness probe sent to an admitted peer.
#[derive(Debug, Deserialize, Serialize, Copy, Clone)]
pub struct PeerLivenessProbe {
    /// Sender-local timestamp for logging and correlation.
    pub sent_at_ms: i64,
}

/// Overlay liveness report sent in response to [`PeerLivenessProbe`].
#[derive(Debug, Deserialize, Serialize, Copy, Clone)]
pub struct PeerLivenessReport {
    /// Sender-local timestamp copied from the probe.
    pub sent_at_ms: i64,
}

impl PeerLivenessProbe {
    /// Build a response that proves the receiver processed this probe.
    pub const fn resp(self) -> PeerLivenessReport {
        PeerLivenessReport {
            sent_at_ms: self.sent_at_ms,
        }
    }
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
    /// Post: `Ok(Some(_))` iff this response carries exactly one entry whose DID
    /// equals [`Self::resource`].
    /// Error: more than one entry violates the `SearchEntry -> FoundEntry`
    /// single-resource response model.
    pub(crate) fn single_entry(&self) -> Result<Option<&Entry>> {
        match self.data.as_slice() {
            [] => Ok(None),
            [entry] if entry.did == self.resource => Ok(Some(entry)),
            [_] => Err(Error::InvalidMessage(
                "FoundEntry entry DID does not match searched resource".to_string(),
            )),
            _ => Err(Error::InvalidMessage(
                "FoundEntry carries more than one entry".to_string(),
            )),
        }
    }
}

/// MessageType after `FindSuccessorSend` and syncing data.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SyncEntriesWithSuccessor {
    /// Transition kind controlling whether reports may clean up sender storage.
    pub purpose: StorageSyncPurpose,
    /// Destination semantics used by relay nodes for this sync payload.
    pub destination: StorageSyncDestination,
    /// Entries to sync to the new successor, paired with their placement keys.
    pub data: Vec<PlacedEntry>,
}

impl SyncEntriesWithSuccessor {
    /// Convert a lowered storage-sync delivery into the wire message.
    pub(crate) fn from_delivery(delivery: StorageSyncDelivery) -> Self {
        let (purpose, destination, data) = delivery.into_message_parts();
        Self {
            purpose,
            destination,
            data,
        }
    }
}

/// MessageType used to acknowledge durable storage of synced entries.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SyncEntriesWithSuccessorReport {
    /// Transition kind of the original sync payload.
    pub purpose: StorageSyncPurpose,
    /// Original storage-sync destination semantics.
    pub destination: StorageSyncDestination,
    /// Physical receiver that produced this report.
    pub receiver: Did,
    /// Placement keys and exact values durably persisted by the sync receiver.
    pub acks: Vec<SyncedEntryAck>,
}

impl SyncEntriesWithSuccessorReport {
    /// Build a durable-storage acknowledgement report.
    pub(crate) fn new(
        purpose: StorageSyncPurpose,
        destination: StorageSyncDestination,
        receiver: Did,
        acks: Vec<SyncedEntryAck>,
    ) -> Self {
        Self {
            purpose,
            destination,
            receiver,
            acks,
        }
    }
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

macro_rules! with_message_variants {
    ($macro:ident) => {
        $macro! {
            /// Remote message of try connecting a node.
            0 => ConnectNodeSend(ConnectNodeSend): DhtControl, NoStorageRoute,
            /// Response of ConnectNodeSend.
            1 => ConnectNodeReport(ConnectNodeReport): DhtControl, NoStorageRoute,
            /// Remote message of find successor.
            2 => FindSuccessorSend(FindSuccessorSend): DhtControl, NoStorageRoute,
            /// Response of FindSuccessorSend.
            3 => FindSuccessorReport(FindSuccessorReport): DhtControl, NoStorageRoute,
            /// Remote message of notify a predecessor.
            4 => NotifyPredecessorSend(NotifyPredecessorSend): DhtControl, NoStorageRoute,
            /// Response of NotifyPredecessorSend.
            5 => NotifyPredecessorReport(NotifyPredecessorReport): DhtControl, NoStorageRoute,
            /// Overlay liveness probe.
            6 => PeerLivenessProbe(PeerLivenessProbe): DhtControl, NoStorageRoute,
            /// Overlay liveness probe response.
            7 => PeerLivenessReport(PeerLivenessReport): DhtControl, NoStorageRoute,
            /// Remote message for searching an entry.
            8 => SearchEntry(SearchEntry): Storage, NoStorageRoute,
            /// Response when entries are found.
            9 => FoundEntry(FoundEntry): Storage, NoStorageRoute,
            /// Remote message for entry operations.
            10 => OperateEntry(PlacedEntryOperation): Storage, NoStorageRoute,
            /// Remote message for entry syncing.
            11 => SyncEntriesWithSuccessor(SyncEntriesWithSuccessor): Storage, StorageRoute,
            /// Response after synced entries are durably persisted.
            12 => SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport): Storage, NoStorageRoute,
            /// Custom messages.
            13 => CustomMessage(CustomMessage): Application, NoStorageRoute,
            /// Request to negotiate E2E ElGamal encryption with a signed public key.
            14 => E2eHandshakeRequest(E2eHandshakeRequest): E2e, NoStorageRoute,
            /// Response accepting E2E ElGamal encryption with a signed public key.
            15 => E2eHandshakeResponse(E2eHandshakeResponse): E2e, NoStorageRoute,
            /// Direct ElGamal-encrypted E2E stream frame.
            16 => E2eStreamFrame(E2eStreamFrame): E2e, NoStorageRoute,
            /// Remote message of query topological info of a node.
            17 => QueryForTopoInfoSend(QueryForTopoInfoSend): DhtControl, NoStorageRoute,
            /// Response of QueryForTopoInfoSend.
            18 => QueryForTopoInfoReport(QueryForTopoInfoReport): DhtControl, NoStorageRoute,
            /// A chunk that can be deserialized to a payload.
            19 => Chunk(Chunk): Application, NoStorageRoute,
        }
    };
}
pub(crate) use with_message_variants;

macro_rules! message_requires_storage_route {
    (StorageRoute) => {
        true
    };
    (NoStorageRoute) => {
        false
    };
}

macro_rules! message_storage_destination {
    (StorageRoute, $body:expr) => {
        Some($body.destination)
    };
    (NoStorageRoute, $body:expr) => {{
        let _ = $body;
        None
    }};
}

macro_rules! define_message_model {
    ($( $(#[$docs:meta])* $index:literal => $variant:ident($body:ty): $class:ident, $storage_route:ident ),+ $(,)?) => {
        /// A collection MessageType use for unified management.
        #[derive(Debug, Deserialize, Serialize, Clone)]
        #[non_exhaustive]
        pub enum Message {
            $($(#[$docs])* $variant($body)),+
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub(crate) enum MessageKind {
            $($variant),+
        }

        impl MessageKind {
            #[cfg(test)]
            pub(crate) const WIRE_ORDER: &'static [(u32, Self)] = &[$(($index, Self::$variant)),+];

            const fn from_wire_variant(variant: u32) -> Option<Self> {
                match variant {
                    $($index => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub(crate) fn from_wire(data: &[u8]) -> Result<Self> {
                let variant = rings_codec::deserialize_enum_variant(data)
                    .map_err(Error::CodecDeserialize)?;
                Self::from_wire_variant(variant).ok_or_else(|| {
                    Error::InvalidMessage(format!("unknown message variant {variant}"))
                })
            }

            pub(crate) const fn from_message(message: &Message) -> Self {
                message.kind()
            }

            pub(crate) const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant)),+
                }
            }

            pub(crate) const fn class(self) -> MessageClass {
                match self {
                    $(Self::$variant => MessageClass::$class),+
                }
            }

            pub(crate) const fn is_chunk(self) -> bool {
                matches!(self, Self::Chunk)
            }

            pub(crate) const fn requires_storage_route(self) -> bool {
                match self {
                    $(Self::$variant => message_requires_storage_route!($storage_route)),+
                }
            }

            pub(crate) const fn records_missing_connection_failure(self) -> bool {
                !self.requires_storage_route()
            }
        }

        impl Message {
            pub(crate) const fn storage_sync_destination(&self) -> Option<StorageSyncDestination> {
                match self {
                    $(Self::$variant(body) => message_storage_destination!($storage_route, body)),+
                }
            }

            pub(crate) const fn kind(&self) -> MessageKind {
                match self {
                    $(Self::$variant(_) => MessageKind::$variant),+
                }
            }

            #[cfg(test)]
            pub(crate) fn test_variants() -> Vec<Self> {
                let fixture = tests::MessageFixture::new();
                vec![$(Self::$variant(tests::sample_body::<$body>(&fixture))),+]
            }
        }
    };
}

with_message_variants!(define_message_model);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum MessageClass {
    DhtControl,
    Storage,
    E2e,
    Application,
}

impl MessageClass {
    pub(crate) const COUNT: usize = 4;

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::DhtControl => 0,
            Self::Storage => 1,
            Self::E2e => 2,
            Self::Application => 3,
        }
    }
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
    use bytes::Bytes;

    use super::*;
    use crate::chunk::ChunkMeta;
    use crate::dht::entry::EntryKind;
    use crate::dht::entry::EntryOperation;
    use crate::ecc::SecretKey;

    pub(super) struct MessageFixture {
        did: Did,
        public_key: crate::ecc::PublicKey<33>,
        entry: Entry,
    }

    impl MessageFixture {
        pub(super) fn new() -> Self {
            let secret_key = SecretKey::random();
            let did = secret_key.address().into();
            Self {
                did,
                public_key: secret_key.pubkey(),
                entry: Entry::new(did, Vec::new(), EntryKind::Data),
            }
        }
    }

    pub(super) trait SampleMessageBody: Sized {
        fn sample(fixture: &MessageFixture) -> Self;
    }

    pub(super) fn sample_body<T: SampleMessageBody>(fixture: &MessageFixture) -> T {
        T::sample(fixture)
    }

    macro_rules! sample_message_body {
        ($body:ty, |$fixture:ident| $sample:expr) => {
            impl SampleMessageBody for $body {
                fn sample($fixture: &MessageFixture) -> Self {
                    $sample
                }
            }
        };
    }

    sample_message_body!(ConnectNodeSend, |_fixture| ConnectNodeSend {
        sdp: String::new(),
        network_id: 1,
        storage_redundancy: 1,
        dht_virtual_nodes: 1,
    });
    sample_message_body!(ConnectNodeReport, |_fixture| ConnectNodeReport {
        sdp: String::new(),
        network_id: 1,
        storage_redundancy: 1,
        dht_virtual_nodes: 1,
    });
    sample_message_body!(FindSuccessorSend, |fixture| FindSuccessorSend {
        did: fixture.did,
        strict: false,
        then: FindSuccessorThen::Report(FindSuccessorReportHandler::None),
    });
    sample_message_body!(FindSuccessorReport, |fixture| FindSuccessorReport {
        did: fixture.did,
        handler: FindSuccessorReportHandler::None,
    });
    sample_message_body!(NotifyPredecessorSend, |fixture| NotifyPredecessorSend {
        did: fixture.did,
    });
    sample_message_body!(NotifyPredecessorReport, |fixture| NotifyPredecessorReport {
        did: fixture.did,
    });
    sample_message_body!(PeerLivenessProbe, |fixture| PeerLivenessProbe {
        sent_at_ms: i64::from(fixture.did != Did::from(0_u32)),
    });
    sample_message_body!(PeerLivenessReport, |fixture| PeerLivenessReport {
        sent_at_ms: i64::from(fixture.did != Did::from(0_u32)),
    });
    sample_message_body!(SearchEntry, |fixture| SearchEntry {
        resource: fixture.did,
        placement: fixture.did,
        redundancy: 1,
    });
    sample_message_body!(FoundEntry, |fixture| FoundEntry {
        data: vec![fixture.entry.clone()],
        misses: vec![PlacementMiss::new(fixture.did, fixture.did)],
        resource: fixture.did,
        redundancy: 1,
    });
    sample_message_body!(PlacedEntryOperation, |fixture| PlacedEntryOperation {
        placement: fixture.did,
        op: EntryOperation::Overwrite(fixture.entry.clone()),
    });
    sample_message_body!(SyncEntriesWithSuccessor, |fixture| {
        SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::AdditiveRepair,
            destination: StorageSyncDestination::physical_owner(fixture.did),
            data: vec![PlacedEntry::new(fixture.did, fixture.entry.clone())],
        }
    });
    sample_message_body!(SyncEntriesWithSuccessorReport, |fixture| {
        SyncEntriesWithSuccessorReport::new(
            StorageSyncPurpose::AdditiveRepair,
            StorageSyncDestination::physical_owner(fixture.did),
            fixture.did,
            vec![SyncedEntryAck::new(fixture.did, fixture.entry.clone())],
        )
    });
    sample_message_body!(CustomMessage, |fixture| CustomMessage(
        fixture.did.to_string().into_bytes()
    ));
    sample_message_body!(E2eHandshakeRequest, |fixture| E2eHandshakeRequest {
        requester_public_key: fixture.public_key,
    });
    sample_message_body!(E2eHandshakeResponse, |fixture| E2eHandshakeResponse {
        responder_public_key: fixture.public_key,
    });
    sample_message_body!(E2eStreamFrame, |fixture| E2eStreamFrame {
        stream_id: uuid::Uuid::nil(),
        sender_public_key: fixture.public_key,
        sequence: 0,
        is_final: true,
        ciphertext: Vec::new(),
    });
    sample_message_body!(QueryForTopoInfoSend, |fixture| QueryForTopoInfoSend {
        did: fixture.did,
        then: QueryFor::Stabilization,
    });
    sample_message_body!(QueryForTopoInfoReport, |fixture| QueryForTopoInfoReport {
        info: TopoInfo {
            successors: vec![fixture.did],
            predecessor: Some(fixture.did),
        },
        then: QueryFor::Stabilization,
    });
    sample_message_body!(Chunk, |fixture| Chunk {
        chunk: [0, 1],
        data: Bytes::from(fixture.did.to_string()),
        meta: ChunkMeta::default(),
    });

    fn random_did() -> Did {
        SecretKey::random().address().into()
    }

    #[test]
    fn test_find_successor_send_predicate_names_local_report_rule() {
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
    fn test_find_successor_report_predicate_names_remote_successor() {
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
    fn test_topo_info_query_predicate_names_target_node() {
        let local = random_did();
        let remote = random_did();

        assert!(QueryForTopoInfoSend::new_for_sync(local).targets(local));
        assert!(!QueryForTopoInfoSend::new_for_sync(remote).targets(local));
    }

    #[test]
    fn test_message_metadata_wire_indices_follow_enum_declaration_order() {
        let messages = Message::test_variants();
        assert_eq!(messages.len(), MessageKind::WIRE_ORDER.len());
        for (position, ((wire_index, kind), message)) in
            MessageKind::WIRE_ORDER.iter().zip(messages).enumerate()
        {
            assert_eq!(usize::try_from(*wire_index), Ok(position));
            assert_eq!(message.kind(), *kind);
            assert_eq!(
                kind.requires_storage_route(),
                message.storage_sync_destination().is_some()
            );
        }
    }
}
