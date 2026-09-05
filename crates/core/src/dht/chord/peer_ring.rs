use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_trait::async_trait;

use super::PeerRingAction;
use super::RemoteAction;
use super::TopoInfo;
use crate::consts::LOCAL_CACHE_CAPACITY;
use crate::dht::did::BiasId;
use crate::dht::entry::Entry;
use crate::dht::finger::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorSeq;
use crate::dht::topology;
use crate::dht::topology::FindSuccessorStep;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyAction;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyState;
use crate::dht::topology::TopologyStep;
use crate::dht::types::Chord;
use crate::dht::types::CorrectChord;
use crate::dht::virtual_node::VirtualNodeConfig;
use crate::dht::Did;
use crate::dht::FingerTable;
use crate::dht::LiveDid;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;

/// Storage accepted by [`PeerRing::new_with_storage`].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type EntryStorage = Box<dyn KvStorageInterface<Entry>>;

/// Storage accepted by [`PeerRing::new_with_storage`].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type EntryStorage = Box<dyn KvStorageInterface<Entry> + Send + Sync>;

/// Chord routing and replicated-storage state for one network peer.
pub struct PeerRing {
    /// The DID of the current node.
    pub did: Did,
    finger: Arc<Mutex<FingerTable>>,
    successor_seq: SuccessorSeq,
    predecessor: Arc<Mutex<Option<Did>>>,
    /// Persistent replicated-entry storage.
    pub storage: EntryStorage,
    /// Local fetched-entry cache, bounded at [`LOCAL_CACHE_CAPACITY`] entries.
    pub cache: EntryStorage,
    storage_virtual_node_config: VirtualNodeConfig,
    topology_transition: Mutex<()>,
}

impl PeerRing {
    /// Construct a peer ring with caller-provided entry storage.
    pub fn new_with_storage(did: Did, succ_max: u8, storage: EntryStorage) -> Self {
        Self::new_with_storage_and_finger_table_size(
            did,
            succ_max,
            storage,
            DEFAULT_FINGER_TABLE_SIZE,
        )
    }

    /// Construct a peer ring with caller-provided storage and finger-table size.
    ///
    /// `Did` is 160-bit. Sizes above [`DEFAULT_FINGER_TABLE_SIZE`] are clamped
    /// by [`FingerTable::new`]; zero disables finger maintenance.
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

