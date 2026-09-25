//! Effectful shell around the pure Chord topology model.
//!
//! [`PeerRing`] owns the mutable routing state of one node (successor list,
//! predecessor, finger table) together with the correlation tokens of the
//! maintenance rounds that are allowed to change it. It carries no protocol
//! logic of its own: every mutation is the image of one pure transition
//! `topology::step : TopologyState × TopologyEvent → TopologyStep`, and the
//! shell performs only the three effects the pure model cannot:
//!
//! 1. **Snapshot.** Assemble one coherent [`TopologyState`] from the per-field
//!    locks.
//! 2. **Step.** Run the pure transition on that snapshot, supplying the
//!    monotonic clock reading and the fresh request token it asks for.
//! 3. **Commit.** Write the next state back and translate the transition's
//!    [`TopologyAction`]s into the [`PeerRingAction`]s the transport executes.
//!
//! All three run under one lock, `topology_transition`, so transitions are
//! totally ordered: a snapshot never mixes fields from two states, and each
//! commit is visible to the next snapshot. Read-only views take the same lock
//! for the same reason.
//!
//! Successor-list synchronization tokens are the one piece of state kept
//! beside the model rather than inside it. A token is valid only while its
//! reporter is a successor, so the transition core prunes the tokens of
//! departed reporters whenever a commit changes the successor list.
//!
//! The `impl` blocks below are grouped by concern: construction, read-only
//! views, the transition core, membership, finger convergence, stabilization
//! rounds, successor-list synchronization, and test hooks.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use futures::lock::Mutex as FuturesMutex;

use super::PeerRingAction;
use super::RemoteAction;
use super::TopoInfo;
use crate::consts::LOCAL_CACHE_CAPACITY;
use crate::dht::delivery;
use crate::dht::delivery::NextHop;
use crate::dht::delivery::Origination;
use crate::dht::delivery::RouteStage;
use crate::dht::entry::Entry;
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerConvergenceStatus;
use crate::dht::finger::FingerDeferOutcome;
use crate::dht::finger::FingerRetireOutcome;
use crate::dht::finger::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::successor::SuccessorSeq;
use crate::dht::topology;
use crate::dht::topology::FindSuccessorStep;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyAction;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyRemoval;
use crate::dht::topology::TopologyState;
use crate::dht::topology::TopologyStep;
use crate::dht::types::Chord;
use crate::dht::virtual_node::VirtualNodeConfig;
use crate::dht::Did;
use crate::dht::FingerFixRequest;
use crate::dht::FingerTable;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::utils::new_uuid;
use crate::utils::Instant;

/// Storage accepted by [`PeerRing::new_with_storage`].
pub type EntryStorage = Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<Entry>)>;

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
    /// Total order of topology transitions and of the views taken between them
    /// (see the module documentation).
    topology_transition: Mutex<()>,
    /// Stabilization round whose authenticated report may mutate topology.
    ///
    /// Part of the pure model: the token moves `Requested → Processing` by
    /// transition, and consuming or cancelling it removes its authority, so a
    /// stale report cannot start connection work.
    pending_stabilization: Mutex<Option<topology::StabilizationRequest>>,
    /// Per-successor ownership of outstanding successor-list queries.
    ///
    /// Kept beside the pure model because a token is valid only while its
    /// reporter is still a successor; the transition core prunes departed
    /// reporters' tokens when a commit changes the successor list.
    pending_successor_sync: Mutex<topology::SuccessorSyncState>,
    /// Origin of the monotonic clock behind every finger deadline and retry
    /// floor.
    ///
    /// Timestamps are elapsed milliseconds from this instant, so a wall-clock
    /// change can neither extend nor prematurely expire an in-flight lookup.
    clock_origin: Instant,
    /// Lazily initialized entropy used to phase automatic finger maintenance.
    ///
    /// Stable for the lifetime of this ring, including browser listener
    /// restarts, so a restart cannot reroll the node's phase; a newly
    /// constructed ring draws a new phase.
    finger_jitter_entropy: OnceLock<uuid::Uuid>,
    /// Serializes every read-modify-write of a storage slot (see `chord::storage`).
    pub(super) storage_transition: FuturesMutex<()>,
}

