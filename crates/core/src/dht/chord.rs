//! Chord algorithm implement.
#![warn(missing_docs)]
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

use super::did::BiasId;
use super::entry::Entry;
use super::entry::EntryLookupEvidence;
use super::entry::EntryLookupKey;
use super::entry::EntryOperation;
use super::entry::PlacedEntry;
use super::entry::PlacedEntryOperation;
use super::entry::PlacementMiss;
use super::finger::DEFAULT_FINGER_TABLE_SIZE;
use super::storage::StorageSyncPurpose;
use super::storage::StorageSyncRoute;
use super::successor::SuccessorSeq;
use super::topology;
use super::topology::FindSuccessorStep;
use super::topology::TopologyAction;
use super::topology::TopologyEvent;
use super::topology::TopologyState;
use super::types::Chord;
use super::types::ChordStorage;
use super::types::ChordStorageCache;
use super::types::CorrectChord;
use super::virtual_node::VirtualNodeConfig;
use super::FingerTable;
use crate::dht::Did;
use crate::dht::LiveDid;
use crate::dht::SuccessorReader;
use crate::dht::SuccessorWriter;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;

/// `EntryStorage` is the type accepted by `PeerRing::new_with_storage`.
/// It's used to store [Entry]s in a storage media provided by user.
#[cfg(feature = "wasm")]
pub type EntryStorage = Box<dyn KvStorageInterface<Entry>>;

/// `EntryStorage` is the type accepted by `PeerRing::new_with_storage`.
/// It's used to store [Entry]s in a storage media provided by user.
#[cfg(not(feature = "wasm"))]
pub type EntryStorage = Box<dyn KvStorageInterface<Entry> + Send + Sync>;

/// PeerRing is used to help a node interact with other nodes.
/// All nodes in rings network form a clockwise ring in the order of Did.
/// This struct takes its name from that.
/// PeerRing implemented [Chord] algorithm.
/// PeerRing implemented [ChordStorage] protocol.
pub struct PeerRing {
    /// The did of current node.
    pub did: Did,
    /// [FingerTable] help node to find successor quickly.
    pub finger: Arc<Mutex<FingerTable>>,
    /// The next node on the ring.
    /// The [SuccessorSeq] may contain multiple node dids for fault tolerance.
    /// The min did should be same as the first element in finger table.
    pub successor_seq: SuccessorSeq,
    /// The did of previous node on the ring.
    pub predecessor: Arc<Mutex<Option<Did>>>,
    /// Local storage for [ChordStorage].
    pub storage: EntryStorage,
    /// Local cache for [ChordStorage].
    pub cache: EntryStorage,
    /// Storage-only virtual ownership configuration.
    storage_virtual_node_config: VirtualNodeConfig,
}

/// Type alias is just for making the code easy to read.
type Target = Did;

/// `PeerRing` use this to describe the result of [Chord] algorithm. Sometimes it's a
/// direct result, sometimes it's an action that is continued externally.
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
    RemoteAction(Target, RemoteAction),
    /// Trigger multiple remote actions.
    MultiActions(Vec<PeerRingAction>),
}

/// Some of the process needs to be done remotely. This enum is used to describe that.
/// Don't worry about leaving the context. There will be callback machinisim externally
/// that will invoke appropriate methods in `PeerRing` to continue the process.
///
/// To avoid ambiguity, in the following comments, `did_a` is the Did declared in
/// [PeerRingAction]. Other dids are the fields declared in this [RemoteAction].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAction {
    /// Need `did_a` to find `did_b`.
    FindSuccessor(Did),
    /// Need `did_a` to find one entry placement.
    FindEntry(EntryLookupKey),
    /// Need `did_a` to find one placement for operating.
    FindEntryForOperate(PlacedEntryOperation),
    /// Send a predecessor notification to `did_a`.
    ///
    /// `did_a` is the remote recipient from [`PeerRingAction::RemoteAction`].
    /// This field is the predecessor DID announced in `NotifyPredecessorSend`.
    Notify(Did),
    /// Copy placed entries to one storage sync destination.
    SyncEntriesWithSuccessor {
        /// Sync transition kind.
        purpose: StorageSyncPurpose,
        /// Routing semantics for the outer [`PeerRingAction::RemoteAction`] target.
        route: StorageSyncRoute,
        /// Entries to copy at their placement keys.
        data: Vec<PlacedEntry>,
    },

    /// Need `did_a` to find `did_b` then send back with `for connect` flag.
    FindSuccessorForConnect(Did),

    /// Need `did_a` to find `did_b` then send back with `for finger table fixing` flag.
    FindSuccessorForFix {
        /// DID whose successor should populate the finger slot.
        did: Did,
        /// Finger slot that should be updated by the report.
        index: usize,
    },

    /// Fetch successor_list from successor
    QueryForSuccessorList,
    /// Fetch successor_list and pred from successor
    QueryForSuccessorListAndPred,
    /// Try connect to a Node
    TryConnect,
}

