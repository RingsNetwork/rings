use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use rings_transport::core::transport::TransportInterface;
mod registry;

pub(super) use registry::ActiveConnectionSet;
pub(super) use registry::ConnectionLifecycleRegistry;
use registry::PeerConnectionLifecycle;

use super::SwarmConnection;
use super::SwarmTransport;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::callback::InnerSwarmCallback;
use crate::utils::get_epoch_ms_i64;

/// Maximum number of peers that may be handshaking before a data channel opens.
pub(crate) const DEFAULT_PENDING_CONNECTION_CAPACITY: usize = 32;

pub(super) const PENDING_CONNECTION_TIMEOUT_MS: i64 = 180_000;

pub(super) type SharedConnectionLifecycles =
    Arc<Mutex<ConnectionLifecycleRegistry<DEFAULT_PENDING_CONNECTION_CAPACITY>>>;
pub(super) type PendingFingerUpdates =
    BTreeMap<PendingConnectionAttempt, BTreeMap<usize, Option<Did>>>;
type PendingFingerUpdatesGuard<'transport> =
    std::sync::MutexGuard<'transport, PendingFingerUpdates>;

/// Shared serialization boundary for logical connection ownership.
///
/// Clone law: every clone refers to the same mutex. Holding the boundary
/// prevents admission, retirement, and final send admission from crossing.
#[derive(Clone)]
pub(super) struct ConnectionLifecycleBoundary {
    inner: Arc<Mutex<()>>,
}

impl ConnectionLifecycleBoundary {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(())),
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
}

/// Identifies one pending handshake for a peer.
///
/// A peer can have a replacement handshake after a timeout. Callbacks carry
/// this token so a late callback from the replaced connection cannot promote
/// the newer handshake into the active routing set.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PendingConnectionAttempt {
    pub(super) peer: Did,
    pub(super) generation: u64,
}

impl PendingConnectionAttempt {
    pub(crate) fn peer(self) -> Did {
        self.peer
    }

    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionEventDisposition {
    Deliver,
    Suppress { active: PendingConnectionAttempt },
}

/// Result of reconciling one reported finger with connection ownership.
///
/// The variants make the state transition exhaustive: a candidate is either
/// committed, retained by the current handshake, absent, or owned by an active
/// transport that is not presently routable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerUpdateDisposition {
    /// The candidate was committed to the finger table.
    Applied,
    /// The candidate was attached to the current pending generation.
    Queued,
    /// No logical connection generation exists for the candidate.
    Missing,
    /// An active generation exists, but its transport cannot make progress.
    Unroutable,
}

impl FingerUpdateDisposition {
    /// Whether the caller should start a connection before retrying admission.
    pub(crate) const fn needs_connection(self) -> bool {
        matches!(self, Self::Missing)
    }
}

/// Pure plan for reconciling one finger candidate with a lifecycle snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FingerCandidateAdmission {
    Apply,
    Queue(PendingConnectionAttempt),
    Missing,
    Unroutable,
}