/// Construction.
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
            clock_origin: Instant::now(),
            finger_jitter_entropy: OnceLock::new(),
            storage_transition: FuturesMutex::new(()),
            did,
        }
    }
}

/// Read-only views.
///
/// Every view observes one committed state and mutates nothing.
impl PeerRing {
    /// Return the successor sequence.
    pub fn successors(&self) -> SuccessorSeq {
        self.successor_seq.clone()
    }

    /// The overlay this ring belongs to; every stored value is admitted inside it.
    pub const fn network_id(&self) -> u32 {
        self.storage_virtual_node_config.network_id()
    }

    /// Storage virtual-node configuration used by the DHT storage layer.
    pub(in crate::dht) const fn storage_virtual_node_config(&self) -> VirtualNodeConfig {
        self.storage_virtual_node_config
    }

    /// An owned copy of the current topology state.
    pub(crate) fn topology_state(&self) -> Result<TopologyState> {
        self.with_topology_state(Clone::clone)
    }

    /// Run a pure observation `TopologyState → T` against one coherent snapshot.
    ///
    /// The transition lock is held while the snapshot is assembled, so
    /// `observe` sees exactly the state the next transition would start from,
    /// never a torn view in which one field has already moved on. `observe`
    /// must be pure: it borrows the snapshot, cannot reach the ring, and must
    /// not block, because every transition waits behind the lock while it
    /// runs. Prefer this over [`Self::topology_state`] when the answer is
    /// smaller than the state, since it avoids copying the finger table.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn with_topology_state<T>(
        &self,
        observe: impl FnOnce(&TopologyState) -> T,
    ) -> Result<T> {
        let _transition = self.lock_transition()?;
        let state = self.snapshot_unlocked()?;
        Ok(observe(&state))
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
        self.with_topology_state(|state| match topology::find_successor(state, destination) {
            FindSuccessorStep::Local(responsible) => Some(responsible),
            FindSuccessorStep::Remote { .. } => None,
        })
    }

    /// The delivery decision for a payload addressed to the node `destination` whose carrier
    /// is in `stage`, against one coherent snapshot; see [`delivery::delivery_step`] for the
    /// step and its laws. `linked` is the transport's direct-link predicate.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn delivery_step(
        &self,
        destination: Did,
        stage: RouteStage,
        linked: impl Fn(Did) -> bool,
    ) -> Result<Option<NextHop>> {
        self.with_topology_state(|view| delivery::delivery_step(view, destination, stage, linked))
    }

    /// The peer this node names for its reports while no node is known to route to it; see
    /// [`topology::reply_via`].
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn reply_via(&self) -> Result<Option<Did>> {
        self.with_topology_state(topology::reply_via)
    }

    /// The first hop and the `reply_via` of a request this node originates toward
    /// `destination`, from one coherent snapshot; see [`delivery::origination`].
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn origination(
        &self,
        destination: Did,
        linked: impl Fn(Did) -> bool,
    ) -> Result<Origination> {
        self.with_topology_state(|view| delivery::origination(view, destination, linked))
    }
}