/// Information about successor and predecessor
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct TopoInfo {
    /// Successor list
    pub successors: Vec<Did>,
    /// Predecessor
    pub predecessor: Option<Did>,
}

impl TryFrom<&PeerRing> for TopoInfo {
    type Error = Error;
    fn try_from(dht: &PeerRing) -> Result<TopoInfo> {
        let successors = dht.successors().list()?;
        let predecessor = *dht.lock_predecessor()?;
        Ok(TopoInfo {
            successors,
            predecessor,
        })
    }
}

impl PeerRingAction {
    /// Returns `true` if the action is a [PeerRingAction::None] value.
    pub fn is_none(&self) -> bool {
        if let Self::None = self {
            return true;
        }
        false
    }

    /// Returns `true` if the action is a [PeerRingAction::Some] value.
    pub fn is_some(&self) -> bool {
        if let Self::Some(_) = self {
            return true;
        }
        false
    }

    /// Returns `true` if the action is a [PeerRingAction::SomeEntry] value.
    pub fn is_some_entry(&self) -> bool {
        if let Self::SomeEntry(_) = self {
            return true;
        }
        false
    }

    /// Returns `true` if the action is a [PeerRingAction::RemoteAction] value.
    pub fn is_remote(&self) -> bool {
        if let Self::RemoteAction(..) = self {
            return true;
        }
        false
    }

    /// Returns `true` if the action is a [PeerRingAction::MultiActions] value.
    pub fn is_multi(&self) -> bool {
        if let Self::MultiActions(..) = self {
            return true;
        }
        false
    }
}

impl From<Vec<PeerRingAction>> for PeerRingAction {
    fn from(acts: Vec<PeerRingAction>) -> Self {
        if !acts.is_empty() {
            Self::MultiActions(acts)
        } else {
            Self::None
        }
    }
}

impl PeerRing {
    /// Same as new with config, but with a given storage.
    pub fn new_with_storage(did: Did, succ_max: u8, storage: EntryStorage) -> Self {
        Self::new_with_storage_and_finger_table_size(
            did,
            succ_max,
            storage,
            DEFAULT_FINGER_TABLE_SIZE,
        )
    }

    /// Same as new with config, but with a given storage and finger table size.
    ///
    /// `Did` is 160-bit. Sizes above [`DEFAULT_FINGER_TABLE_SIZE`] are clamped
    /// by [`FingerTable::new`]; zero is allowed to disable finger maintenance.
    pub fn new_with_storage_and_finger_table_size(
        did: Did,
        succ_max: u8,
        storage: EntryStorage,
        finger_table_size: usize,
    ) -> Self {
        Self::new_with_storage_finger_table_size_and_virtual_nodes(
            did,
            succ_max,
            storage,
            finger_table_size,
            VirtualNodeConfig::disabled(),
        )
    }

    /// Same as new with config, with a storage virtual-node configuration.
    pub fn new_with_storage_finger_table_size_and_virtual_nodes(
        did: Did,
        succ_max: u8,
        storage: EntryStorage,
        finger_table_size: usize,
        virtual_nodes: VirtualNodeConfig,
    ) -> Self {
        Self {
            successor_seq: SuccessorSeq::new(did, succ_max),
            predecessor: Arc::new(Mutex::new(None)),
            finger: Arc::new(Mutex::new(FingerTable::new(did, finger_table_size))),
            storage,
            cache: Box::new(MemStorage::new()),
            storage_virtual_node_config: virtual_nodes,
            did,
        }
    }