    /// Construct a peer ring with storage and virtual ownership configuration.
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
            cache: Box::new(MemStorage::bounded(LOCAL_CACHE_CAPACITY)),
            storage_virtual_node_config: virtual_nodes,
            topology_transition: Mutex::new(()),
            did,
        }
    }

    /// Return the successor sequence.
    #[deprecated(note = "use PeerRing::successors")]
    pub fn lock_successor(&self) -> Result<SuccessorSeq> {
        Ok(self.successor_seq.clone())
    }

    /// Return the successor sequence.
    pub fn successors(&self) -> SuccessorSeq {
        self.successor_seq.clone()
    }

    fn lock_finger_state(&self) -> Result<MutexGuard<'_, FingerTable>> {
        self.finger.lock().map_err(|_| Error::LockPoisoned)
    }

    fn lock_predecessor_state(&self) -> Result<MutexGuard<'_, Option<Did>>> {
        self.predecessor.lock().map_err(|_| Error::LockPoisoned)
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn lock_finger(&self) -> Result<MutexGuard<'_, FingerTable>> {
        self.lock_finger_state()
    }

    #[cfg(test)]
    pub(crate) fn replace_fingers_for_test(&self, fingers: &[(usize, Did)]) -> Result<()> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let mut observed = self.lock_finger_state()?;
        for (index, did) in fingers {
            if *index >= observed.slot_count() {
                return Err(Error::InvalidMessage(format!(
                    "test finger index {index} exceeds slot count {}",
                    observed.slot_count()
                )));
            }
            if *did == self.did {
                return Err(Error::InvalidMessage(
                    "test finger fixture cannot contain the local DID".to_owned(),
                ));
            }
        }
        observed.reset_finger();
        for (index, did) in fingers {
            observed.set(*index, *did);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn lock_predecessor(&self) -> Result<MutexGuard<'_, Option<Did>>> {
        self.lock_predecessor_state()
    }

    /// Remove a node from finger, predecessor, and successor state.
    pub fn remove(&self, did: Did) -> Result<()> {
        self.remove_with_successor_evidence(did, SuccessorRemoval::Preserve)
    }

    /// Remove an unavailable node using transport-validated successor evidence.
    pub(crate) fn remove_unavailable(&self, did: Did, replacements: Vec<Did>) -> Result<()> {
        self.remove_with_successor_evidence(did, SuccessorRemoval::ReplaceWith(replacements))
    }

    /// Post: the emitted actions are dropped. A removal only widens `(self, head]` (a closer
    /// connected peer would already be the head), so its head change moves no placement out of
    /// this node; the caller requests the storage repair round for the widened interval itself.
    fn remove_with_successor_evidence(&self, did: Did, successor: SuccessorRemoval) -> Result<()> {
        self.transition_topology(TopologyEvent::Remove {
            peer: did,
            successor,
        })
        .map(|_| ())
    }

    /// Calculate the DID's clockwise bias from this node.
    pub fn bias(&self, did: Did) -> BiasId {
        BiasId::new(self.did, did)
    }

    pub(crate) fn topology_state(&self) -> Result<TopologyState> {
        self.with_topology_state(Clone::clone)
    }

    pub(crate) fn with_topology_state<T>(
        &self,
        observe: impl FnOnce(&TopologyState) -> T,
    ) -> Result<T> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let state = self.topology_state_unlocked()?;
        Ok(observe(&state))
    }

    fn topology_state_unlocked(&self) -> Result<TopologyState> {
        let successors = self.successor_seq.list()?;
        let predecessor = *self.lock_predecessor_state()?;
        let finger = self.lock_finger_state()?;
        Ok(TopologyState::new(
            self.did,
            successors,
            predecessor,
            finger.list().clone(),
            finger.fix_finger_index(),
        ))
    }

    pub(in crate::dht) const fn storage_virtual_node_config(&self) -> VirtualNodeConfig {
        self.storage_virtual_node_config
    }

    /// The overlay this ring belongs to; every stored value is admitted inside it.
    pub const fn network_id(&self) -> u32 {
        self.storage_virtual_node_config.network_id()
    }

    /// Whether this node is the Chord successor of the position `did`, i.e.
    /// `did ∈ (predecessor, self]`. With no known predecessor the node is responsible only when
    /// it stands alone: a node that has successors but has not yet learned its predecessor is
    /// merely uninformed, not responsible for the whole ring. A message addressed to an
    /// unreachable `did` in this interval has reached the node that must hold it.
    pub(crate) fn is_responsible_for(&self, did: Did) -> Result<bool> {
        self.with_topology_state(|state| topology::is_responsible_for(state, did))
    }

    /// The node this owner routes the position `destination` to, when its own view answers:
    /// the only node whose holds for `destination` this owner admits (see the `inbox` module).
    pub(crate) fn inbox_hold_authority(&self, destination: Did) -> Result<Option<Did>> {
        let state = self.topology_state()?;
        Ok(match topology::find_successor(&state, destination) {
            FindSuccessorStep::Local(responsible) => Some(responsible),
            FindSuccessorStep::Remote { .. } => None,
        })
    }

    fn transition_topology(&self, event: TopologyEvent) -> Result<TopologyStep> {
        self.transition_topology_with_observer(event, |_| {})
    }

    pub(super) fn transition_topology_with_observer(
        &self,
        event: TopologyEvent,
        observe_snapshot: impl FnOnce(&TopologyState),
    ) -> Result<TopologyStep> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let current = self.topology_state_unlocked()?;
        observe_snapshot(&current);
        let next = topology::step(&current, event, self.successor_seq.capacity());
        self.interpret_topology_state_unlocked(&next.state)?;
        Ok(next)
    }

    fn interpret_topology_state_unlocked(&self, next: &TopologyState) -> Result<()> {
        let mut predecessor = self.lock_predecessor_state()?;
        let mut finger = self.lock_finger_state()?;
        self.successor_seq.replace_state(&next.successors)?;
        *predecessor = next.predecessor;
        finger.replace_state(&next.fingers, next.fix_finger_index);
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
            // The pass reads the current head when it runs, so the head is not carried.
            TopologyAction::SuccessorHeadChanged(_) => PeerRingAction::StorageRepairDue,
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

    pub(crate) fn apply_fixed_finger(&self, index: usize, successor: Did) -> Result<()> {
        self.transition_topology(TopologyEvent::ApplyFinger { index, successor })
            .map(|_| ())
    }

    pub(crate) fn admit_connected(
        &self,
        peer: Did,
        fixed_fingers: Vec<topology::ConditionalFingerUpdate>,
    ) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::Admit {
            peer,
            fixed_fingers,
        })?;
        Ok(self.topology_multi_actions(next.actions))
    }
}

impl Chord<PeerRingAction> for PeerRing {
    fn join(&self, did: Did) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::Join { peer: did })?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    fn find_successor(&self, did: Did) -> Result<PeerRingAction> {
        let state = self.topology_state()?;
        let result = match topology::find_successor(&state, did) {
            FindSuccessorStep::Local(successor) => Ok(PeerRingAction::Some(successor)),
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
            result
        );
        result
    }

    fn notify(&self, did: Did) -> Result<Did> {
        let next = self.transition_topology(TopologyEvent::Notify { predecessor: did })?;
        next.state.predecessor.ok_or(Error::PeerRingInvalidAction)
    }

    fn fix_fingers(&self) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::FixFinger)?;
        Ok(self.topology_leaf_actions(next.actions))
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl CorrectChord<PeerRingAction> for PeerRing {
    async fn update_successor(&self, did: impl LiveDid) -> Result<PeerRingAction> {
        if !did.live().await {
            return Ok(PeerRingAction::RemoteAction(
                did.into(),
                RemoteAction::TryConnect,
            ));
        }
        let next = self.transition_topology(TopologyEvent::UpdateSuccessor {
            successor: did.into(),
        })?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    async fn extend_successor(&self, dids: &[impl LiveDid]) -> Result<PeerRingAction> {
        let mut actions = vec![];
        for did in dids {
            if let PeerRingAction::RemoteAction(recipient, action) =
                self.update_successor(did.clone()).await?
            {
                actions.push(PeerRingAction::RemoteAction(recipient, action));
            }
        }
        Ok(PeerRingAction::MultiActions(actions))
    }

    async fn join_then_sync(&self, did: impl LiveDid) -> Result<PeerRingAction> {
        if !did.live().await {
            return Ok(PeerRingAction::None);
        }
        self.admit_connected(did.into(), Vec::new())
    }

    fn rectify(&self, pred: Did) -> Result<()> {
        self.transition_topology(TopologyEvent::Notify { predecessor: pred })
            .map(|_| ())
    }

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

    fn stabilize(&self, info: TopoInfo) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::Stabilize {
            successors: info.successors,
            predecessor: info.predecessor,
        })?;
        Ok(self.topology_multi_actions(next.actions))
    }

    fn topo_info(&self) -> Result<TopoInfo> {
        self.try_into()
    }
}