/// Transition core: `snapshot → step → commit` under `topology_transition`.
///
/// Every mutation of routing state goes through [`Self::transition`]. The
/// other functions here are its lock, clock, snapshot, commit, and
/// action-translation helpers.
impl PeerRing {
    fn lock_transition(&self) -> Result<MutexGuard<'_, ()>> {
        self.topology_transition
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    fn lock_finger_state(&self) -> Result<MutexGuard<'_, FingerTable>> {
        self.finger.lock().map_err(|_| Error::LockPoisoned)
    }

    fn lock_predecessor_state(&self) -> Result<MutexGuard<'_, Option<Did>>> {
        self.predecessor.lock().map_err(|_| Error::LockPoisoned)
    }

    fn lock_pending_stabilization(
        &self,
    ) -> Result<MutexGuard<'_, Option<topology::StabilizationRequest>>> {
        self.pending_stabilization
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    fn lock_pending_successor_sync(&self) -> Result<MutexGuard<'_, topology::SuccessorSyncState>> {
        self.pending_successor_sync
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// Monotonic milliseconds since this ring was constructed: the clock
    /// domain of every finger deadline and retry floor.
    ///
    /// Saturates at [`u64::MAX`]; never reads wall time.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Assemble one [`TopologyState`] from the per-field locks.
    ///
    /// Caller holds `topology_transition`; the per-field locks are still taken
    /// because those fields are also exposed through older read APIs.
    fn snapshot_unlocked(&self) -> Result<TopologyState> {
        let successors = self.successor_seq.list()?;
        let predecessor = *self.lock_predecessor_state()?;
        let finger = self.lock_finger_state()?;
        let pending_stabilization = *self.lock_pending_stabilization()?;
        Ok(TopologyState::restore(
            self.did,
            successors,
            predecessor,
            finger.list().clone(),
            finger.convergence_state().clone(),
            pending_stabilization,
        ))
    }

    /// Write a [`TopologyState`] back into the per-field locks.
    ///
    /// Caller holds `topology_transition`, as for [`Self::snapshot_unlocked`].
    fn commit_unlocked(&self, next: &TopologyState) -> Result<()> {
        let mut predecessor = self.lock_predecessor_state()?;
        let mut finger = self.lock_finger_state()?;
        let mut pending_stabilization = self.lock_pending_stabilization()?;
        self.successor_seq.replace_state(&next.successors)?;
        *predecessor = next.predecessor;
        finger.replace_state(&next.fingers, next.finger_convergence_state().clone());
        *pending_stabilization = next.pending_stabilization();
        Ok(())
    }

    /// Run one pure transition against the current snapshot and commit its state.
    ///
    /// `transition` receives the snapshot and the clock reading taken after the
    /// lock was acquired, and returns the pure step together with whatever
    /// outcome the caller wants to report (a finger disposition, a claim
    /// result). The next state is committed before either is returned, so no
    /// caller can observe an outcome whose state did not land. A commit that
    /// changes the successor list also prunes the successor-sync tokens of
    /// reporters that are no longer successors (see the module documentation).
    ///
    /// # Errors
    ///
    /// Returns an error when a lock is poisoned or the successor list cannot
    /// be read or replaced.
    fn transition<Outcome>(
        &self,
        transition: impl FnOnce(&TopologyState, u64) -> (TopologyStep, Outcome),
    ) -> Result<(TopologyStep, Outcome)> {
        let _transition = self.lock_transition()?;
        let current = self.snapshot_unlocked()?;
        let (next, outcome) = transition(&current, self.now_ms());
        self.commit_unlocked(&next.state)?;
        if next.state.successors != current.successors {
            self.lock_pending_successor_sync()?
                .retain_current(&next.state.successors);
        }
        Ok((next, outcome))
    }

    /// Apply one [`TopologyEvent`] and commit the result.
    fn transition_topology(&self, event: TopologyEvent) -> Result<TopologyStep> {
        self.transition_topology_at(|_| event)
    }

    /// Apply an event that carries the transition's own clock reading.
    ///
    /// The reading is taken under the lock, so the event's deadline arithmetic
    /// and the snapshot it is applied to belong to the same instant.
    fn transition_topology_at(
        &self,
        event: impl FnOnce(u64) -> TopologyEvent,
    ) -> Result<TopologyStep> {
        self.transition(|state, now_ms| (self.step(state, event(now_ms)), ()))
            .map(|(step, ())| step)
    }

    /// The pure transition, closed over this ring's successor capacity.
    fn step(&self, state: &TopologyState, event: TopologyEvent) -> TopologyStep {
        topology::step(state, event, self.successor_seq.capacity())
    }

    /// Translate one pure topology action into the action the transport executes.
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

    /// Translate a transition's actions for callers of the single-action API:
    /// `None` for no action, the action itself for one, a batch otherwise.
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

    /// Translate a transition's actions for callers that always execute a batch.
    fn topology_multi_actions(&self, actions: Vec<TopologyAction>) -> PeerRingAction {
        PeerRingAction::MultiActions(
            actions
                .into_iter()
                .map(|action| self.topology_action(action))
                .collect(),
        )
    }
}

/// Membership: admitting connected peers and removing unreachable ones.
impl PeerRing {
    /// Atomically admit a connected peer and the finger proof, if any, that
    /// waited on it.
    pub(crate) fn admit_connected(
        &self,
        peer: Did,
        deferred_proof: Option<FingerFixRequest>,
    ) -> Result<PeerRingAction> {
        let next = self.transition_topology_at(|now_ms| TopologyEvent::Admit {
            peer,
            deferred_proof,
            now_ms,
        })?;
        Ok(self.topology_multi_actions(next.actions))
    }

