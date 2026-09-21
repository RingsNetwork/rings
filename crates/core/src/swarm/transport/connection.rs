use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use rings_transport::core::transport::WebrtcConnectionState;

use super::pending::ActiveConnectionSet;
use super::pending::ConnectionLifecycleBoundary;
use super::pending::RetirementOutcome;
use super::pending::SharedConnectionLifecycles;
use super::PendingConnectionAttempt;
use super::SwarmConnection;
use super::SwarmTransport;
use super::TRANSPORT_TIMEOUT_PROFILE;
use crate::dht::topology::TopologyRemoval;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::utils::sleep;

/// Maximum wait for the data channel to open after the peer connection is usable.
const DATA_CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(8);
/// Poll cadence while waiting for the combined WebRTC/data-channel readiness snapshot.
const TRANSPORT_READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Maximum wait for a close operation before lifecycle cleanup continues.
pub(super) const DATA_CHANNEL_CLOSE_TIMEOUT: Duration = TRANSPORT_TIMEOUT_PROFILE.close;

/// Await a transport close future without letting a slow backend block lifecycle cleanup forever.
pub(super) async fn await_bounded_connection_close(
    close: impl Future<Output = Result<()>>,
) -> Result<bool> {
    let close = close.fuse();
    let timeout = sleep(DATA_CHANNEL_CLOSE_TIMEOUT).fuse();
    pin_mut!(close, timeout);
    select! {
        result = close => result.map(|()| true),
        _ = timeout => Ok(false),
    }
}

/// Which topology removal a retirement performs: an ordinary leave keeps the
/// surviving successor tail, an unavailable head is replaced by the routable
/// admitted successors. Visible to the transport module so the rejoin model
/// composes the same two removals.
#[cfg_attr(test, derive(Clone, Copy, Debug, Eq, PartialEq))]
pub(in crate::swarm::transport) enum DhtPeerRemoval {
    /// Remove the peer without selecting a replacement successor.
    Ordinary,
    /// Remove the peer as unreachable and allow the DHT to promote live replacements.
    Unavailable,
}

/// Proof that one physical connection belongs to the current admitted
/// generation for its peer.
///
/// Clone law: clones identify the same physical connection generation. They do
/// not authorize sends by possession; every asynchronous progression
/// revalidates the generation through [`Self::ensure_current`] or
/// [`Self::with_current_connection`].
#[derive(Clone)]
pub(crate) struct AdmittedConnection {
    /// Generation token that must still own the active slot before sends proceed.
    attempt: PendingConnectionAttempt,
    /// Physical transport connection associated with `attempt`.
    connection: SwarmConnection,
    /// Shared gate preventing send admission from crossing retirement.
    lifecycle_boundary: ConnectionLifecycleBoundary,
    /// Registry used to revalidate that `attempt` is still sendable.
    lifecycles: SharedConnectionLifecycles,
}

impl AdmittedConnection {
    fn new(
        attempt: PendingConnectionAttempt,
        connection: SwarmConnection,
        lifecycle_boundary: ConnectionLifecycleBoundary,
        lifecycles: SharedConnectionLifecycles,
    ) -> Self {
        Self {
            attempt,
            connection,
            lifecycle_boundary,
            lifecycles,
        }
    }

    pub(crate) const fn attempt(&self) -> PendingConnectionAttempt {
        self.attempt
    }

    pub(crate) const fn connection(&self) -> &SwarmConnection {
        &self.connection
    }

    pub(crate) fn ensure_current(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_boundary.lock()?;
        let lifecycles = self
            .lifecycles
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)?;
        if lifecycles.sendable_attempt(self.attempt.peer) == Some(self.attempt) {
            return Ok(());
        }
        Err(Error::ConnectionAttemptSuperseded {
            peer: self.attempt.peer,
            generation: self.attempt.generation,
        })
    }

    /// Run `operation` while this connection generation cannot be retired.
    ///
    /// `Ok(Some(value))` proves that `attempt` was active throughout
    /// `operation`. `Ok(None)` means the generation had already been revoked.
    pub(crate) fn with_current_connection<T>(
        &self,
        operation: impl FnOnce(&SwarmConnection) -> T,
    ) -> Result<Option<T>> {
        let _lifecycle = self.lifecycle_boundary.lock()?;
        let is_current = self
            .lifecycles
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)?
            .sendable_attempt(self.attempt.peer)
            == Some(self.attempt);
        if !is_current {
            return Ok(None);
        }
        Ok(Some(operation(&self.connection)))
    }

    /// Revoke new sends for this exact generation before transport cleanup.
    pub(crate) fn mark_send_terminal(&self) -> Result<bool> {
        let _lifecycle = self.lifecycle_boundary.lock()?;
        let mut lifecycles = self
            .lifecycles
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)?;
        Ok(lifecycles.mark_send_terminal(self.attempt))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PeerRemovalOutcome {
    /// First live successor replacement selected while the peer was removed.
    fallback: Option<Did>,
    /// Whether the topology referenced the peer when it was removed.
    removal: TopologyRemoval,
}

