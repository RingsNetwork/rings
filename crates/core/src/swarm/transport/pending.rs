use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use rings_transport::core::transport::TransportInterface;
/// Finger-table proof admission at the pending/active transport boundary.
///
/// The module-level algorithm and its exhaustive lifecycle branches are
/// documented in `pending::finger`.
mod finger;
mod registry;

use finger::finger_candidate_admission;
use finger::FingerCandidateAdmission;
pub(crate) use finger::FingerUpdateDisposition;
pub(super) use registry::ActiveConnectionSet;
pub(super) use registry::ConnectionLifecycleRegistry;
pub(super) use registry::LifecycleBounds;
use registry::PeerConnectionLifecycle;
#[cfg(all(test, not(target_family = "wasm")))]
pub(super) use registry::ReservationVerdict;
pub(super) use registry::Retirement;
pub(super) use registry::RetirementOutcome;

use super::SwarmConnection;
use super::SwarmTransport;
use crate::dht::finger::FingerDeferOutcome;
use crate::dht::finger::FingerRetireOutcome;
use crate::dht::Did;
use crate::dht::FingerFixRequest;
use crate::dht::PeerRingAction;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::callback::InnerSwarmCallback;
use crate::utils::get_epoch_ms_i64;

/// Maximum number of peers that may be handshaking before a data channel opens.
pub(crate) const DEFAULT_PENDING_CONNECTION_CAPACITY: usize = 32;

/// Maximum lifetime of a pending or admitting connection generation.
pub(super) const PENDING_CONNECTION_TIMEOUT_MS: i64 = 180_000;
// A deferred finger proof and the handshake generation that can claim it run
// on different clocks (the ring's monotonic origin versus wall-clock epoch
// milliseconds) and start at different instants (report arrival versus
// reservation). The generation owns the proof once it is attached, and its
// expiry cancels the proof explicitly, so the DHT lease only has to outlast
// the generation: a lease that expired first would charge a failure and
// discard a proof whose handshake could still succeed. Nothing can commit a
// proof the DHT has already released, because commit re-validates token
// ownership.
const _: () =
    assert!(crate::dht::finger::FINGER_ADMISSION_TIMEOUT_MS > PENDING_CONNECTION_TIMEOUT_MS as u64);

/// Shared registry of per-peer pending, admitting, and active connection generations.
pub(super) type SharedConnectionLifecycles = Arc<Mutex<ConnectionLifecycleRegistry>>;
/// The finger proof each pending generation retains until it commits or ends.
pub(super) type PendingFingerUpdates = BTreeMap<PendingConnectionAttempt, FingerFixRequest>;
/// Guard for the deferred finger-proof map.
type PendingFingerUpdatesGuard<'transport> =
    std::sync::MutexGuard<'transport, PendingFingerUpdates>;

/// Shared serialization boundary for logical connection ownership.
///
/// Clone law: every clone refers to the same mutex. Holding the boundary
/// prevents admission, retirement, and final send admission from crossing.
#[derive(Clone)]
pub(super) struct ConnectionLifecycleBoundary {
    /// Mutex that serializes admission, retirement, and final send checks.
    inner: Arc<Mutex<()>>,
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    /// Test-only count of threads waiting to acquire the lifecycle gate.
    waiting: Arc<std::sync::atomic::AtomicUsize>,
}