    /// Remove a node from finger, predecessor, and successor state.
    ///
    /// Returns whether the node was referenced, decided under the transition
    /// lock on the state the removal was applied to.
    pub fn remove(&self, did: Did) -> Result<TopologyRemoval> {
        self.remove_with_successor_evidence(did, SuccessorRemoval::Preserve)
    }

    /// Remove an unavailable node using transport-validated successor evidence.
    pub(crate) fn remove_unavailable(
        &self,
        did: Did,
        replacements: Vec<Did>,
    ) -> Result<TopologyRemoval> {
        self.remove_with_successor_evidence(did, SuccessorRemoval::ReplaceWith(replacements))
    }

    /// Post: the emitted actions are dropped. A removal only widens `(self, head]` (a closer
    /// connected peer would already be the head), so its head change moves no placement out of
    /// this node; the caller requests the storage repair round for the widened interval itself,
    /// and only when the returned [`TopologyRemoval`] says a slot was vacated.
    fn remove_with_successor_evidence(
        &self,
        did: Did,
        successor: SuccessorRemoval,
    ) -> Result<TopologyRemoval> {
        let (_, removal) = self.transition(|state, _| {
            let removal = TopologyRemoval::of(state, did);
            let event = TopologyEvent::Remove {
                peer: did,
                successor,
            };
            (self.step(state, event), removal)
        })?;
        Ok(removal)
    }
}

/// Finger convergence.
///
/// The pure lifecycle of a finger lookup lives in `dht::finger::convergence`;
/// this block only supplies its effects: the monotonic clock, fresh request
/// tokens, and the serialized commit. In order of occurrence:
///
/// 1. [`Self::begin_finger_revalidation`] marks the ranges whose proof must be
///    refreshed.
/// 2. [`Self::advance_finger_convergence`] emits at most one lookup, owned by
///    a fresh UUID.
/// 3. The lookup's report is applied by [`Self::apply_fixed_finger`] when its
///    successor is already connected, or parked by [`Self::defer_fixed_finger`]
///    under an admission lease while transport connects it; a lease that
///    cannot be honoured is released by [`Self::retire_finger_candidate`].
/// 4. [`Self::cancel_finger_lookup`] charges a lookup that transport could not
///    deliver as one failed attempt.
///
/// Every step compares the caller's token with the token the model currently
/// owns, so a late or replayed report never moves newer state.
impl PeerRing {
    /// Per-ring entropy used to spread automatic finger work.
    ///
    /// Initialized at most once, so concurrent callers and browser listener
    /// restarts all observe the same phase.
    pub(crate) fn finger_jitter_entropy(&self) -> uuid::Uuid {
        *self.finger_jitter_entropy.get_or_init(new_uuid)
    }

