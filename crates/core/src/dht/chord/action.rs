use serde::Deserialize;
use serde::Serialize;

use super::PeerRing;
use crate::dht::entry::EntryLookupEvidence;
use crate::dht::entry::EntryLookupKey;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::storage::StorageSyncPurpose;
use crate::dht::storage::StorageSyncRoute;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// Describes either a completed peer-ring operation or work to continue remotely.
#[derive(Clone, Debug, PartialEq)]
pub enum PeerRingAction {
    /// No result, the whole manipulation is done internally.
    None,
    /// Found an entry together with lookup evidence.
    SomeEntry(EntryLookupEvidence),
    /// Observed placement misses without a hit.
    EntryMisses(Vec<PlacementMiss>),
    /// Found some node.
    Some(Did),
    /// Trigger a remote action.
    RemoteAction(Did, RemoteAction),
    /// Trigger multiple remote actions.
    MultiActions(Vec<PeerRingAction>),
    /// The placement interval changed: a storage repair round is due. A local intent, carrying
    /// nothing, because the pass reads the ring state when it runs.
    StorageRepairDue,
}

/// Describes the remote continuation required by a peer-ring operation.
///
/// The DID in [`PeerRingAction::RemoteAction`] is the recipient; DIDs stored in
/// this enum are the operation's payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAction {
    /// Ask the recipient to find this DID's successor.
    FindSuccessor(Did),
    /// Ask the recipient to find one entry placement.
    FindEntry(EntryLookupKey),
    /// Ask the recipient to find one placement for operating. Boxed: the carried entry
    /// dominates the size of every other variant.
    FindEntryForOperate(Box<PlacedEntryOperation>),
    /// Send a predecessor notification to the recipient.
    Notify(Did),
    /// Copy placed entries to one storage sync destination.
    SyncEntriesWithSuccessor {
        /// Sync transition kind.
        purpose: StorageSyncPurpose,
        /// Routing semantics for the outer action target.
        route: StorageSyncRoute,
        /// Entries to copy at their placement keys.
        data: Vec<PlacedEntry>,
    },
    /// Find a successor and report it for connection establishment.
    FindSuccessorForConnect(Did),
    /// Find a successor and report its proved finger-table range.
    FindSuccessorForFix {
        /// Ring position whose successor proves the request's lowest finger slot.
        ///
        /// The recipient resolves this position without changing it; the
        /// returned successor may cover later slots only after the receiver
        /// validates the Chord range proof.
        did: Did,
        /// Correlation token that owns this lookup until its report is consumed.
        ///
        /// It binds the requested slot to a unique attempt, preventing a late
        /// or duplicate response from updating a newer convergence round.
        request: crate::dht::FingerFixRequest,
    },
    /// Fetch the recipient's successor list.
    QueryForSuccessorList,
    /// Fetch the recipient's successor list and predecessor for one exact
    /// stabilization request.
    QueryForSuccessorListAndPred {
        /// Correlation token that authorizes exactly one topology report.
        ///
        /// The queried successor echoes this value unchanged; the requester
        /// rejects reports whose token or authenticated sender no longer owns
        /// the active stabilization round.
        request_id: uuid::Uuid,
    },
}

/// Information about a node's successors and predecessor.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct TopoInfo {
    /// Successor list.
    pub successors: Vec<Did>,
    /// Predecessor.
    pub predecessor: Option<Did>,
}

impl TopoInfo {
    /// Retain only peers supported by the caller's current routing evidence.
    pub(crate) fn confirmed_by(&self, mut is_routable: impl FnMut(Did) -> bool) -> Self {
        Self {
            successors: self
                .successors
                .iter()
                .copied()
                .filter(|peer| is_routable(*peer))
                .collect(),
            predecessor: self.predecessor.filter(|peer| is_routable(*peer)),
        }
    }

    /// Return whether any reported topology position survived confirmation.
    pub(crate) fn has_confirmed_peer(&self) -> bool {
        self.predecessor.is_some() || !self.successors.is_empty()
    }
}

impl TryFrom<&PeerRing> for TopoInfo {
    type Error = Error;

    fn try_from(dht: &PeerRing) -> Result<Self> {
        let state = dht.topology_state()?;
        Ok(Self {
            successors: state.successors,
            predecessor: state.predecessor,
        })
    }
}

impl PeerRingAction {
    /// Returns `true` if the action is [`PeerRingAction::SomeEntry`].
    pub fn is_some_entry(&self) -> bool {
        matches!(self, Self::SomeEntry(_))
    }

    /// Returns `true` if the action is [`PeerRingAction::RemoteAction`].
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::RemoteAction(..))
    }
}

impl From<Vec<PeerRingAction>> for PeerRingAction {
    fn from(actions: Vec<PeerRingAction>) -> Self {
        if actions.is_empty() {
            Self::None
        } else {
            Self::MultiActions(actions)
        }
    }
}