// Pre: `is_routable` describes the transport owned by `lifecycle`.
// Post: every lifecycle state maps to exactly one effect plan.
fn finger_candidate_admission(
    lifecycle: Option<PeerConnectionLifecycle>,
    is_routable: bool,
) -> FingerCandidateAdmission {
    match lifecycle {
        Some(
            PeerConnectionLifecycle::Pending { attempt, .. }
            | PeerConnectionLifecycle::Admitting { attempt, .. },
        ) => FingerCandidateAdmission::Queue(attempt),
        Some(PeerConnectionLifecycle::Active(_)) if is_routable => FingerCandidateAdmission::Apply,
        Some(PeerConnectionLifecycle::Active(_)) => FingerCandidateAdmission::Unroutable,
        None => FingerCandidateAdmission::Missing,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RawConnectionOwner {
    Pending(PendingConnectionAttempt),
    Owned,
    Orphan,
}

fn event_disposition(
    state: Option<PeerConnectionLifecycle>,
    source: PendingConnectionAttempt,
) -> ConnectionEventDisposition {
    match state {
        Some(PeerConnectionLifecycle::Active(active)) if active != source => {
            ConnectionEventDisposition::Suppress { active }
        }
        Some(PeerConnectionLifecycle::Pending { .. })
        | Some(PeerConnectionLifecycle::Admitting { .. })
        | Some(PeerConnectionLifecycle::Active(_))
        | None => ConnectionEventDisposition::Deliver,
    }
}

struct RetiredPendingConnection {
    connection: Option<SwarmConnection>,
}

pub(super) struct PendingTransportConnection {
    attempt: PendingConnectionAttempt,
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

    pub(super) fn peer_lifecycles(
        &self,
    ) -> Result<
        std::sync::MutexGuard<'_, ConnectionLifecycleRegistry<DEFAULT_PENDING_CONNECTION_CAPACITY>>,
    > {
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
                PeerConnectionLifecycle::Admitting { .. } | PeerConnectionLifecycle::Active(_),
            ) => RawConnectionOwner::Owned,
            None => RawConnectionOwner::Orphan,
        })
    }

    /// Return whether `peer` completed a handshake and still owns a logical slot.
    ///
    /// This remains true while a terminal callback removes the peer, so
    /// lifecycle cleanup can evict it from the DHT even after WebRTC reports
    /// `Closed`.
    pub(crate) fn is_admitted_connection(&self, peer: Did) -> bool {
        self.peer_lifecycles()
            .map(|lifecycles| lifecycles.active_attempt(peer).is_some())
            .unwrap_or(false)
    }

    /// Return whether `attempt` owns the current active slot for its peer.
    ///
    /// Invariant: a terminal callback may remove an active peer only when its
    /// generation equals the generation admitted by data-channel open.
    pub(crate) fn is_admitted_connection_attempt(&self, attempt: PendingConnectionAttempt) -> bool {
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

    /// Validate a reservation and expire stale lifecycle records before its commit.
    ///
    /// Separation law: validation is pure; expiry and commit are separate lifecycle mutations,
    /// each serialized by the shared boundary without holding a synchronous lock across await.
    async fn prepare_pending_reservation(&self, peer: Did) -> Result<()> {
        self.validate_pending_reservation(peer)?;
        self.expire_pending_connections().await
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
        tracing::info!(
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

    pub(crate) fn pending_connection_with_attempt(
        &self,
        peer: Did,
    ) -> Result<Option<(PendingConnectionAttempt, SwarmConnection)>> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.peer_lifecycles()?.pending_attempt(peer) else {
                return Ok(None);
            };
            Ok(self
                .get_raw_connection(peer)
                .map(|connection| (attempt, connection)))
        })
    }

    pub(crate) fn active_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        Ok(self.peer_lifecycles()?.active_attempt(peer))
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
        tracing::info!(
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
        let fixed_fingers = pending_finger_updates
            .get(&attempt)
            .map(|updates| {
                updates
                    .iter()
                    .map(
                        |(index, expected)| crate::dht::topology::ConditionalFingerUpdate {
                            index: *index,
                            expected: *expected,
                        },
                    )
                    .collect()
            })
            .unwrap_or_default();
        let action = self.dht.admit_connected(attempt.peer, fixed_fingers)?;

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

    pub(super) fn retire_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let _lifecycle = self.connection_lifecycle()?;
        let removed = self.peer_lifecycles()?.remove_pending(attempt);
        if removed {
            self.pending_finger_updates()?.remove(&attempt);
        }
        Ok(removed)
    }

    pub(super) fn retire_active_connection_with<T>(
        &self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce(&ActiveConnectionSet) -> Result<T>,
    ) -> Result<Option<T>> {
        let _lifecycle = self.connection_lifecycle()?;
        let mut lifecycles = self.peer_lifecycles()?;
        if lifecycles.active_attempt(attempt.peer) != Some(attempt) {
            return Ok(None);
        }
        let active = lifecycles.active_connections();

        // Acquire every fallible local-state guard before mutating the DHT.
        // The lifecycle lock prevents admission or retirement from changing
        // `active` while the action validates a successor fallback against it.
        let mut pending_finger_updates = self.pending_finger_updates()?;
        let mut peer_liveness = self.peer_liveness()?;
        let mut measured_disconnects = self
            .measured_disconnects
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)?;
        let result = action(&active)?;

        // These mutations are infallible after the DHT action commits. If the
        // action fails, all four guards drop without changing local state.
        lifecycles.remove_active(attempt);
        pending_finger_updates.retain(|pending, _| pending.peer != attempt.peer);
        peer_liveness.remove(attempt.peer);
        measured_disconnects.remove(&attempt.peer);
        self.outbound_schedulers.shutdown(attempt.peer);
        Ok(Some(result))
    }

    /// Apply one finger candidate or retain it until its current handshake commits.
    pub(crate) fn record_finger_candidate(
        &self,
        peer: Did,
        index: usize,
    ) -> Result<FingerUpdateDisposition> {
        self.record_finger_candidate_with_observer(peer, index, || {})
    }

    fn record_finger_candidate_with_observer(
        &self,
        peer: Did,
        index: usize,
        observe_admission: impl FnOnce(),
    ) -> Result<FingerUpdateDisposition> {
        let _lifecycle = self.connection_lifecycle()?;
        let (lifecycle, active) = {
            let lifecycles = self.peer_lifecycles()?;
            (lifecycles.state(peer), lifecycles.active_connections())
        };
        let is_routable = self.is_routable_active_candidate(peer, &active);
        match finger_candidate_admission(lifecycle, is_routable) {
            FingerCandidateAdmission::Queue(current) => {
                observe_admission();
                let expected = self
                    .dht
                    .topology_state()?
                    .fingers
                    .get(index)
                    .copied()
                    .flatten();
                self.pending_finger_updates()?
                    .entry(current)
                    .or_default()
                    .entry(index)
                    .or_insert(expected);
                Ok(FingerUpdateDisposition::Queued)
            }
            FingerCandidateAdmission::Apply => {
                observe_admission();
                self.dht.apply_fixed_finger(index, peer)?;
                Ok(FingerUpdateDisposition::Applied)
            }
            FingerCandidateAdmission::Missing => Ok(FingerUpdateDisposition::Missing),
            FingerCandidateAdmission::Unroutable => Ok(FingerUpdateDisposition::Unroutable),
        }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn record_finger_candidate_with_observer_for_test(
        &self,
        peer: Did,
        index: usize,
        observe_admission: impl FnOnce(),
    ) -> Result<FingerUpdateDisposition> {
        self.record_finger_candidate_with_observer(peer, index, observe_admission)
    }

    /// Cancel a current pending or admitting handshake and release its transport object.
    pub(crate) async fn cancel_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let Some(retired) = self.retire_pending_connection_for_close(attempt)? else {
            return Ok(false);
        };
        tracing::info!(
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
    ) -> Result<Option<RetiredPendingConnection>> {
        let _lifecycle = self.connection_lifecycle()?;
        if !self.peer_lifecycles()?.remove_unadmitted(attempt) {
            return Ok(None);
        }
        self.pending_finger_updates()?.remove(&attempt);
        Ok(Some(RetiredPendingConnection {
            connection: self.get_raw_connection(attempt.peer),
        }))
    }

    pub(super) async fn abandon_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        operation: &str,
    ) {
        if let Err(error) = self.cancel_pending_connection(attempt).await {
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
                    self.pending_finger_updates()?.remove(&expired.attempt);
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
        tracing::info!(
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
        tracing::info!(
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
#[path = "pending/lifecycle_model.rs"]
mod lifecycle_model;