    /// The scheduler's view of finger convergence: the current phase and the
    /// time left until its next deadline, relative to the clock reading taken
    /// here.
    ///
    /// The status is a pure function of one topology snapshot and the clock,
    /// so it is computed inside [`Self::with_topology_state`] rather than on a
    /// copied state; nothing is advanced or mutated.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn finger_convergence_status(&self) -> Result<FingerConvergenceStatus> {
        let now_ms = self.now_ms();
        self.with_topology_state(|state| state.finger_convergence_status(now_ms))
    }

    /// Begin a finger-table revalidation pass without emitting a lookup.
    ///
    /// Marking ranges stale and emitting paced network work are kept separate;
    /// the caller drives the latter through
    /// [`Self::advance_finger_convergence`].
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn begin_finger_revalidation(&self) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::BeginFingerRevalidation)?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Advance finger convergence by at most one externally visible step.
    ///
    /// The transition may emit one lookup owned by a fresh UUID, apply one
    /// local proof, wait for a deadline, or report no work; it never fans out
    /// several lookups per call.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn advance_finger_convergence(&self) -> Result<PeerRingAction> {
        let next =
            self.transition_topology_at(|now_ms| TopologyEvent::AdvanceFingerConvergence {
                now_ms,
                request_id: new_uuid(),
            })?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Apply a finger report whose successor is already connected.
    ///
    /// `request` must still own the active lookup. A valid range proof updates
    /// every still-current slot it covers; stale, expired, or geometrically
    /// invalid evidence is reported in the outcome and changes nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn apply_fixed_finger(
        &self,
        request: FingerFixRequest,
        successor: Did,
    ) -> Result<FingerApplyOutcome> {
        self.transition(|state, now_ms| topology::apply_finger(state, request, successor, now_ms))
            .map(|(_, outcome)| outcome)
    }

    /// Park a valid finger proof under an admission lease while transport
    /// connects its successor.
    ///
    /// The timely report closes the lookup now instead of writing an
    /// unconnected DID into the table; the outcome says whether the proof was
    /// deferred or rejected as stale, expired, or geometrically invalid.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn defer_fixed_finger(
        &self,
        request: FingerFixRequest,
        successor: Did,
    ) -> Result<FingerDeferOutcome> {
        self.transition(|state, now_ms| topology::defer_finger(state, request, successor, now_ms))
            .map(|(_, outcome)| outcome)
    }

    /// Release a parked finger proof whose admission cannot complete.
    ///
    /// Only the matching `request` and `successor` may release the lease. A
    /// matching retirement advances retry accounting; a stale call leaves a
    /// newer proof untouched and says so in the outcome.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn retire_finger_candidate(
        &self,
        request: FingerFixRequest,
        successor: Did,
    ) -> Result<FingerRetireOutcome> {
        self.transition(|state, now_ms| {
            topology::retire_finger_candidate(state, request, successor, now_ms)
        })
        .map(|(_, outcome)| outcome)
    }

    /// Cancel an outstanding finger lookup and charge it as one failed attempt.
    ///
    /// Retry backoff starts from the clock reading of this transition. A stale
    /// token can neither cancel nor penalize the lookup that replaced it.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn cancel_finger_lookup(&self, request: FingerFixRequest) -> Result<()> {
        self.transition_topology_at(|now_ms| TopologyEvent::CancelFinger { request, now_ms })
            .map(|_| ())
    }
}

/// The exclusive right to spend one claimed stabilization report's candidate
/// budget and commit it.
///
/// Dropping the claim releases the token, on every exit path of the handler
/// holding it, so a failed or cancelled handler cannot leave the report in
/// `Processing` and block the head's next round (which
/// [`PeerRing::begin_stabilization`] skips while a report is being processed).
/// Release is idempotent: the pure model clears only a token with this exact
/// identity, so a claim whose report was already applied releases nothing.
#[must_use = "dropping the claim releases the report"]
pub(crate) struct StabilizationClaim<'ring> {
    ring: &'ring PeerRing,
    request_id: uuid::Uuid,
}

impl StabilizationClaim<'_> {
    /// The token this claim owns.
    pub(crate) const fn request_id(&self) -> uuid::Uuid {
        self.request_id
    }
}

impl Drop for StabilizationClaim<'_> {
    fn drop(&mut self) {
        // A poisoned lock is the only possible failure, and a destructor has
        // no caller to report it to.
        let _ = self.ring.cancel_stabilization(self.request_id);
    }
}

/// The exclusive right to spend one claimed successor-sync report's
/// candidate budget.
///
/// Dropping the claim releases the token, on every exit path of the handler
/// holding it; see [`StabilizationClaim`] for why that matters.
#[must_use = "dropping the claim releases the report"]
pub(crate) struct SuccessorSyncClaim<'ring> {
    ring: &'ring PeerRing,
    reporter: Did,
    request_id: uuid::Uuid,
}

impl Drop for SuccessorSyncClaim<'_> {
    fn drop(&mut self) {
        let _ = self
            .ring
            .cancel_successor_sync(self.reporter, self.request_id);
    }
}