    /// Return successor sequence. This function is deprecated, please use [chord.successors] instead.
    #[deprecated]
    pub fn lock_successor(&self) -> Result<SuccessorSeq> {
        Ok(self.successor_seq.clone())
    }

    /// Return successor sequence
    pub fn successors(&self) -> SuccessorSeq {
        self.successor_seq.clone()
    }

    /// Lock and return MutexGuard of finger table.
    pub fn lock_finger(&self) -> Result<MutexGuard<'_, FingerTable>> {
        self.finger.lock().map_err(|_| Error::DHTSyncLockError)
    }

    /// Lock and return MutexGuard of predecessor.
    pub fn lock_predecessor(&self) -> Result<MutexGuard<'_, Option<Did>>> {
        self.predecessor.lock().map_err(|_| Error::DHTSyncLockError)
    }

    /// Remove a node from finger table.
    /// Also remove it from successor sequence.
    /// If successor_seq become empty, try setting the closest node to it.
    pub fn remove(&self, did: Did) -> Result<()> {
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::Remove { peer: did },
            self.successors().capacity(),
        );
        self.interpret_topology_state(&next.state)
    }

    /// Calculate bias of the Did on the ring.
    pub fn bias(&self, did: Did) -> BiasId {
        BiasId::new(self.did, did)
    }

    pub(super) fn topology_state(&self) -> Result<TopologyState> {
        let finger = self.lock_finger()?;
        Ok(TopologyState::new(
            self.did,
            self.successors().list()?,
            *self.lock_predecessor()?,
            finger.list().clone(),
            finger.fix_finger_index(),
        ))
    }

    pub(super) const fn storage_virtual_node_config(&self) -> VirtualNodeConfig {
        self.storage_virtual_node_config
    }

    fn interpret_topology_state(&self, next: &TopologyState) -> Result<()> {
        let successors = self.successors();
        for did in successors.list()? {
            successors.remove(did)?;
        }
        successors.extend(&next.successors)?;
        *self.lock_predecessor()? = next.predecessor;
        self.lock_finger()?
            .replace_state(&next.fingers, next.fix_finger_index);
        Ok(())
    }

    fn topology_action(&self, action: TopologyAction) -> PeerRingAction {
        match action {
            TopologyAction::FindSuccessorForConnect { next, did } => {
                PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessorForConnect(did))
            }
            TopologyAction::FindSuccessorForFix { next, did, index } => {
                PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessorForFix { did, index })
            }
            TopologyAction::QuerySuccessorList(did) => {
                PeerRingAction::RemoteAction(did, RemoteAction::QueryForSuccessorList)
            }
            TopologyAction::Notify(did) => {
                PeerRingAction::RemoteAction(did, RemoteAction::Notify(self.did))
            }
        }
    }

    fn topology_leaf_actions(&self, actions: Vec<TopologyAction>) -> PeerRingAction {
        let mut actions = actions
            .into_iter()
            .map(|action| self.topology_action(action))
            .collect::<Vec<_>>();
        match actions.len() {
            0 => PeerRingAction::None,
            1 => actions.pop().unwrap_or(PeerRingAction::None),
            _ => PeerRingAction::MultiActions(actions),
        }
    }

    fn topology_multi_actions(&self, actions: Vec<TopologyAction>) -> PeerRingAction {
        PeerRingAction::MultiActions(
            actions
                .into_iter()
                .map(|action| self.topology_action(action))
                .collect(),
        )
    }

    /// Apply a reported finger successor through the pure topology transition.
    pub(crate) fn apply_fixed_finger(&self, index: usize, successor: Did) -> Result<()> {
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::ApplyFinger { index, successor },
            self.successors().capacity(),
        );
        self.interpret_topology_state(&next.state)
    }

    /// Join an incoming replicated entry delta into local storage.
    ///
    /// Post: the stored value is the least upper bound of the previous local
    /// value and `incoming` when a previous value exists; otherwise it is
    /// `incoming` normalized for storage.
    pub(crate) async fn join_storage_entry(&self, key: Did, incoming: Entry) -> Result<Entry> {
        let incoming = incoming.try_into_storage_entry()?;
        let stored = if let Some(local) = self.storage.get(&key.to_string()).await? {
            local.join(incoming)?
        } else {
            incoming
        }
        .try_into_storage_entry()?;
        self.storage.put(&key.to_string(), &stored).await?;
        Ok(stored)
    }
}

