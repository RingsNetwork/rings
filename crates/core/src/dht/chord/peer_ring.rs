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
//! commit is visible to the next snapshot. Read-only views (the `views`
//! module) assemble their snapshot under the same lock for the same reason.
//!
//! A commit that changes the routes (`TopologyState::routes_equal` is false) advances the
//! topology epoch, the event a data-plane operation deferred on a stale hop waits for:
//!
//! ```text
//! Law (Epoch) : commit(s, s') ∧ ¬RoutesEqual(s, s') ⇒ epoch' = epoch + 1
//!               commit(s, s') ∧  RoutesEqual(s, s') ⇒ epoch' = epoch
//! ```
//!
//! Successor-list synchronization tokens are the one piece of state kept
//! beside the model rather than inside it. A token is valid only while its
//! reporter is a successor, so the transition core prunes the tokens of
//! departed reporters whenever a commit changes the successor list.
//!
//! The `impl` blocks below are grouped by concern: construction, the
//! transition core, membership, finger convergence, stabilization
//! rounds, successor-list synchronization, and test hooks.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use futures::lock::Mutex as FuturesMutex;

use super::carrier_store::CarrierStore;
use super::carrier_store::RemovalIndex;
use super::carrier_store::RemovalStorage;
use super::carrier_store::REMOVAL_SHARES;
use super::PeerRingAction;
use super::RemoteAction;
use crate::consts::LOCAL_CACHE_CAPACITY;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryVersion;
use crate::dht::finger::FingerApplyOutcome;
use crate::dht::finger::FingerConvergenceStatus;
use crate::dht::finger::FingerDeferOutcome;
use crate::dht::finger::FingerRetireOutcome;
use crate::dht::finger::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::relay_holder::RelayNeighborhood;
use crate::dht::relay_holder::RelayReplication;
use crate::dht::successor::SuccessorSeq;
use crate::dht::topology;
use crate::dht::topology::edge_slots;
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
use crate::fair_admission::SlotFloorUse;
use crate::fair_admission::SlotRoster;
use crate::fair_admission::TableAccounting;
use crate::lifecycle::epoch::Epoch;
use crate::message::RelayHoldDigest;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::utils::new_uuid;
use crate::utils::Instant;

mod relay_custody;
use relay_custody::KeptRelayHold;
use relay_custody::RelayHolderHints;
mod relay_repair;
mod relay_replicas;
use relay_replicas::NextHolder;
use relay_replicas::PendingReplication;
pub(crate) use relay_replicas::ReplicaAcknowledgement;
pub(crate) use relay_replicas::ReplicationStep;