/// Stabilization rounds.
///
/// A round is correlated by a fresh UUID recorded against the successor head
/// it was sent to; only an authenticated report from that head carrying that
/// UUID may change topology. In order of occurrence:
///
/// 1. [`Self::begin_stabilization`] records `(head, request_id)` as
///    `Requested` and returns the query action. While the head's previous
///    report is still being processed it records nothing and emits nothing.
/// 2. [`Self::claim_stabilization_report`] moves the token to `Processing`
///    exactly once, so duplicate deliveries of one report cannot both spend
///    its connection budget, and hands the caller a [`StabilizationClaim`]
///    that releases the token when dropped.
/// 3. [`Self::advance_stabilization_connection_plan`] hands out at most one
///    candidate per call, re-checking the token each time; churn revokes the
///    remaining budget.
/// 4. [`Self::stabilize_reported_by`] applies the report and returns the
///    follow-up actions; [`Self::cancel_stabilization`] releases the token.
///
/// The pure model clears the token whenever the head changes, so a report
/// answered by a superseded head is stale by construction.
impl PeerRing {
    /// Start one stabilization round against the current head.
    ///
    /// With a head, the transition records `(head, request_id)` and returns
    /// the query action that must carry the same token. Without one, it
    /// records nothing and returns no remote work.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn begin_stabilization(&self, request_id: uuid::Uuid) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::BeginStabilize { request_id })?;
        Ok(self.topology_leaf_actions(next.actions))
    }

    /// Claim a stabilization report before any of its candidate work starts.
    ///
    /// The claim predicate and the claiming step see the same snapshot, so two
    /// handlers racing on one report cannot both succeed: the first commit
    /// moves the token to `Processing`, and the second predicate evaluates
    /// against that state. Returns the claim when this caller won it.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn claim_stabilization_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<Option<StabilizationClaim<'_>>> {
        self.transition(|state, _| {
            let claimable = state.can_claim_stabilization_report(reporter, request_id);
            let step = self.step(state, TopologyEvent::ClaimStabilize {
                reporter,
                request_id,
            });
            (step, claimable)
        })
        // Lazily: constructing a guard for a failed claim would drop it at
        // once and release a token this caller never owned.
        .map(|(_, claimed)| {
            claimed.then(|| StabilizationClaim {
                ring: self,
                request_id,
            })
        })
    }

    /// Reserve the next stabilization candidate against the current topology.
    ///
    /// Each call revalidates the plan's reporter and token before advancing
    /// its cursor: `Connect` for one authorized candidate, `Complete` when
    /// exhausted, `Stale` (with no further effect) after churn.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn advance_stabilization_connection_plan(
        &self,
        plan: &mut topology::ConnectionPlan,
    ) -> Result<topology::ConnectionStep> {
        self.with_topology_state(|state| {
            plan.advance(|reporter, request_id| {
                state.is_processing_stabilization_report(reporter, request_id)
            })
        })
    }

    /// Apply a topology report from the owner of the matching stabilization token.
    ///
    /// The pure model rechecks `reporter` and `request_id`, merges the reported
    /// successor and predecessor evidence, and returns every follow-up action
    /// as a batch. A stale token yields no state change and no work.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn stabilize_reported_by(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
        info: TopoInfo,
    ) -> Result<PeerRingAction> {
        let next = self.transition_topology(TopologyEvent::Stabilize {
            reporter,
            request_id,
            successors: info.successors,
            predecessor: info.predecessor,
        })?;
        Ok(self.topology_multi_actions(next.actions))
    }

    /// Release the stabilization token `request_id`, and only that one.
    ///
    /// The pure model compares tokens before clearing, so an old cancellation
    /// cannot retire a newer round.
    ///
    /// # Errors
    ///
    /// Returns an error when topology state cannot be read or committed.
    pub(crate) fn cancel_stabilization(&self, request_id: uuid::Uuid) -> Result<()> {
        self.transition_topology(TopologyEvent::CancelStabilize { request_id })
            .map(|_| ())
    }
}

/// Successor-list synchronization.
///
/// Each successor may have one outstanding successor-list query, owned by a
/// `(reporter, request_id)` token in [`topology::SuccessorSyncState`]. A
/// token is valid only while its reporter is still a successor: the
/// transition core prunes departed reporters' tokens when a commit changes
/// the successor list, and each operation here reads the list under the same
/// lock it uses to touch the tokens.
impl PeerRing {
    /// Operate on the sync tokens and the successor list they are judged
    /// against, from one committed topology.
    fn with_successor_sync<T>(
        &self,
        operate: impl FnOnce(&mut topology::SuccessorSyncState, &[Did]) -> T,
    ) -> Result<T> {
        let _transition = self.lock_transition()?;
        let successors = self.successor_seq.list()?;
        let mut pending = self.lock_pending_successor_sync()?;
        Ok(operate(&mut pending, &successors))
    }