impl ConnectionLifecycleBoundary {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(())),
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            waiting: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub(super) fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.inner
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(super) fn is_held_for_test(&self) -> bool {
        self.inner.try_lock().is_err()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn lock_with_waiter_observer_for_test(
        &self,
        waiter_registered: impl FnOnce(),
    ) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.waiting
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        waiter_registered();
        let result = self.lock();
        self.waiting
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        result
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn waiting_for_test(&self) -> usize {
        self.waiting.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Identifies one pending handshake for a peer.
///
/// A peer can have a replacement handshake after a timeout. Callbacks carry
/// this token so a late callback from the replaced connection cannot promote
/// the newer handshake into the active routing set.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(test, derive(Hash))]
pub(crate) struct PendingConnectionAttempt {
    /// Peer this logical connection generation is trying to own.
    pub(super) peer: Did,
    /// Monotonic per-peer generation used to reject stale callbacks.
    pub(super) generation: u64,
}

impl PendingConnectionAttempt {
    pub(crate) fn peer(self) -> Did {
        self.peer
    }

    /// Whether this generation is a handshake with `peer`.
    pub(crate) fn is_with(self, peer: Did) -> bool {
        self.peer == peer
    }

    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionEventDisposition {
    /// Deliver the callback because it still belongs to the current generation or no owner exists.
    Deliver,
    /// Drop the callback because another active generation already owns the peer.
    Suppress {
        /// Current active generation that superseded the callback source.
        active: PendingConnectionAttempt,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RawConnectionOwner {
    /// Raw transport belongs to a still-unadmitted logical attempt.
    Pending(PendingConnectionAttempt),
    /// Raw transport is owned by an admitting or active logical attempt.
    Owned,
    /// Raw transport exists after its logical lifecycle slot was removed.
    Orphan,
}

fn event_disposition(
    state: Option<PeerConnectionLifecycle>,
    source: PendingConnectionAttempt,
) -> ConnectionEventDisposition {
    match state {
        Some(PeerConnectionLifecycle::Active {
            attempt: active, ..
        }) if active != source => ConnectionEventDisposition::Suppress { active },
        Some(PeerConnectionLifecycle::Pending { .. })
        | Some(PeerConnectionLifecycle::Admitting { .. })
        | Some(PeerConnectionLifecycle::Active { .. })
        | None => ConnectionEventDisposition::Deliver,
    }
}

/// The retired value of an outcome, its announcement witness projected away; for tests that
/// assert the retirement itself.
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
fn retired_value<T>(outcome: RetirementOutcome<(T, Retirement)>) -> Option<T> {
    outcome.retired().map(|(value, _witness)| value)
}

/// A peer's slot as an answer to this node's offer finds it.
pub(super) enum AnswerSlot {
    /// No record.
    Vacant,
    /// A pending generation with the transport object the answer applies to.
    Pending(PendingConnectionAttempt, SwarmConnection),
    /// A generation an answer cannot apply to: past pending, or pending without a transport
    /// object.
    Owned(PendingConnectionAttempt),
}

/// Which unadmitted phases a cancellation may release, in the registry's vocabulary.
enum CancellationScope {
    /// `Pending` only: no data channel has opened, so only the record and its transport object
    /// are released.
    Pending,
    /// `Pending` or `Admitting`.
    Unadmitted,
}

struct RetiredPendingConnection {
    /// Raw transport released by retirement, if it still exists in the backend.
    connection: Option<SwarmConnection>,
}

/// Transport object paired with the pending generation that owns its callbacks.
pub(super) struct PendingTransportConnection {
    /// Logical generation created before the backend transport object.
    attempt: PendingConnectionAttempt,
    /// Raw transport connection created for `attempt`.
    connection: SwarmConnection,
}

impl PendingTransportConnection {
    pub(super) fn attempt(&self) -> PendingConnectionAttempt {
        self.attempt
    }

    pub(super) fn connection(&self) -> &SwarmConnection {
        &self.connection
    }

    #[cfg(all(
        test,
        feature = "dummy",
        not(all(feature = "wasm", target_family = "wasm"))
    ))]
    pub(super) fn into_connection(self) -> SwarmConnection {
        self.connection
    }
}

impl SwarmTransport {
    fn connection_lifecycle(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.connection_lifecycle.lock()
    }

    pub(super) fn with_connection_lifecycle<T>(
        &self,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let _lifecycle = self.connection_lifecycle()?;
        action()
    }

    pub(super) fn with_active_slot<T>(
        &self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        self.with_connection_lifecycle(|| {
            if !self.owns_active_slot(attempt)? {
                return Ok(None);
            }
            action().map(Some)
        })
    }

    /// Lock the full connection lifecycle registry for compound lifecycle decisions.
    pub(super) fn peer_lifecycles(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ConnectionLifecycleRegistry>> {
        self.peer_lifecycles
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    pub(super) fn active_connections(&self) -> Result<ActiveConnectionSet> {
        Ok(self.peer_lifecycles()?.active_connections())
    }

    fn pending_finger_updates(&self) -> Result<PendingFingerUpdatesGuard<'_>> {
        self.pending_finger_updates
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    /// Cancel every deferred finger proof owned by one connection generation.
    ///
    /// The function first removes the generation's complete request set from
    /// the transport side table, then tells the DHT to retire each correlated
    /// lookup. After success, no deferred proof remains owned by `attempt`.
    /// Lock poisoning or a failed DHT transition is returned to the caller.
    fn cancel_pending_finger_updates(&self, attempt: PendingConnectionAttempt) -> Result<()> {
        let retained = self.pending_finger_updates()?.remove(&attempt);
        if let Some(request) = retained {
            self.dht.cancel_finger_lookup(request)?;
        }
        Ok(())
    }

    pub(super) fn get_raw_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.transport
            .connection(&peer.to_string())
            .map(|conn| SwarmConnection {
                peer,
                connection: conn,
            })
            .ok()
    }

    pub(super) fn raw_connection_owner(&self, peer: Did) -> Result<RawConnectionOwner> {
        Ok(match self.peer_lifecycles()?.state(peer) {
            Some(PeerConnectionLifecycle::Pending { attempt, .. }) => {
                RawConnectionOwner::Pending(attempt)
            }
            Some(
                PeerConnectionLifecycle::Admitting { .. } | PeerConnectionLifecycle::Active { .. },
            ) => RawConnectionOwner::Owned,
            None => RawConnectionOwner::Orphan,
        })
    }

    /// Return whether `peer` completed a handshake and still owns a logical slot.
    ///
    /// This remains true while a terminal callback removes the peer, so
    /// lifecycle cleanup can evict it from the DHT even after WebRTC reports
    /// `Closed`.
    pub(crate) fn has_active_connection(&self, peer: Did) -> bool {
        self.peer_lifecycles()
            .map(|lifecycles| lifecycles.active_attempt(peer).is_some())
            .unwrap_or(false)
    }

    /// Return whether `attempt` owns the current active slot for its peer.
    ///
    /// Invariant: a terminal callback may remove an active peer only when its
    /// generation equals the generation admitted by data-channel open.
    pub(crate) fn is_active_connection_attempt(&self, attempt: PendingConnectionAttempt) -> bool {
        self.owns_active_slot(attempt).unwrap_or(false)
    }

    /// Return whether `attempt` is the unique owner of its peer's active slot.
    pub(super) fn owns_active_slot(&self, attempt: PendingConnectionAttempt) -> Result<bool> {
        self.peer_lifecycles()
            .map(|lifecycles| lifecycles.active_attempt(attempt.peer) == Some(attempt))
    }

    pub(super) async fn reserve_pending_connection(
        &self,
        peer: Did,
    ) -> Result<PendingConnectionAttempt> {
        self.prepare_pending_reservation(peer).await?;
        self.commit_pending_reservation(peer)
    }

    /// Validate the remote-peer precondition and free a slot before commit.
    ///
    /// Stale handshakes expire first; when the registry is still full for a
    /// peer without a record, the oldest unreferenced admitted connection is
    /// evicted so the bound recycles displaced connections instead of refusing
    /// every newcomer.
    ///
    /// Separation law: validation is pure; expiry, eviction, and commit are separate lifecycle
    /// mutations, each serialized by the shared boundary without holding a synchronous lock
    /// across await.
    async fn prepare_pending_reservation(&self, peer: Did) -> Result<()> {
        self.validate_pending_reservation(peer)?;
        self.expire_pending_connections().await?;
        if self.reservation_needs_eviction(peer)? {
            self.evict_unreferenced_connection(get_epoch_ms_i64())
                .await?;
        }
        Ok(())
    }

    /// Pure precondition for reserving a remote peer.
    fn validate_pending_reservation(&self, peer: Did) -> Result<()> {
        if peer == self.dht.did {
            return Err(Error::ShouldNotConnectSelf);
        }
        Ok(())
    }

    /// Commit one previously prepared reservation under the lifecycle boundary.
    fn commit_pending_reservation(&self, peer: Did) -> Result<PendingConnectionAttempt> {
        let _lifecycle = self.connection_lifecycle()?;
        let attempt = self.peer_lifecycles()?.reserve(peer, get_epoch_ms_i64())?;
        tracing::debug!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            pending_timeout_ms = PENDING_CONNECTION_TIMEOUT_MS,
            "pending connection reserved"
        );
        Ok(attempt)
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) async fn reserve_pending_connection_with_observer_for_test(
        &self,
        peer: Did,
        observe_after_prepare: impl FnOnce(),
        observe_before_commit: impl FnOnce(),
    ) -> Result<PendingConnectionAttempt> {
        self.prepare_pending_reservation(peer).await?;
        observe_after_prepare();
        observe_before_commit();
        self.commit_pending_reservation(peer)
    }

    #[cfg(all(test, feature = "dummy"))]
    pub(crate) fn pending_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        Ok(self.peer_lifecycles()?.pending_attempt(peer))
    }

    pub(crate) fn unadmitted_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        Ok(self.peer_lifecycles()?.unadmitted_attempt(peer))
    }

    pub(crate) fn active_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        Ok(self.peer_lifecycles()?.active_attempt(peer))
    }

    /// Whether any connection attempt toward `peer` exists, admitted or still handshaking.
    ///
    /// Post: `false` means `peer` is unreachable from this node right now and nothing is under
    /// way to change that; a message for it cannot be handed to a connection.
    pub(crate) fn has_connection_attempt(&self, peer: Did) -> Result<bool> {
        Ok(self.peer_lifecycles()?.contains(peer))
    }

    /// The slot of `peer` as an answer finds it, read once under the lifecycle boundary
    /// together with the transport object an answer applies to.
    pub(super) fn answer_slot(&self, peer: Did) -> Result<AnswerSlot> {
        self.with_connection_lifecycle(|| {
            Ok(match self.peer_lifecycles()?.state(peer) {
                None => AnswerSlot::Vacant,
                Some(PeerConnectionLifecycle::Pending { attempt, .. }) => {
                    match self.get_raw_connection(peer) {
                        Some(connection) => AnswerSlot::Pending(attempt, connection),
                        None => AnswerSlot::Owned(attempt),
                    }
                }
                Some(state) => AnswerSlot::Owned(state.attempt()),
            })
        })
    }

    /// The active generation of `peer` whose admission was announced to the application.
    pub(crate) fn announced_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        Ok(self.peer_lifecycles()?.announced_attempt(peer))
    }

    #[cfg(all(test, feature = "dummy"))]
    pub(crate) fn is_pending_connection_attempt(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        Ok(self.peer_lifecycles()?.pending_attempt(attempt.peer) == Some(attempt))
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn is_admitting_connection_attempt(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        Ok(self.peer_lifecycles()?.admitting_attempt(attempt.peer) == Some(attempt))
    }

    pub(crate) fn is_current_connection_attempt(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        Ok(self
            .peer_lifecycles()?
            .state(attempt.peer)
            .is_some_and(|state| state.attempt() == attempt))
    }

    pub(crate) fn connection_event_disposition(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<ConnectionEventDisposition> {
        Ok(event_disposition(
            self.peer_connection_lifecycle(attempt.peer)?,
            attempt,
        ))
    }

    fn peer_connection_lifecycle(&self, peer: Did) -> Result<Option<PeerConnectionLifecycle>> {
        Ok(self.peer_lifecycles()?.state(peer))
    }

    /// Start admission only after the coherent transport product state can make progress.
    ///
    /// Data-channel and peer-connection callbacks are independent inputs. Browsers may therefore
    /// report `data_channel_open = true` while the peer-connection state still reads `New`. That
    /// observation is transient, not a failed handshake: leave the attempt in `Pending` so the
    /// later peer-connection callback can retry the same transition.
    ///
    /// Pre: `attempt` may identify the current `Pending` generation.
    /// Post: `Pending(attempt) -> Admitting(attempt)` iff the current transport snapshot is ready;
    /// a non-terminal snapshot preserves `Pending(attempt)`, while a terminal snapshot is an error.
    pub(crate) fn begin_ready_connection_admission(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.begin_connection_admission_when(
            attempt,
            |transport| {
                if transport.peer_lifecycles()?.pending_attempt(attempt.peer) != Some(attempt) {
                    return Ok(false);
                }

                let connection = transport
                    .get_raw_connection(attempt.peer)
                    .ok_or(Error::SwarmMissTransport(attempt.peer))?;
                let readiness = connection.readiness();
                if readiness.can_make_progress() {
                    return Ok(true);
                }
                if readiness.is_terminal() {
                    readiness.ensure_can_make_progress()?;
                }
                tracing::debug!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %transport.dht.did,
                    peer = %attempt.peer,
                    generation = attempt.generation,
                    readiness = readiness.as_str(),
                    state = ?readiness.state(),
                    data_channel_open = readiness.data_channel_open(),
                    "connection admission deferred until transport state converges"
                );
                Ok(false)
            },
            |_| {},
        )
    }

    /// Serialize one guarded transition into `Admitting` and its optional test observation hook.
    ///
    /// Post: `false` preserves the lifecycle state; `true` means the matching `Pending` generation
    /// became `Admitting` and the observer ran while the lifecycle gate was still held.
    fn begin_connection_admission_when(
        &self,
        attempt: PendingConnectionAttempt,
        guard: impl FnOnce(&Self) -> Result<bool>,
        observe_transition: impl FnOnce(&Self),
    ) -> Result<bool> {
        let _lifecycle = self.connection_lifecycle()?;
        if !guard(self)? || !self.peer_lifecycles()?.begin_admission(attempt) {
            return Ok(false);
        }
        observe_transition(self);
        tracing::debug!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "pending connection admission started"
        );
        Ok(true)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn begin_connection_admission_for_test(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.begin_connection_admission_with_observer_for_test(attempt, |_| {})
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn begin_connection_admission_with_observer_for_test(
        &self,
        attempt: PendingConnectionAttempt,
        observe_transition: impl FnOnce(&Self),
    ) -> Result<bool> {
        self.begin_connection_admission_when(attempt, |_| Ok(true), observe_transition)
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn activate_connection_for_test(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.activate_connection_with_observer_for_test(attempt, |_| {})
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn activate_connection_with_observer_for_test(
        &self,
        attempt: PendingConnectionAttempt,
        observe_transition: impl FnOnce(&Self),
    ) -> Result<bool> {
        let _lifecycle = self.connection_lifecycle()?;
        if !self.peer_lifecycles()?.activate_for_test(attempt) {
            return Ok(false);
        }
        observe_transition(self);
        Ok(true)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn replace_active_generation_for_test(
        &self,
        peer: Did,
    ) -> Result<(PendingConnectionAttempt, PendingConnectionAttempt)> {
        let _lifecycle = self.connection_lifecycle()?;
        let mut lifecycles = self.peer_lifecycles()?;
        let old = lifecycles
            .active_attempt(peer)
            .ok_or(Error::SwarmMissTransport(peer))?;
        if !lifecycles.remove_active(old) {
            return Err(Error::ConnectionAttemptSuperseded {
                peer,
                generation: old.generation,
            });
        }
        let replacement = lifecycles.reserve(peer, get_epoch_ms_i64())?;
        if !lifecycles.activate_for_test(replacement) {
            return Err(Error::ConnectionAttemptSuperseded {
                peer,
                generation: replacement.generation,
            });
        }
        Ok((old, replacement))
    }

    /// Commit `Admitting(attempt) -> Active(attempt)` together with all local DHT state.
    pub(crate) fn commit_connection_admission(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<Option<PeerRingAction>> {
        let _lifecycle = self.connection_lifecycle()?;
        let mut lifecycles = self.peer_lifecycles()?;
        let Some(admitting) = lifecycles.admitting_connection(attempt) else {
            return Ok(None);
        };
        let connection = self
            .get_raw_connection(attempt.peer)
            .ok_or(Error::SwarmMissTransport(attempt.peer))?;
        connection.readiness().ensure_can_make_progress()?;

        let mut pending_finger_updates = self.pending_finger_updates()?;
        // The retained proof was validated by DHT report handling; it is
        // applied in the same DHT transition that admits the peer.
        let deferred_proof = pending_finger_updates.get(&attempt).copied();
        let action = self.dht.admit_connected(attempt.peer, deferred_proof)?;

        admitting.activate();
        pending_finger_updates.remove(&attempt);
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "connection admission committed"
        );
        Ok(Some(action))
    }

    /// Release the record of `attempt` iff it is still pending, leaving its transport object,
    /// if any, in place: the `Pending` arm of the one cancellation transition without the close.
    pub(super) fn retire_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.retire_pending_connection_for_close(attempt, CancellationScope::Pending)
            .map(|retired| retired.is_some())
    }

    /// Mark the admission of `attempt` as announced to the application, iff `attempt` still
    /// owns the active slot. Called in the delivery turn that then starts `Connected`, and
    /// observed atomically with retirement under the lifecycle boundary; see
    /// `PeerConnectionLifecycle::Active`.
    pub(crate) fn mark_admission_announced(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let _lifecycle = self.connection_lifecycle()?;
        Ok(self.peer_lifecycles()?.mark_announced(attempt))
    }

    /// Retire `attempt` only if `action` decides to, under the lifecycle boundary; see
    /// `ConnectionLifecycleRegistry::retire_active_if` for the post-conditions. The witness in
    /// a `Retired` outcome is the caller's to announce.
    pub(super) fn retire_active_connection_if<T>(
        &self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce(&ActiveConnectionSet) -> Result<Option<T>>,
    ) -> Result<RetirementOutcome<(T, Retirement)>> {
        let _lifecycle = self.connection_lifecycle()?;
        self.retire_active_connection_locked(attempt, action)
    }

    /// The retirement transition under an already held lifecycle boundary: the registry
    /// decides, and the per-peer side tables follow a `Retired` decision.
    fn retire_active_connection_locked<T>(
        &self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce(&ActiveConnectionSet) -> Result<Option<T>>,
    ) -> Result<RetirementOutcome<(T, Retirement)>> {
        let mut lifecycles = self.peer_lifecycles()?;
        // Acquire every fallible local-state guard before the action mutates the DHT, so the
        // side-table mutations after a commit are infallible; if the action fails or declines,
        // all four guards drop without changing local state. The lifecycle lock prevents
        // admission or retirement from changing the active set while the action validates a
        // successor fallback against it.
        let mut pending_finger_updates = self.pending_finger_updates()?;
        let mut peer_liveness = self.peer_liveness()?;
        let mut measured_disconnects = self.measured_disconnects()?;
        let outcome = lifecycles.retire_active_if(attempt, action)?;
        if let RetirementOutcome::Retired(_) = &outcome {
            pending_finger_updates.retain(|pending, _| pending.peer != attempt.peer);
            peer_liveness.remove(attempt.peer);
            measured_disconnects.remove(&attempt.peer);
            self.outbound_schedulers.shutdown(attempt.peer);
        }
        Ok(outcome)
    }

    /// Unconditional retirement for tests that assert the retirement itself; the announcement
    /// witness is projected away.
    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(super) fn retire_active_connection_for_test<T>(
        &self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce(&ActiveConnectionSet) -> Result<T>,
    ) -> Result<Option<T>> {
        self.retire_active_connection_if(attempt, |active| action(active).map(Some))
            .map(retired_value)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn retire_active_connection_with_observer_for_test<T>(
        &self,
        attempt: PendingConnectionAttempt,
        before_lifecycle_gate: impl FnOnce(),
        action: impl FnOnce(&ActiveConnectionSet) -> Result<T>,
    ) -> Result<Option<T>> {
        let _lifecycle = self
            .connection_lifecycle
            .lock_with_waiter_observer_for_test(before_lifecycle_gate)?;
        self.retire_active_connection_locked(attempt, |active| action(active).map(Some))
            .map(retired_value)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn retirement_waiter_count_for_test(&self) -> usize {
        self.connection_lifecycle.waiting_for_test()
    }

    /// Apply one finger candidate or retain its proof until its current handshake commits.
    ///
    /// `request` identifies the specific finger lookup being satisfied. The
    /// returned disposition tells the message handler whether it should open a
    /// transport connection or cancel a proof that could not gain an owner.
    pub(crate) fn record_finger_candidate(
        &self,
        peer: Did,
        request: FingerFixRequest,
    ) -> Result<FingerUpdateDisposition> {
        self.record_finger_candidate_with_observer(peer, request, || {})
    }

    /// Classify and either apply, retain, or reject a reported finger candidate.
    ///
    /// The lifecycle boundary covers the snapshot, proof transfer, and queue
    /// attachment so retirement cannot change the owning generation between
    /// validation and commit. `observe_admission` is a test synchronization
    /// hook invoked only after the branch is selected and before its mutation.
    fn record_finger_candidate_with_observer(
        &self,
        peer: Did,
        request: FingerFixRequest,
        observe_admission: impl FnOnce(),
    ) -> Result<FingerUpdateDisposition> {
        // The lifecycle guard keeps classification, proof transfer, and queue
        // attachment in one critical section. No async network effect occurs
        // while it is held.
        let _lifecycle_guard = self.connection_lifecycle()?;
        if peer == self.dht.did {
            return self
                .dht
                .apply_fixed_finger(request, peer)
                .map(FingerUpdateDisposition::from);
        }
        let (lifecycle, active) = {
            let lifecycles = self.peer_lifecycles()?;
            (lifecycles.state(peer), lifecycles.active_connections())
        };
        let is_routable = self.is_routable_active_candidate(peer, &active);
        match finger_candidate_admission(lifecycle, is_routable) {
            FingerCandidateAdmission::Queue(current) => {
                observe_admission();
                // Acquire the queue before transferring proof ownership. If
                // this lock fails, the proof remains AwaitingReport rather
                // than becoming an admission proof with no transport owner.
                let mut pending_updates = self.pending_finger_updates()?;
                match self.dht.defer_fixed_finger(request, peer)? {
                    FingerDeferOutcome::Deferred { .. } => {
                        // The DHT owns one attempt at a time, so a generation
                        // retains one proof; a newer request replaces one the
                        // DHT has already released.
                        pending_updates.insert(current, request);
                        Ok(FingerUpdateDisposition::Queued)
                    }
                    FingerDeferOutcome::Rejected(rejection) => {
                        Ok(FingerUpdateDisposition::Rejected(rejection))
                    }
                }
            }
            FingerCandidateAdmission::Apply => {
                observe_admission();
                self.dht
                    .apply_fixed_finger(request, peer)
                    .map(FingerUpdateDisposition::from)
            }
            FingerCandidateAdmission::Missing => {
                observe_admission();
                // The report becomes a durable, expiring admission proof
                // before the caller starts an async WebRTC handshake. A
                // cancelled or hung handler therefore cannot strand it.
                self.dht
                    .defer_fixed_finger(request, peer)
                    .map(|outcome| match outcome {
                        FingerDeferOutcome::Deferred { .. } => FingerUpdateDisposition::Missing,
                        FingerDeferOutcome::Rejected(rejection) => {
                            FingerUpdateDisposition::Rejected(rejection)
                        }
                    })
            }
            FingerCandidateAdmission::Unroutable => {
                // Validate token and successor in the same topology
                // transition that retires the unusable candidate. Checking
                // only the token here would let a conflicting duplicate evict
                // a different successor's retained admission proof.
                match self.dht.retire_finger_candidate(request, peer)? {
                    FingerRetireOutcome::Retired => Ok(FingerUpdateDisposition::Unroutable),
                    FingerRetireOutcome::Rejected(rejection) => {
                        Ok(FingerUpdateDisposition::Rejected(rejection))
                    }
                }
            }
        }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn record_finger_candidate_with_observer_for_test(
        &self,
        peer: Did,
        request: FingerFixRequest,
        observe_admission: impl FnOnce(),
    ) -> Result<FingerUpdateDisposition> {
        self.record_finger_candidate_with_observer(peer, request, observe_admission)
    }

    /// Cancel a current pending or admitting handshake and release its transport object.
    pub(crate) async fn cancel_unadmitted_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.cancel_connection(attempt, CancellationScope::Unadmitted)
            .await
    }

    /// Cancel `attempt` iff it is still pending, that is, no data channel has opened on it, and
    /// release its transport object. An admitting generation has a data channel open and its
    /// DHT admission in flight; it is left to complete, or to fail on its own path.
    pub(crate) async fn cancel_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.cancel_connection(attempt, CancellationScope::Pending)
            .await
    }

    /// Release the unadmitted record of `attempt` within `scope` and close its transport
    /// object; `false` when `attempt` owns no record in that scope.
    async fn cancel_connection(
        &self,
        attempt: PendingConnectionAttempt,
        scope: CancellationScope,
    ) -> Result<bool> {
        let Some(retired) = self.retire_pending_connection_for_close(attempt, scope)? else {
            return Ok(false);
        };
        tracing::debug!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "pending connection cancelled"
        );
        if let Some(connection) = retired.connection {
            self.transport
                .close_connection_if_current(&connection.connection)
                .await
                .map_err(Error::Transport)?;
        }
        Ok(true)
    }

    fn retire_pending_connection_for_close(
        &self,
        attempt: PendingConnectionAttempt,
        scope: CancellationScope,
    ) -> Result<Option<RetiredPendingConnection>> {
        let _lifecycle = self.connection_lifecycle()?;
        let removed = {
            let mut lifecycles = self.peer_lifecycles()?;
            match scope {
                CancellationScope::Pending => lifecycles.remove_pending(attempt),
                CancellationScope::Unadmitted => lifecycles.remove_unadmitted(attempt),
            }
        };
        if !removed {
            return Ok(None);
        }
        self.cancel_pending_finger_updates(attempt)?;
        Ok(Some(RetiredPendingConnection {
            connection: self.get_raw_connection(attempt.peer),
        }))
    }

    pub(super) async fn abandon_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        operation: &str,
    ) {
        if let Err(error) = self.cancel_unadmitted_connection(attempt).await {
            tracing::warn!(
                "failed to cancel pending connection to {} after {operation}: {error}",
                attempt.peer
            );
        }
    }

    /// Close pending handshakes whose data channel did not open before the deadline.
    ///
    /// These peers have never entered the DHT, so expiry only releases the
    /// transport object; it deliberately performs no topology mutation.
    pub(crate) async fn expire_pending_connections(&self) -> Result<()> {
        let expired = {
            let _lifecycle = self.connection_lifecycle()?;
            let expired = self.peer_lifecycles()?.expire(get_epoch_ms_i64());
            expired
                .into_iter()
                .map(|expired| {
                    self.cancel_pending_finger_updates(expired.attempt)?;
                    let connection = self.get_raw_connection(expired.attempt.peer);
                    Ok((expired, connection))
                })
                .collect::<Result<Vec<_>>>()?
        };
        for (expired, connection) in expired {
            let attempt = expired.attempt;
            let state = connection
                .as_ref()
                .map(|connection| connection.webrtc_connection_state());
            tracing::warn!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %attempt.peer,
                generation = attempt.generation,
                age_ms = expired.age_ms,
                timeout_ms = PENDING_CONNECTION_TIMEOUT_MS,
                phase = expired.phase.as_str(),
                state = ?state,
                "connection attempt timed out before admission commit"
            );
            if let Some(connection) = connection {
                self.transport
                    .close_connection_if_current(&connection.connection)
                    .await
                    .map_err(Error::Transport)?;
            }
        }
        Ok(())
    }

    /// Create a new non-routable transport connection and register its pending attempt.
    pub(super) async fn new_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        callback: InnerSwarmCallback,
    ) -> Result<PendingTransportConnection> {
        // The per-peer creation lease prevents two async backend calls from
        // racing to install different raw transports for one logical peer.
        let creation = self.connection_creation.lease(attempt.peer);
        let _guard = creation.acquire().await;
        match self.is_current_connection_attempt(attempt) {
            Ok(true) => {
                self.create_pending_transport_connection(attempt, callback)
                    .await
            }
            Ok(false) => Err(Error::ConnectionAttemptSuperseded {
                peer: attempt.peer,
                generation: attempt.generation,
            }),
            Err(error) => Err(error),
        }
    }

    async fn create_pending_transport_connection(
        &self,
        attempt: PendingConnectionAttempt,
        callback: InnerSwarmCallback,
    ) -> Result<PendingTransportConnection> {
        let cid = attempt.peer.to_string();
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "creating pending transport connection"
        );
        let connection = match self
            .transport
            .new_connection(&cid, Box::new(callback))
            .await
        {
            Ok(connection) => PendingTransportConnection {
                attempt,
                connection: SwarmConnection {
                    peer: attempt.peer,
                    connection,
                },
            },
            Err(error) => {
                let _ = self.retire_pending_connection(attempt);
                return Err(Error::Transport(error));
            }
        };
        // Backend creation awaited outside the lifecycle gate, so re-check
        // generation ownership before exposing the raw transport to callbacks.
        let still_current = match self.is_current_connection_attempt(attempt) {
            Ok(still_current) => still_current,
            Err(error) => {
                if let Err(close_error) = self
                    .transport
                    .close_connection_if_current(&connection.connection().connection)
                    .await
                {
                    tracing::warn!(
                        peer = %attempt.peer,
                        generation = attempt.generation,
                        error = ?close_error,
                        "failed to close pending transport after lifecycle lookup failed"
                    );
                }
                return Err(error);
            }
        };
        if !still_current {
            self.transport
                .close_connection_if_current(&connection.connection().connection)
                .await
                .map_err(Error::Transport)?;
            return Err(Error::ConnectionAttemptSuperseded {
                peer: attempt.peer,
                generation: attempt.generation,
            });
        }
        tracing::trace!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "pending transport connection created"
        );
        Ok(connection)
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn pending_connection_count(&self) -> Result<usize> {
        Ok(self.peer_lifecycles()?.pending_len())
    }
}

#[cfg(test)]
mod test_lifecycle_model;
