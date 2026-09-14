use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use async_trait::async_trait;
use futures::lock::Mutex as FuturesMutex;

use super::PeerRingAction;
use super::RemoteAction;
use super::TopoInfo;
use crate::consts::LOCAL_CACHE_CAPACITY;
use crate::dht::did::BiasId;
use crate::dht::entry::Entry;
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerConvergenceStatus;
use crate::dht::finger::FingerDeferOutcome;
use crate::dht::finger::FingerRetireOutcome;
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
use crate::dht::FingerFixRequest;
use crate::dht::FingerTable;
use crate::dht::LiveDid;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::utils::new_uuid;
use crate::utils::Instant;

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
    /// Sparse/no-wrap finger table plus its convergence metadata.
    finger: Arc<Mutex<FingerTable>>,
    /// Bounded successor list shared with storage placement and routing.
    successor_seq: SuccessorSeq,
    /// Current predecessor learned through Chord notifications.
    predecessor: Arc<Mutex<Option<Did>>>,
    /// Persistent replicated-entry storage.
    pub storage: EntryStorage,
    /// Local fetched-entry cache, bounded at [`LOCAL_CACHE_CAPACITY`] entries.
    pub cache: EntryStorage,
    /// Virtual ownership layout used by storage placement.
    storage_virtual_node_config: VirtualNodeConfig,
    /// Serializes topology transitions that must observe and publish one coherent state snapshot.
    topology_transition: Mutex<()>,
    /// The single stabilization report currently admitted by its UUID correlation token.
    pending_stabilization: Mutex<Option<topology::StabilizationRequest>>,
    /// Successor-sync request ownership that is kept outside [`TopologyState`] because it mirrors
    /// the mutable successor list guarded by [`SuccessorSeq`].
    pending_successor_sync: Mutex<topology::SuccessorSyncState>,
    /// Monotonic origin for finger lookup deadlines and retry backoff inside this node lifecycle.
    finger_clock_origin: Instant,
    /// Stable per-lifecycle entropy used to phase automatic finger maintenance without changing
    /// phase on every browser listener restart.
    finger_jitter_entropy: OnceLock<uuid::Uuid>,
    /// Serializes every read-modify-write of a storage slot (see `chord::storage`).
    pub(super) storage_transition: FuturesMutex<()>,
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
            pending_stabilization: Mutex::new(None),
            pending_successor_sync: Mutex::new(topology::SuccessorSyncState::default()),
            finger_clock_origin: Instant::now(),
            finger_jitter_entropy: OnceLock::new(),
            storage_transition: FuturesMutex::new(()),
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

    /// Snapshot the pure topology state behind the effectful peer-ring shell.
    pub(crate) fn topology_state(&self) -> Result<TopologyState> {
        self.with_topology_state(Clone::clone)
    }

    /// Observe a topology snapshot while no concurrent topology transition can mutate backing
    /// successor, predecessor, finger, or pending-report state.
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

    /// Build a pure [`TopologyState`] from already-serialized mutable fields.
    fn topology_state_unlocked(&self) -> Result<TopologyState> {
        let successors = self.successor_seq.list()?;
        let predecessor = *self.lock_predecessor_state()?;
        let finger = self.lock_finger_state()?;
        let pending_stabilization = *self
            .pending_stabilization
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(TopologyState::restore(
            self.did,
            successors,
            predecessor,
            finger.list().clone(),
            finger.fix_finger_index(),
            finger.convergence_state().clone(),
            pending_stabilization,
        ))
    }

    /// Storage virtual-node configuration used by the DHT storage layer.
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

    /// Apply one already-built topology event and publish the resulting state.
    fn transition_topology(&self, event: TopologyEvent) -> Result<TopologyStep> {
        self.transition_topology_with_observer(event, |_| {})
    }

    /// Apply one topology event while exposing the pre-transition snapshot to the caller.
    ///
    /// The observer runs under the same transition lock as the pure topology step. Callers use it
    /// to answer "did this exact token own the pending operation before the step consumed it?"
    /// without opening a second time-of-check/time-of-use window.
    pub(super) fn transition_topology_with_observer(
        &self,
        event: TopologyEvent,
        observe_snapshot: impl FnOnce(&TopologyState),
    ) -> Result<TopologyStep> {
        self.transition_topology_with_factory(|| event, observe_snapshot)
    }

    /// Apply a lazily-built topology event and commit its projected storage, successor, and finger
    /// table state as one outer-ring mutation.
    ///
    /// Some events need the same monotonic timestamp that is read after the transition lock is held.
    /// The factory keeps those events cheap to build and ensures that all lock-protected state used
    /// by [`topology::step`] comes from a single current snapshot.
    fn transition_topology_with_factory(
        &self,
        event: impl FnOnce() -> TopologyEvent,
        observe_snapshot: impl FnOnce(&TopologyState),
    ) -> Result<TopologyStep> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let current = self.topology_state_unlocked()?;
        observe_snapshot(&current);
        let next = topology::step(&current, event(), self.successor_seq.capacity());
        self.interpret_topology_state_unlocked(&next.state)?;
        if next.state.successors != current.successors {
            self.pending_successor_sync
                .lock()
                .map_err(|_| Error::LockPoisoned)?
                .invalidate();
        }
        Ok(next)
    }

    /// Write a pure [`TopologyState`] projection back into the mutable structures owned by
    /// [`PeerRing`].
    ///
    /// Caller holds `topology_transition`; this function still takes the per-field locks because
    /// those fields are also exposed through older read APIs.
    fn interpret_topology_state_unlocked(&self, next: &TopologyState) -> Result<()> {
        let mut predecessor = self.lock_predecessor_state()?;
        let mut finger = self.lock_finger_state()?;
        let mut pending_stabilization = self
            .pending_stabilization
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        self.successor_seq.replace_state(&next.successors)?;
        *predecessor = next.predecessor;
        finger.replace_state(
            &next.fingers,
            next.fix_finger_index,
            next.finger_convergence_state().clone(),
        );
        *pending_stabilization = next.pending_stabilization();
        Ok(())
    }

    /// Convert a pure topology-side action into the transport/storage action used by `PeerRing`.
    fn topology_action(&self, action: TopologyAction) -> PeerRingAction {
        match action {
            TopologyAction::FindSuccessorForConnect { next, did } => {
                PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessorForConnect(did))
            }
            TopologyAction::FindSuccessorForFix { next, did, request } => {
                PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessorForFix {
                    did,
                    request,
                })
            }
            TopologyAction::QuerySuccessorList(did) => {
                PeerRingAction::RemoteAction(did, RemoteAction::QueryForSuccessorList)
            }
            TopologyAction::Notify(did) => {
                PeerRingAction::RemoteAction(did, RemoteAction::Notify(self.did))
            }
            TopologyAction::QuerySuccessorTopology {
                successor,
                request_id,
            } => PeerRingAction::RemoteAction(
                successor,
                RemoteAction::QueryForSuccessorListAndPred { request_id },
            ),
            // The pass reads the current head when it runs, so the head is not carried.
            TopologyAction::SuccessorHeadChanged(_) => PeerRingAction::StorageRepairDue,
        }
    }

    /// Collapse zero or one topology action into the legacy single-action return shape.
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

    /// Preserve all topology actions when the caller is prepared to execute a batch.
    fn topology_multi_actions(&self, actions: Vec<TopologyAction>) -> PeerRingAction {
        PeerRingAction::MultiActions(
            actions
                .into_iter()
                .map(|action| self.topology_action(action))
                .collect(),
        )
    }

    /// Return elapsed monotonic milliseconds for finger lookup deadlines.
    fn finger_now_ms(&self) -> u64 {
        u64::try_from(self.finger_clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Return the per-node-lifecycle entropy used to spread automatic finger work.
    ///
    /// A browser provider can stop and restart its listener without rebuilding
    /// the ring. Keeping this value on the ring prevents every listener restart
    /// from rerolling its phase while still assigning a new phase after a full
    /// provider reconstruction.
    pub(crate) fn finger_jitter_entropy(&self) -> uuid::Uuid {
        *self.finger_jitter_entropy.get_or_init(new_uuid)
    }

    /// Read the scheduler-facing finger convergence status from the current topology snapshot.
    pub(crate) fn finger_convergence_status(&self) -> Result<FingerConvergenceStatus> {
        let now_ms = self.finger_now_ms();
        // The closure is intentionally pure: it borrows the restored topology snapshot and derives
        // the status without mutating the ring.
        self.with_topology_state(|state| state.finger_convergence_status(now_ms))
    }

    /// Mark every finger slot as needing fresh evidence without emitting the first lookup.
    pub(crate) fn begin_finger_revalidation(&self) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::BeginFingerRevalidation)?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Start one UUID-correlated stabilization round.
    pub(crate) fn begin_stabilization(&self, request_id: uuid::Uuid) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::BeginStabilize { request_id })?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Apply a valid finger proof that names an already connected successor.
    pub(crate) fn apply_fixed_finger(
        &self,
        request: FingerFixRequest,
        successor: Did,
    ) -> Result<FingerApplyOutcome> {
        self.transition_finger_result(|state, now_ms| {
            topology::apply_finger(state, request, successor, now_ms)
        })
    }

    /// Retain a valid finger proof while its successor is being admitted by transport.
    pub(crate) fn defer_fixed_finger(
        &self,
        request: FingerFixRequest,
        successor: Did,
    ) -> Result<FingerDeferOutcome> {
        self.transition_finger_result(|state, now_ms| {
            topology::defer_finger(state, request, successor, now_ms)
        })
    }

    /// Release a retained finger proof after the candidate cannot complete admission.
    pub(crate) fn retire_finger_candidate(
        &self,
        request: FingerFixRequest,
        successor: Did,
    ) -> Result<FingerRetireOutcome> {
        self.transition_finger_result(|state, now_ms| {
            topology::retire_finger_candidate(state, request, successor, now_ms)
        })
    }

    /// Apply a finger-result transition and return the transition-specific outcome.
    fn transition_finger_result<Outcome>(
        &self,
        transition: impl FnOnce(&TopologyState, u64) -> (TopologyStep, Outcome),
    ) -> Result<Outcome> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let current = self.topology_state_unlocked()?;
        let now_ms = self.finger_now_ms();
        let (next, disposition) = transition(&current, now_ms);
        self.interpret_topology_state_unlocked(&next.state)?;
        Ok(disposition)
    }

    /// Apply a successor topology report from the node that owns the matching stabilization token.
    pub(crate) fn stabilize_reported_by(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
        info: TopoInfo,
    ) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::Stabilize {
            reporter,
            request_id: Some(request_id),
            successors: info.successors,
            predecessor: info.predecessor,
        })?;
        Ok(self.topology_multi_actions(next.actions))
    }

    /// Atomically claim a stabilization report token before the async handler starts candidate work.
    pub(crate) fn claim_stabilization_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<bool> {
        let mut claimed = false;
        let _ = self.transition_topology_with_observer(
            TopologyEvent::ClaimStabilize {
                reporter,
                request_id,
            },
            |state| claimed = state.can_claim_stabilization_report(reporter, request_id),
        )?;
        Ok(claimed)
    }

    /// Reserve the next bounded stabilization candidate against the latest topology state.
    pub(crate) fn advance_stabilization_connection_plan(
        &self,
        plan: &mut topology::StabilizationConnectionPlan,
    ) -> Result<topology::StabilizationConnectionStep> {
        self.with_topology_state(|state| plan.advance(state))
    }

    /// Cancel the stabilization request identified by `request_id`.
    pub(crate) fn cancel_stabilization(&self, request_id: uuid::Uuid) -> Result<()> {
        self.transition_topology(TopologyEvent::CancelStabilize { request_id })
            .map(|_| ())
    }

    /// Record that `reporter` is the current owner of a successor-sync report token.
    pub(crate) fn begin_successor_sync(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<bool> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let successors = self.successor_seq.list()?;
        let mut pending = self
            .pending_successor_sync
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(pending.begin(&successors, reporter, request_id))
    }

    /// Claim a successor-sync report once, rejecting stale, duplicate, or post-churn tokens.
    pub(crate) fn claim_successor_sync_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<bool> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let successors = self.successor_seq.list()?;
        let mut pending = self
            .pending_successor_sync
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(pending.claim(&successors, reporter, request_id))
    }

    /// Reserve the next bounded successor-sync candidate against the current successor list.
    pub(crate) fn advance_successor_sync_connection_plan(
        &self,
        plan: &mut topology::SuccessorSyncConnectionPlan,
    ) -> Result<topology::SuccessorSyncConnectionStep> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let successors = self.successor_seq.list()?;
        let pending = self
            .pending_successor_sync
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(plan.advance(&pending, &successors))
    }

    /// Cancel a successor-sync token without mutating the topology graph.
    pub(crate) fn cancel_successor_sync(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<()> {
        let _transition = self
            .topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let mut pending = self
            .pending_successor_sync
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        pending.cancel(reporter, request_id);
        Ok(())
    }

    /// Cancel an outstanding finger lookup and charge it as an explicit failed attempt.
    pub(crate) fn cancel_finger_lookup(&self, request: FingerFixRequest) -> Result<()> {
        self.transition_topology_with_factory(
            || TopologyEvent::CancelFinger {
                request,
                now_ms: self.finger_now_ms(),
            },
            |_| {},
        )
        .map(|_| ())
    }

    /// Advance automatic finger convergence by at most one lookup or local proof step.
    pub(crate) fn advance_finger_convergence(&self) -> Result<PeerRingAction> {
        let next = self.transition_topology_with_factory(
            || TopologyEvent::AdvanceFingerConvergence {
                now_ms: self.finger_now_ms(),
                request_id: new_uuid(),
            },
            |_| {},
        )?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Atomically admit a connected peer and any finger proofs waiting on it.
    pub(crate) fn admit_connected(
        &self,
        peer: Did,
        fixed_fingers: Vec<topology::ConditionalFingerUpdate>,
    ) -> Result<PeerRingAction> {
        let next = self.transition_topology_with_factory(
            move || TopologyEvent::Admit {
                peer,
                fixed_fingers,
                now_ms: self.finger_now_ms(),
            },
            |_| {},
        )?;
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
        self.begin_finger_revalidation()?;
        self.advance_finger_convergence()
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
        self.begin_stabilization(new_uuid())
    }

    fn stabilize(&self, info: TopoInfo) -> Result<PeerRingAction> {
        let reporter = topology::successor_head(&self.topology_state()?).unwrap_or(self.did);
        let next = self.transition_topology(TopologyEvent::Stabilize {
            reporter,
            request_id: None,
            successors: info.successors,
            predecessor: info.predecessor,
        })?;
        Ok(self.topology_multi_actions(next.actions))
    }

    fn topo_info(&self) -> Result<TopoInfo> {
        self.try_into()
    }
}