    /// Register one sync token for a current successor.
    ///
    /// Succeeds only while `reporter` is a successor and its previous report
    /// is not still being processed, replacing an unanswered older token;
    /// failure creates no report authority.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn begin_successor_sync(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<bool> {
        self.with_successor_sync(|pending, successors| {
            pending.begin(successors, reporter, request_id)
        })
    }

    /// Claim one matching sync report before it can create effects.
    ///
    /// Succeeds once for the exact `(reporter, request_id)` pair while the
    /// reporter is still a successor, handing the caller a
    /// [`SuccessorSyncClaim`] that releases the token when dropped. Duplicate,
    /// replaced, and departed-reporter reports return `None` and acquire no
    /// connection budget.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn claim_successor_sync_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<Option<SuccessorSyncClaim<'_>>> {
        // Lazily: a guard built for a failed claim would drop inside the lock
        // and its release would deadlock on it.
        self.with_successor_sync(|pending, successors| {
            pending
                .claim(successors, reporter, request_id)
                .then(|| SuccessorSyncClaim {
                    ring: self,
                    reporter,
                    request_id,
                })
        })
    }

    /// Reserve one sync candidate after revalidating live ownership.
    ///
    /// The cursor advances only for a still-processing token whose reporter
    /// is still a successor: `Connect` permits one connection, `Complete`
    /// reports exhaustion, `Stale` revokes the remaining work.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn advance_successor_sync_connection_plan(
        &self,
        plan: &mut topology::ConnectionPlan,
    ) -> Result<topology::ConnectionStep> {
        self.with_successor_sync(|pending, successors| {
            plan.advance(|reporter, request_id| {
                pending.is_processing(successors, reporter, request_id)
            })
        })
    }

    /// Release the exact sync token `(reporter, request_id)`.
    ///
    /// Cancellation does not consult the successor list: a token must remain
    /// releasable after its reporter has churned out, and a mismatched token
    /// is left untouched, so a delayed send or join failure can race a newer
    /// round safely.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn cancel_successor_sync(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> Result<()> {
        let _transition = self.lock_transition()?;
        self.lock_pending_successor_sync()?
            .cancel(reporter, request_id);
        Ok(())
    }
}

/// Test hooks that read or seed backing state directly, or hold a
/// transition open to observe what waits behind it.
#[cfg(test)]
impl PeerRing {
    /// Apply `event` after running `hold` on the snapshot inside the lock.
    ///
    /// A test blocks in `hold` to keep the transition open while it checks
    /// that concurrent transitions and views wait for the commit.
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    pub(crate) fn transition_topology_holding(
        &self,
        event: TopologyEvent,
        hold: impl FnOnce(&TopologyState),
    ) -> Result<TopologyStep> {
        self.transition(|state, _| {
            hold(state);
            (self.step(state, event), ())
        })
        .map(|(step, ())| step)
    }

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    pub(crate) fn lock_finger(&self) -> Result<MutexGuard<'_, FingerTable>> {
        self.lock_finger_state()
    }

    pub(crate) fn lock_predecessor(&self) -> Result<MutexGuard<'_, Option<Did>>> {
        self.lock_predecessor_state()
    }

    /// Seed the finger table with fixture hints, bypassing the pure transition.
    pub(crate) fn replace_fingers_for_test(&self, fingers: &[(usize, Did)]) -> Result<()> {
        let _transition = self.lock_transition()?;
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
}

impl Chord<PeerRingAction> for PeerRing {
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

    /// One repair pass: begin revalidation, then emit the first due lookup.
    fn fix_fingers(&self) -> Result<PeerRingAction> {
        self.begin_finger_revalidation()?;
        self.advance_finger_convergence()
    }
}

/// The first half of the HMCC/Zave stabilize operation.
///
/// The second half, applying the successor's report, enters only through the
/// token-checked report path: a report changes topology exactly when it echoes
/// the correlation token that this call issued.
impl PeerRing {
    /// Query the successor head for its predecessor and successor list.
    ///
    /// The returned action carries the correlation token the head's report
    /// must echo.
    pub fn pre_stabilize(&self) -> Result<PeerRingAction> {
        self.begin_stabilization(new_uuid())
    }
}