impl PeerRemovalOutcome {
    pub(crate) const fn fallback(self) -> Option<Did> {
        self.fallback
    }

    pub(crate) const fn removal(self) -> TopologyRemoval {
        self.removal
    }
}

impl SwarmTransport {
    /// Get an active, routable connection by DID.
    ///
    /// Pending and non-ready physical transports are intentionally invisible here.
    pub fn get_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.with_connection_lifecycle(|| {
            if self.peer_lifecycles()?.sendable_attempt(peer).is_none() {
                return Ok(None);
            }
            let Some(connection) = self.get_raw_connection(peer) else {
                return Ok(None);
            };
            Ok(connection
                .readiness()
                .can_make_progress()
                .then_some(connection))
        })
        .ok()
        .flatten()
    }

    pub(crate) fn admitted_connection_with_attempt(
        &self,
        peer: Did,
    ) -> Result<Option<(PendingConnectionAttempt, SwarmConnection)>> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.peer_lifecycles()?.sendable_attempt(peer) else {
                return Ok(None);
            };
            let Some(connection) = self.get_raw_connection(peer) else {
                return Ok(None);
            };
            Ok(Some((attempt, connection)))
        })
    }

    pub(crate) fn admitted_connection(&self, peer: Did) -> Result<Option<SwarmConnection>> {
        Ok(self
            .admitted_connection_with_attempt(peer)?
            .map(|(_, connection)| connection))
    }

    pub(crate) fn admitted_send_connection(&self, peer: Did) -> Result<Option<AdmittedConnection>> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.peer_lifecycles()?.sendable_attempt(peer) else {
                return Ok(None);
            };
            let Some(connection) = self.get_raw_connection(peer) else {
                return Ok(None);
            };
            Ok(Some(AdmittedConnection::new(
                attempt,
                connection,
                self.connection_lifecycle.clone(),
                Arc::clone(&self.peer_lifecycles),
            )))
        })
    }

    pub(crate) fn admitted_connection_snapshots(
        &self,
    ) -> Result<Vec<(PendingConnectionAttempt, Option<SwarmConnection>)>> {
        self.with_connection_lifecycle(|| {
            let admitted = self.peer_lifecycles()?.admitted_connections();
            Ok(admitted
                .iter()
                .map(|attempt| (attempt, self.get_raw_connection(attempt.peer)))
                .collect())
        })
    }

    pub(crate) fn is_send_terminal_attempt(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        Ok(self.peer_lifecycles()?.is_send_terminal(attempt))
    }

    /// Get all active, ready transport connections.
    pub fn get_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.active_peer_ids()
            .into_iter()
            .filter_map(|peer| {
                self.get_connection(peer)
                    .map(|connection| (peer, connection))
            })
            .collect()
    }

    fn active_peer_ids(&self) -> Vec<Did> {
        self.active_connections()
            .map(|active| active.iter().map(|attempt| attempt.peer).collect())
            .unwrap_or_default()
    }

    /// Return admitted transports, including a terminal connection that still
    /// needs lifecycle cleanup. Pending and admitting transports are excluded.
    /// This is deliberately internal: callers outside the swarm only observe
    /// routable connections through [`Self::get_connections`].
    pub(crate) fn admitted_connections(&self) -> Vec<(PendingConnectionAttempt, SwarmConnection)> {
        self.admitted_connection_snapshots()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(attempt, connection)| connection.map(|connection| (attempt, connection)))
            .collect()
    }

    /// Return admitted DIDs, even if their raw transport object has already gone away.
    pub(crate) fn admitted_connection_ids(&self) -> Vec<Did> {
        self.admitted_connection_snapshots()
            .unwrap_or_default()
            .into_iter()
            .map(|(attempt, _)| attempt.peer)
            .collect()
    }

    /// Admit one currently routable peer into the local topology.
    ///
    /// The active generation and readiness proof remain protected until the
    /// topology transition commits.
    pub(crate) fn join_routable_peer(&self, peer: Did) -> Result<Option<PeerRingAction>> {
        self.join_routable_peer_with_observer(peer, || {})
    }

    fn join_routable_peer_with_observer(
        &self,
        peer: Did,
        observe_admission: impl FnOnce(),
    ) -> Result<Option<PeerRingAction>> {
        self.with_connection_lifecycle(|| {
            let active = self.active_connections()?;
            if !self.is_routable_active_candidate(peer, &active) {
                return Ok(None);
            }
            observe_admission();
            self.dht.admit_connected(peer, None).map(Some)
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn join_routable_peer_with_observer_for_test(
        &self,
        peer: Did,
        observe_admission: impl FnOnce(),
    ) -> Result<Option<PeerRingAction>> {
        self.join_routable_peer_with_observer(peer, observe_admission)
    }

    /// Apply a predecessor notification only while its origin remains admitted.
    ///
    /// Connection retirement and the topology transition share the lifecycle
    /// boundary, preventing a retired origin from being reintroduced after
    /// disconnect cleanup.
    pub(crate) fn notify_admitted_predecessor(&self, peer: Did) -> Result<Option<Did>> {
        self.notify_admitted_predecessor_with_observer(peer, || {})
    }

    fn notify_admitted_predecessor_with_observer(
        &self,
        peer: Did,
        observe_admission: impl FnOnce(),
    ) -> Result<Option<Did>> {
        self.with_connection_lifecycle(|| {
            let active = self.active_connections()?;
            if !self.is_routable_active_candidate(peer, &active) {
                return Ok(None);
            }
            observe_admission();
            self.dht.notify(peer).map(Some)
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn notify_admitted_predecessor_with_observer_for_test(
        &self,
        peer: Did,
        observe_admission: impl FnOnce(),
    ) -> Result<Option<Did>> {
        self.notify_admitted_predecessor_with_observer(peer, observe_admission)
    }

    /// Apply one correlated topology report using only peers that are still routable.
    ///
    /// Filtering and the DHT transition share the connection lifecycle
    /// boundary, so retirement cannot invalidate the evidence between them.
    /// `reporter` and `request_id` must match the stabilization slot already
    /// claimed by the message handler.
    pub(crate) fn stabilize_routable_topology(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
        reported: &TopoInfo,
    ) -> Result<Option<PeerRingAction>> {
        self.stabilize_routable_topology_with_observer(reporter, request_id, reported, || {})
    }

    /// Commit a correlated topology report while its transport evidence remains valid.
    ///
    /// The lifecycle gate prevents reporter retirement between readiness
    /// validation and DHT mutation. The observer is a test synchronization hook
    /// called after all reported peers are filtered but before the correlated
    /// stabilization transition consumes `request_id`.
    fn stabilize_routable_topology_with_observer(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
        reported: &TopoInfo,
        observe_confirmation: impl FnOnce(),
    ) -> Result<Option<PeerRingAction>> {
        self.with_connection_lifecycle(|| {
            let active = self.active_connections()?;
            // A report from a retired or terminal peer cannot validate any
            // successor/predecessor evidence, even if its request id is correct.
            if !self.is_routable_active_candidate(reporter, &active) {
                return Ok(None);
            }
            // Keep only peers that are either local or still have a routable
            // active transport generation at the instant of commit.
            let confirmed = reported.confirmed_by(|peer| {
                peer == self.dht.did || self.is_routable_active_candidate(peer, &active)
            });
            if !confirmed.has_confirmed_peer() {
                return Ok(None);
            }
            observe_confirmation();
            self.dht
                .stabilize_reported_by(reporter, request_id, confirmed)
                .map(Some)
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn stabilize_routable_topology_with_observer_for_test(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
        reported: &TopoInfo,
        observe_confirmation: impl FnOnce(),
    ) -> Result<Option<PeerRingAction>> {
        self.stabilize_routable_topology_with_observer(
            reporter,
            request_id,
            reported,
            observe_confirmation,
        )
    }

    /// Get DIDs of active, routable connections.
    pub fn get_connection_ids(&self) -> Vec<Did> {
        self.get_connections()
            .into_iter()
            .map(|(peer, _)| peer)
            .collect()
    }

    /// Disconnect a connection.
    ///
    /// Pending connections are never represented in the DHT, so cancelling one
    /// only closes its transport object. Active connections leave the DHT before
    /// the underlying WebRTC object is released.
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        if let Some(attempt) = self.unadmitted_attempt(peer)? {
            if self.cancel_unadmitted_connection(attempt).await? {
                return Ok(());
            }
            return self
                .disconnect_with_removal(attempt, DhtPeerRemoval::Ordinary)
                .await
                .map(|_| ());
        }
        if let Some(attempt) = self.active_attempt(peer)? {
            return self
                .disconnect_with_removal(attempt, DhtPeerRemoval::Ordinary)
                .await
                .map(|_| ());
        }
        if let Some(connection) = self.get_raw_connection(peer) {
            self.transport
                .close_connection_if_current(&connection.connection)
                .await
                .map_err(Error::Transport)?;
        }
        Ok(())
    }

    /// Disconnect an active peer while promoting a validated successor.
    pub(crate) async fn disconnect_unavailable(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<Option<PeerRemovalOutcome>> {
        self.disconnect_with_removal(attempt, DhtPeerRemoval::Unavailable)
            .await
    }

    /// Disconnect an admitted peer, reporting whether the topology referenced
    /// it; `None` when `attempt` is no longer the admitted generation.
    pub(crate) async fn disconnect_attempt(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<Option<TopologyRemoval>> {
        Ok(self
            .disconnect_with_removal(attempt, DhtPeerRemoval::Ordinary)
            .await?
            .map(PeerRemovalOutcome::removal))
    }

    pub(crate) fn remove_unavailable_topology(
        &self,
        peer: Did,
        expected: Option<PendingConnectionAttempt>,
    ) -> Result<Option<PeerRemovalOutcome>> {
        self.with_connection_lifecycle(|| {
            let active_attempt = self.active_attempt(peer)?;
            match expected {
                Some(attempt) if active_attempt != Some(attempt) => return Ok(None),
                None if active_attempt.is_some() => return Ok(None),
                _ => {}
            }
            let replacements = self.live_successor_replacements(peer)?;
            let fallback = replacements.first().copied();
            let removal = self.dht.remove_unavailable(peer, replacements)?;
            Ok(Some(PeerRemovalOutcome { fallback, removal }))
        })
    }

    /// Remove the topology references of a peer whose generation is already
    /// retired; `None` when a newer generation of the peer is admitted.
    pub(crate) fn remove_retired_attempt_topology(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<Option<TopologyRemoval>> {
        self.with_connection_lifecycle(|| {
            if self.active_attempt(attempt.peer)?.is_some() {
                return Ok(None);
            }
            Ok(Some(self.dht.remove(attempt.peer)?))
        })
    }

    pub(crate) fn live_successor_fallback(&self, removed: Did) -> Result<Option<Did>> {
        Ok(self.live_successor_replacements(removed)?.first().copied())
    }

    fn live_successor_replacements(&self, removed: Did) -> Result<Vec<Did>> {
        let active = self.active_connections()?;
        self.live_successor_replacements_from_active(removed, &active)
    }

    fn live_successor_replacements_from_active(
        &self,
        removed: Did,
        active: &ActiveConnectionSet,
    ) -> Result<Vec<Did>> {
        let topology = self.dht.topology_state()?;
        if topology.successors.first().copied() != Some(removed) {
            return Ok(Vec::new());
        }

        let mut candidates = active
            .iter()
            .map(PendingConnectionAttempt::peer)
            .filter(|candidate| *candidate != self.dht.did && *candidate != removed)
            .collect::<Vec<_>>();
        let observer = self.dht.did;
        candidates.sort_by(|left, right| Did::cmp_from_observer(observer, *left, *right));
        candidates.dedup();

        let capacity = self.dht.successors().capacity();
        if capacity == 0 {
            return Ok(Vec::new());
        }
        let mut replacements = Vec::with_capacity(capacity);
        for candidate in candidates {
            if self.is_routable_active_candidate(candidate, active) {
                replacements.push(candidate);
                if replacements.len() == capacity {
                    break;
                }
            }
        }
        Ok(replacements)
    }

    pub(super) fn is_routable_active_candidate(
        &self,
        candidate: Did,
        active: &ActiveConnectionSet,
    ) -> bool {
        if active.attempt(candidate).is_none() {
            return false;
        }
        let Some(connection) = self.get_raw_connection(candidate) else {
            return false;
        };
        connection.readiness().can_make_progress()
    }

    /// Retire `attempt` if `action` decides to, announcing the retirement in the peer's ordered
    /// delivery. The delivery turn is taken before the record is retired, so a later admission
    /// of the same peer cannot announce ahead of this retirement.
    async fn retire_announced_if<T>(
        &self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce(&ActiveConnectionSet) -> Result<Option<T>>,
    ) -> Result<RetirementOutcome<T>> {
        self.with_delivery_turn(attempt.peer, |turn| async move {
            Ok(match self.retire_active_connection_if(attempt, action)? {
                RetirementOutcome::Superseded => RetirementOutcome::Superseded,
                RetirementOutcome::Declined => RetirementOutcome::Declined,
                RetirementOutcome::Retired((value, retirement)) => {
                    self.announce_retirement(turn, attempt.peer, retirement)
                        .await;
                    RetirementOutcome::Retired(value)
                }
            })
        })
        .await
    }

    async fn disconnect_with_removal(
        &self,
        attempt: PendingConnectionAttempt,
        removal: DhtPeerRemoval,
    ) -> Result<Option<PeerRemovalOutcome>> {
        let connection = self.get_raw_connection(attempt.peer);
        // Removal never declines: the action always commits.
        let Some(outcome) = self
            .retire_announced_if(attempt, |active| match removal {
                DhtPeerRemoval::Ordinary => Ok(Some(PeerRemovalOutcome {
                    fallback: None,
                    removal: self.dht.remove(attempt.peer)?,
                })),
                DhtPeerRemoval::Unavailable => {
                    let replacements =
                        self.live_successor_replacements_from_active(attempt.peer, active)?;
                    let fallback = replacements.first().copied();
                    let removal = self.dht.remove_unavailable(attempt.peer, replacements)?;
                    Ok(Some(PeerRemovalOutcome { fallback, removal }))
                }
            })
            .await?
            .retired()
        else {
            return Ok(None);
        };

        tracing::info!(
            peer = %attempt.peer,
            generation = attempt.generation,
            fallback = ?outcome.fallback(),
            removal = ?outcome.removal(),
            "removed peer from DHT"
        );
        if let Some(connection) = connection {
            self.close_connection_for_disconnect(&connection).await?;
        }
        Ok(Some(outcome))
    }

    /// Retire `attempt` unless the local topology references its peer.
    ///
    /// The reference check and the retirement run inside one lifecycle
    /// critical section; every transition that writes a topology slot also
    /// holds that boundary, so no reference can appear between the check and
    /// `dht.remove`. The physical close runs afterwards and its failure is
    /// logged rather than returned: the record slot is already free, which is
    /// what the caller asked for.
    pub(super) async fn retire_unless_referenced(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<RetirementOutcome<()>> {
        let connection = self.get_raw_connection(attempt.peer);
        let outcome = self
            .retire_announced_if(attempt, |_| {
                if self
                    .dht
                    .with_topology_state(|topology| topology.references(attempt.peer))?
                {
                    return Ok(None);
                }
                self.dht.remove(attempt.peer)?;
                Ok(Some(()))
            })
            .await?;
        if let (true, Some(connection)) = (outcome.is_retired(), connection) {
            if let Err(error) = self.close_connection_for_disconnect(&connection).await {
                tracing::warn!(
                    peer = %attempt.peer,
                    generation = attempt.generation,
                    error = ?error,
                    "evicted connection failed to close after retirement"
                );
            }
        }
        Ok(outcome)
    }

    async fn close_connection_for_disconnect(&self, connection: &SwarmConnection) -> Result<()> {
        let close = async {
            self.transport
                .close_connection_if_current(&connection.connection)
                .await
                .map(|_| ())
                .map_err(Error::Transport)
        };
        if !await_bounded_connection_close(close).await? {
            tracing::warn!(
                peer = %connection.peer,
                timeout_ms = DATA_CHANNEL_CLOSE_TIMEOUT.as_millis(),
                "timed out cleaning up retired transport connection"
            );
        }
        Ok(())
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_connection_state_without_callback(
        &self,
        peer: Did,
        state: WebrtcConnectionState,
    ) -> Result<()> {
        let Some(conn) = self.get_raw_connection(peer) else {
            return Err(Error::SwarmMissTransport(peer));
        };
        conn.connection
            .force_dummy_webrtc_connection_state_without_callback(state)
            .map_err(Error::Transport)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_data_channel_open_without_callback(
        &self,
        peer: Did,
        open: Option<bool>,
    ) -> Result<()> {
        let Some(conn) = self.get_raw_connection(peer) else {
            return Err(Error::SwarmMissTransport(peer));
        };
        conn.connection
            .force_dummy_data_channel_open_without_callback(open)
            .map_err(Error::Transport)
    }

    pub(crate) async fn get_and_check_send_connection(
        &self,
        peer: Did,
    ) -> Option<AdmittedConnection> {
        self.get_and_check_send_connection_with_timeout(peer, DATA_CHANNEL_OPEN_TIMEOUT)
            .await
    }

    pub(crate) async fn get_and_check_send_connection_with_timeout(
        &self,
        peer: Did,
        wait_timeout: Duration,
    ) -> Option<AdmittedConnection> {
        let admitted = self.admitted_send_connection(peer).ok().flatten()?;
        let attempt = admitted.attempt();
        let conn = admitted.connection();
        let initial_readiness = conn.readiness();
        if initial_readiness.is_terminal() {
            return None;
        }

        tracing::debug!(
            target: "rings_core::transport::data_channel",
            local = %self.dht.did,
            peer = %peer,
            state = ?initial_readiness.state(),
            readiness = initial_readiness.as_str(),
            data_channel_open = initial_readiness.data_channel_open(),
            timeout_ms = wait_timeout.as_millis(),
            "waiting for active connection data channel"
        );

        let failure = {
            let wait_for_ready = wait_for_transport_readiness(conn).fuse();
            let timeout = sleep(wait_timeout).fuse();
            pin_mut!(wait_for_ready, timeout);

            select! {
                result = wait_for_ready => result.err().map(|e| format!("transport_wait_failed: {e:?}")),
                _ = timeout => Some("data_channel_open_wait_timeout".to_string()),
            }
        };

        if let Some(reason) = failure {
            let final_readiness = conn.readiness();
            tracing::warn!(
                target: "rings_core::transport::data_channel",
                local = %self.dht.did,
                peer = %peer,
                initial_state = ?initial_readiness.state(),
                initial_readiness = initial_readiness.as_str(),
                final_state = ?final_readiness.state(),
                final_readiness = final_readiness.as_str(),
                final_data_channel_open = final_readiness.data_channel_open(),
                timeout_ms = wait_timeout.as_millis(),
                reason = %reason,
                "send connection data channel not open, will be dropped"
            );

            let disconnect_result = self.disconnect_unavailable(attempt).await;
            if let Err(e) = disconnect_result {
                tracing::error!(
                    target: "rings_core::transport::data_channel",
                    local = %self.dht.did,
                    peer = %peer,
                    reason = %reason,
                    "failed to close connection after data-channel wait failure: {e:?}"
                );
            }

            return None;
        };

        tracing::debug!(
            target: "rings_core::transport::data_channel",
            local = %self.dht.did,
            peer = %peer,
            readiness = conn.readiness().as_str(),
            "active connection data channel is open"
        );

        admitted.ensure_current().ok()?;
        Some(admitted)
    }
}

async fn wait_for_transport_readiness(connection: &SwarmConnection) -> Result<()> {
    connection
        .connection
        .webrtc_wait_for_data_channel_open()
        .await
        .map_err(Error::Transport)?;
    loop {
        let readiness = connection.readiness();
        if readiness.can_make_progress() {
            return Ok(());
        }
        if readiness.is_terminal() {
            return readiness.ensure_can_make_progress();
        }
        sleep(TRANSPORT_READINESS_POLL_INTERVAL).await;
    }
}