impl Chord<PeerRingAction> for PeerRing {
    /// Join a ring containing a node identified by `did`.
    /// This method is usually invoked to maintain successor sequence and finger table
    /// after connect to another node.
    ///
    /// This method will return a [RemoteAction::FindSuccessorForConnect] to the caller.
    /// The caller will send it to the node identified by `did`, and let the node find
    /// the successor of current node and make current node connect to that successor.
    fn join(&self, did: Did) -> Result<PeerRingAction> {
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::Join { peer: did },
            self.successors().capacity(),
        );
        self.interpret_topology_state(&next.state)?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Find the successor of a Did.
    /// May return a remote action for the successor is recorded in another node.
    fn find_successor(&self, did: Did) -> Result<PeerRingAction> {
        let state = self.topology_state()?;
        let succ = match topology::find_successor(&state, did) {
            FindSuccessorStep::Local(successor) => {
                // If the DID is closer to self than the successor head, return
                // that head as the successor. With an empty successor list, the
                // pure topology model returns `self.did`, matching
                // `SuccessorSeq::min`.
                Ok(PeerRingAction::Some(successor))
            }
            FindSuccessorStep::Remote { next, did } => Ok(PeerRingAction::RemoteAction(
                next,
                RemoteAction::FindSuccessor(did),
            )),
        };

        tracing::debug!(
            "find_successor: self: {}, did: {}, successor: {:?}, result: {:?}",
            self.did,
            did,
            state.successors,
            succ
        );

        succ
    }