/// The holds a holder replicates to the other members of their keys (see `relay_replicas`).
type RelayReplications = BTreeMap<RelayHoldDigest, PendingReplication>;
mod views;

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
    /// Persistent replicated-entry storage of the data namespace: the element sides of the
    /// overlay's replicated topics, rebuildable from their replicas, which its budget may evict
    /// (see `chord::carrier_store`).
    pub(crate) storage: EntryStorage,
    /// The remove sides of the data namespace (#920): a `RecordAuthority::Retained` store, which
    /// refuses and never evicts, given by [`Self::with_removal_storage`]; an in-memory one until
    /// then, whose loss at a restart makes this node a fresh replica (removal-stability paper,
    /// Section IV).
    pub(crate) removal_storage: RemovalStorage,
    /// The charge index of [`Self::removal_storage`](PeerRing#structfield.removal_storage).
    removal_index: RemovalIndex,
    /// The connection slots of this node, which every table under slot accounting charges
    /// (load-bounds paper, Law 1); the transport gives an edge a slot as it admits the edge and
    /// frees it as it retires the edge.
    edges: Arc<SlotRoster>,
    /// Storage of the relay-inbox namespace: this node's copy of the mail it holds as a member of
    /// a carrier's key (a relay carrier is replicated to the members alone, #777, never cached,
    /// and fetched by its recipient alone), so it must be a `RecordAuthority::Retained` store
    /// (#1021). `None` until [`Self::with_relay_storage`]
    /// gives it one; without it the node takes custody of no relay carrier (Law 1 admits no
    /// volatile relay store): it declares that it holds no relay mail, so the holder law
    /// (`dht::relay_holder`) never names it, every relay write that reaches it anyway is refused
    /// with `Error::RelayStorageUnavailable`, and every relay read finds nothing.
    pub(crate) relay_storage: Option<EntryStorage>,
    /// Local fetched-entry cache, bounded at [`LOCAL_CACHE_CAPACITY`] entries: each carrier is
    /// the join of the replies observed for its key (#864).
    pub(crate) cache: EntryStorage,
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
    /// Serializes every read-modify-write of a fetch-cache slot (see `chord::storage`).
    pub(super) cache_transition: FuturesMutex<()>,
    /// Count of committed transitions that changed the routes (`Law (Epoch)`).
    topology_epoch: Epoch,
    /// The relay custody this node knows of its neighbours (see `relay_custody`).
    relay_neighborhood: Mutex<RelayNeighborhood>,
    /// The relay holds this node wrote and no holder acknowledged yet (see `relay_custody`).
    unacknowledged_relay_holds: Mutex<BTreeMap<RelayHoldDigest, KeptRelayHold>>,
    /// The greatest version this node has stamped or forwarded through
    /// [`Self::stamp_operation`]: the floor of its next stamp, so its own operations are
    /// totally ordered in issue order (#1075).
    pub(super) last_stamped: Mutex<Option<EntryVersion>>,
    /// The queries asked of, and the holders named by, the owners of the relay keys kept here
    /// that this view does not place (see `relay_custody`).
    relay_holder_hints: Mutex<RelayHolderHints>,
    /// The holds this node took as their key's holder and replicates (see `relay_replicas`).
    relay_replications: Mutex<RelayReplications>,
    /// How relay custody is replicated: `r` and the age gate (see `relay_replicas`).
    relay_replication: RelayReplication,
    /// The peers this node declared to that it holds relay mail, once its age gate opened.
    relay_custody_declared: Mutex<BTreeSet<Did>>,
    /// The first holder after this node past its successor list (see `relay_replicas`).
    relay_next_holder: Mutex<NextHolder>,
    /// Whether a chain neighbour was lost and the chain exchange has not run since (`Law
    /// (Event-driven repair)`); the predicate the chain repair task guards on.
    relay_chain_repair_due: AtomicBool,
    /// Count of chain-neighbour losses, which wakes the chain repair task (`Law (Wake)`).
    relay_chain_losses: Epoch,
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
        let edges = Arc::new(SlotRoster::new(edge_slots(usize::from(succ_max))));
        Self {
            successor_seq: SuccessorSeq::new(did, succ_max),
            predecessor: Arc::new(Mutex::new(None)),
            finger: Arc::new(Mutex::new(FingerTable::new(did, finger_table_size))),
            storage,
            removal_storage: Box::new(MemStorage::new()),
            removal_index: RemovalIndex::new(TableAccounting::new(REMOVAL_SHARES)),
            edges,
            relay_storage: None,
            cache: Box::new(MemStorage::bounded(LOCAL_CACHE_CAPACITY)),
            storage_virtual_node_config: virtual_nodes,
            topology_transition: Mutex::new(()),
            pending_stabilization: Mutex::new(None),
            pending_successor_sync: Mutex::new(topology::SuccessorSyncState::default()),
            clock_origin: Instant::now(),
            finger_jitter_entropy: OnceLock::new(),
            storage_transition: FuturesMutex::new(()),
            cache_transition: FuturesMutex::new(()),
            topology_epoch: Epoch::default(),
            relay_neighborhood: Mutex::new(RelayNeighborhood::default()),
            unacknowledged_relay_holds: Mutex::new(BTreeMap::new()),
            last_stamped: Mutex::new(None),
            relay_holder_hints: Mutex::new(RelayHolderHints::default()),
            relay_replications: Mutex::new(BTreeMap::new()),
            relay_replication: RelayReplication::default(),
            relay_custody_declared: Mutex::new(BTreeSet::new()),
            relay_next_holder: Mutex::new(NextHolder::default()),
            relay_chain_repair_due: AtomicBool::new(false),
            relay_chain_losses: Epoch::default(),
            did,
        }
    }

    /// This ring with `storage` as the store of its relay-inbox namespace (see
    /// [`Self::relay_storage`](PeerRing#structfield.relay_storage)): a retained store, or, in a
    /// test that chooses it by name, an in-memory one.
    pub fn with_relay_storage(mut self, storage: EntryStorage) -> Self {
        self.relay_storage = Some(storage);
        self
    }

    /// This ring with `storage` as the store of the data namespace's remove sides (see
    /// [`Self::removal_storage`](PeerRing#structfield.removal_storage)): a retained store, which
    /// refuses and never evicts (#920).
    pub fn with_removal_storage(mut self, storage: RemovalStorage) -> Self {
        self.removal_storage = storage;
        self
    }

    /// Whether this node's view names `node` as a previous owner of the placements it now owns:
    /// its predecessor or one of its successors (load-bounds paper, Sec. V, the custody
    /// principal `χ`).
    pub(crate) fn names_previous_owner(&self, node: Did) -> Result<bool> {
        Ok(*self.lock_predecessor_state()? == Some(node) || self.successor_seq.contains(&node)?)
    }

    /// The connection slots of this node (see [`Self::edges`](PeerRing#structfield.edges)).
    pub(crate) fn edges(&self) -> &SlotRoster {
        &self.edges
    }

    /// The floor use of every slot in the removal store at `now_ms` (load-bounds paper, Def. 1).
    pub(crate) fn removal_floor_use_at(&self, now_ms: u128) -> SlotFloorUse {
        self.removal_index.floor_use_at(now_ms)
    }

    /// This ring replicating relay custody as `replication` says (#777).
    pub fn with_relay_replication(mut self, replication: RelayReplication) -> Self {
        let replication = RelayReplication {
            replicas: replication.replicas.max(1),
            ..replication
        };
        self.relay_replication = replication;
        self.relay_neighborhood =
            Mutex::new(RelayNeighborhood::with_replicas(replication.replicas));
        self
    }

    /// The store of the carriers of `kind`, if this node keeps one: data topics and relay
    /// inboxes live in separate stores of their own authority, as they live in separate
    /// namespaces, and a node with no relay store keeps no relay carrier. The data store is
    /// factored into element and remove sides.
    pub(crate) fn carrier_storage(&self, kind: EntryKind) -> Option<CarrierStore<'_>> {
        match kind {
            EntryKind::Data => Some(CarrierStore::Factored {
                elements: &self.storage,
                removals: &self.removal_storage,
                index: &self.removal_index,
            }),
            EntryKind::RelayMessage => self.relay_storage.as_ref().map(CarrierStore::Whole),
        }
    }

    /// The store a write of a carrier of `kind` goes to (the custody law of #1021).
    ///
    /// Post: `Err(RelayStorageUnavailable)` for a relay carrier on a node with no relay store.
    pub(crate) fn carrier_store_for_write(&self, kind: EntryKind) -> Result<CarrierStore<'_>> {
        self.carrier_storage(kind)
            .ok_or(Error::RelayStorageUnavailable)
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
            self.forget_next_relay_holder(None)?;
        }
        if !next.state.routes_equal(&current) {
            self.topology_epoch.advance();
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
        // A chain neighbour vacates a slot of relay custody even when the topology does not
        // reference it, so its loss makes the repair round due at once (`Law (Event-driven
        // repair)`, #777).
        let (previous, next) = self.relay_chain_neighbours()?;
        let chain_neighbour = previous == Some(did) || next == Some(did);
        let (_, removal) = self.transition(|state, _| {
            let removal = TopologyRemoval::of(state, did);
            let event = TopologyEvent::Remove {
                peer: did,
                successor,
            };
            (self.step(state, event), removal)
        })?;
        self.forget_relay_custody(did)?;
        if chain_neighbour {
            self.relay_chain_repair_due
                .store(true, std::sync::atomic::Ordering::Release);
            self.relay_chain_losses.advance();
        }
        Ok(if chain_neighbour {
            TopologyRemoval::Referenced
        } else {
            removal
        })
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

/// The stabilization and successor-sync report rounds: their claims and transitions.
mod report_rounds;