    /// Handle notification from a node that thinks a did is the predecessor of current node.
    /// The `did` in parameters is the Did of that predecessor.
    /// If that node is closer to current node or current node has no predecessor, set it to the did.
    /// This method will return current predecessor after setting.
    fn notify(&self, did: Did) -> Result<Did> {
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::Notify { predecessor: did },
            self.successors().capacity(),
        );
        let Some(predecessor) = next.state.predecessor else {
            return Err(Error::PeerRingInvalidAction);
        };
        self.interpret_topology_state(&next.state)?;
        Ok(predecessor)
    }

    /// Fix finger table by finding the successor for each finger.
    /// According to the paper, this method should be called periodically.
    /// According to the paper, only one finger should be fixed at a time.
    fn fix_fingers(&self) -> Result<PeerRingAction> {
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::FixFinger,
            self.successors().capacity(),
        );
        self.interpret_topology_state(&next.state)?;
        Ok(self.topology_leaf_actions(next.actions))
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl<const REDUNDANT: u16> ChordStorage<PeerRingAction, REDUNDANT> for PeerRing {
    /// Look up an [`Entry`] by its ring key.
    /// Always finds resource by finger table, ignoring the local cache.
    /// If the `entry_key` is between current node and its successor, its resource should be
    /// stored in current node.
    async fn entry_lookup(&self, entry_key: Did) -> Result<PeerRingAction> {
        let mut ret = vec![];
        let mut misses = vec![];
        // Pre: REDUNDANT > 0, enforced by rotate_affine.
        // Post: if result is SomeEntry(e), e.misses contains exactly the
        // local placement misses observed before the first hit in
        // place(entry_key, REDUNDANT). Later placements are Unknown.
        // Post: EntryMisses carries only observed misses; no remote
        // SearchEntry is emitted solely to classify Unknown as Miss.
        for placement_key in entry_key.rotate_affine(REDUNDANT)? {
            let query = EntryLookupKey::new(entry_key, placement_key);
            let act = match self.find_storage_owner(placement_key) {
                // Resource should be stored in current node.
                Ok(PeerRingAction::Some(succ)) => {
                    match self.storage.get(&placement_key.to_string()).await {
                        Ok(Some(v)) => {
                            let observed_misses = std::mem::take(&mut misses);
                            Ok(PeerRingAction::SomeEntry(EntryLookupEvidence::new(
                                v,
                                observed_misses,
                            )))
                        }
                        Ok(None) => {
                            tracing::debug!(
                                "Cannot find entry in local storage, try to query from successor"
                            );
                            // If cannot find and has successor, try to query it from successor.
                            // This is useful when the node is just joined and has not stabilized yet.
                            if succ == self.did {
                                misses.push(PlacementMiss::new(placement_key, succ));
                                Ok(PeerRingAction::None)
                            } else {
                                Ok(PeerRingAction::RemoteAction(
                                    succ,
                                    RemoteAction::FindEntry(query),
                                ))
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                // Resource is stored in other nodes.
                // Return an action to describe how to find it.
                Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(id))) => {
                    Ok(PeerRingAction::RemoteAction(
                        n,
                        RemoteAction::FindEntry(EntryLookupKey::new(entry_key, id)),
                    ))
                }
                Ok(a) => Err(Error::unexpected_peer_ring_action(a)),
                Err(e) => Err(e),
            }?;
            if act.is_remote() {
                ret.push(act);
            } else {
                // If found entry, break and return directly
                if act.is_some_entry() {
                    return Ok(act);
                }
            }
        }
        if !misses.is_empty() {
            ret.push(PeerRingAction::EntryMisses(misses));
        }
        Ok(ret.into())
    }

    /// Handle [EntryOperation] if the target entry between current node and the
    /// successor of current node, otherwise find the responsible node and return
    /// as Action.
    async fn entry_operate(&self, op: EntryOperation) -> Result<PeerRingAction> {
        let op = op.stamped(self.did)?;
        let entry_key = op.did()?;
        let mut ret = vec![];
        // Pre: op.did() is the entry identity id(e), and REDUNDANT > 0 is
        // checked by rotate_affine.
        // Post: for every k in place(id(e), REDUNDANT), either sigma_self[k]
        // is updated with Entry::operate(op) when self is the observed owner,
        // or exactly one FindEntryForOperate action is emitted toward the
        // current routing owner for k.
        // Preservation: no placement outside place(id(e), REDUNDANT) is
        // written by this transition.
        for entry_key in entry_key.rotate_affine(REDUNDANT)? {
            let act = match self.find_storage_owner(entry_key) {
                // `entry` should be on current node.
                Ok(PeerRingAction::Some(_)) => {
                    let this = match self.storage.get(&entry_key.to_string()).await? {
                        Some(this) => this,
                        None => op.clone().gen_default_entry()?,
                    };
                    let entry = this.operate(op.clone(), self.did)?;
                    self.join_storage_entry(entry_key, entry).await?;
                    Ok(PeerRingAction::None)
                }
                // `entry` should be on other nodes.
                // Return an action to describe how to store it.
                Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(_))) => {
                    Ok(PeerRingAction::RemoteAction(
                        n,
                        RemoteAction::FindEntryForOperate(PlacedEntryOperation {
                            placement: entry_key,
                            op: op.clone(),
                        }),
                    ))
                }
                Ok(a) => Err(Error::unexpected_peer_ring_action(a)),
                Err(e) => Err(e),
            }?;
            if act.is_remote() {
                ret.push(act);
            }
        }
        Ok(ret.into())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorageCache<PeerRingAction> for PeerRing {
    /// Cache fetched `entry` locally.
    async fn local_cache_put(&self, entry: Entry) -> Result<()> {
        self.cache.put(&entry.did.to_string(), &entry).await
    }

    /// Get entry from local cache.
    async fn local_cache_get(&self, entry_key: Did) -> Result<Option<Entry>> {
        self.cache.get(&entry_key.to_string()).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl CorrectChord<PeerRingAction> for PeerRing {
    /// When Chord have a new successor, ask the new successor for successor list
    async fn update_successor(&self, did: impl LiveDid) -> Result<PeerRingAction> {
        let is_live = did.live().await;
        if !is_live {
            return Ok(PeerRingAction::RemoteAction(
                did.into(),
                RemoteAction::TryConnect,
            ));
        }
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::UpdateSuccessor {
                successor: did.into(),
            },
            self.successors().capacity(),
        );
        self.interpret_topology_state(&next.state)?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    async fn extend_successor(&self, dids: &[impl LiveDid]) -> Result<PeerRingAction> {
        let mut ret: Vec<PeerRingAction> = vec![];
        for did in dids {
            if let PeerRingAction::RemoteAction(r, act) = self.update_successor(did.clone()).await?
            {
                ret.push(PeerRingAction::RemoteAction(r, act))
            }
        }
        Ok(PeerRingAction::MultiActions(ret))
    }

    /// Join Operation in the paper.
    /// Zave's work differs from the original Chord paper in that it requires
    /// a newly joined node to synchronize its successors from remote nodes.
    async fn join_then_sync(&self, did: impl LiveDid) -> Result<PeerRingAction> {
        let is_live = did.live().await;
        if !is_live {
            return Ok(PeerRingAction::None);
        }
        let mut ret: Vec<PeerRingAction> = vec![];
        let succ_act = self.update_successor(did.clone()).await?;
        if succ_act.is_remote() {
            ret.push(succ_act)
        }
        let join_act = self.join(did.into())?;
        ret.push(join_act);

        Ok(PeerRingAction::MultiActions(ret))
    }

    /// HMCC/Zave Rectify operation.
    ///
    /// Rectify is the local predecessor transition run when this node receives
    /// a predecessor notification from `pred`. It has no remote action: the
    /// message layer's report path is handled by `NotifyPredecessorSend`.
    fn rectify(&self, pred: Did) -> Result<()> {
        // Pre: in protocol traces, pred is the notifier's DID and pred != self.did.
        // Post: predecessor' = pred iff predecessor = None or
        // bias(predecessor) < bias(pred); otherwise predecessor is unchanged.
        // Preservation: successor list, finger table, and storage state are
        // unchanged. Delegating to Chord::notify is exactly this predecessor
        // choice rule; Rectify discards the returned predecessor because it
        // emits no follow-up action.
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::Notify { predecessor: pred },
            self.successors().capacity(),
        );
        self.interpret_topology_state(&next.state)
    }

    /// Pre-Stabilize Operation:
    /// Before stabilizing, the node should query its first successor for TopoInfo.
    /// If there are no successors, return PeerRingAction::None.
    fn pre_stabilize(&self) -> Result<PeerRingAction> {
        let successor = self.successors();
        if successor.is_empty()? {
            return Ok(PeerRingAction::None);
        }
        let head = successor.min()?;
        Ok(PeerRingAction::RemoteAction(
            head,
            RemoteAction::QueryForSuccessorListAndPred,
        ))
    }

    /// Stabilize Operation:
    ///
    /// Mirrors the TLA+-style `CorrectStabilize` operator in
    /// `tests/default/dht_convergence.rs`.
    /// The old head is captured before updating successors for the improved-successor
    /// query check; the remote successor list contributes `but_last`; and notify
    /// is emitted for the post-update head when that head is not self.
    fn stabilize(&self, info: TopoInfo) -> Result<PeerRingAction> {
        let next = topology::step(
            &self.topology_state()?,
            TopologyEvent::Stabilize {
                successors: info.successors,
                predecessor: info.predecessor,
            },
            self.successors().capacity(),
        );
        self.interpret_topology_state(&next.state)?;
        Ok(self.topology_multi_actions(next.actions))
    }

    /// A function to provide topological information about the chord.
    fn topo_info(&self) -> Result<TopoInfo> {
        self.try_into()
    }
}

#[cfg(all(not(feature = "wasm"), test))]
#[path = "chord_tests.rs"]
mod tests;
