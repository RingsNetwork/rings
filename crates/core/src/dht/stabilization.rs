//! Stabilization run daemons to maintain dht.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::dht::successor::SuccessorReader;
use crate::dht::types::ChordStorageRepair;
use crate::dht::types::CorrectChord;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageSyncDelivery;
use crate::dht::StorageSyncDestination;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::measure::PeerMeasurement;
use crate::measure::PeerQualityThresholds;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::PeerLivenessProbe;
use crate::message::QueryForTopoInfoSend;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;
use crate::utils::get_epoch_ms_i64;
use crate::utils::sleep;

const STABILIZATION_STEP_TIMEOUT: Duration = Duration::from_secs(30);
const DISCONNECTED_CONNECTION_GRACE_MS: i64 = 30_000;
pub(crate) const STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP: usize = 64;
pub(crate) const STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS: i64 = 30_000;
const DHT_TOPOLOGY_EVICTION_THRESHOLDS: PeerQualityThresholds =
    PeerQualityThresholds::new(3, 10, 10);

#[derive(Clone, Copy, Debug)]
enum TopologyPeerRemovalReason {
    NoAdmittedTransport,
    MissingTransportObject,
    TerminalTransport(WebrtcConnectionState),
    DisconnectedGraceElapsed {
        disconnected_for_ms: i64,
        grace_ms: i64,
    },
    UnansweredLivenessProbe {
        unanswered_for_ms: i64,
        timeout_ms: i64,
    },
    LocalFailureLimit(PeerMeasurement),
}

#[derive(Clone, Copy, Debug)]
enum StorageRepairDeferReason {
    MissingNextHop,
    NextHopNotAdmitted,
    PhysicalOwnerNotAdmitted,
    NextHopFresh { connected_for_ms: i64 },
    PhysicalOwnerFresh { connected_for_ms: i64 },
}

impl StorageRepairDeferReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingNextHop => "missing_next_hop",
            Self::NextHopNotAdmitted => "next_hop_not_admitted",
            Self::PhysicalOwnerNotAdmitted => "physical_owner_not_admitted",
            Self::NextHopFresh { .. } => "next_hop_fresh",
            Self::PhysicalOwnerFresh { .. } => "physical_owner_fresh",
        }
    }

    const fn connected_for_ms(self) -> Option<i64> {
        match self {
            Self::NextHopFresh { connected_for_ms }
            | Self::PhysicalOwnerFresh { connected_for_ms } => Some(connected_for_ms),
            _ => None,
        }
    }
}

impl TopologyPeerRemovalReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoAdmittedTransport => "no_admitted_transport",
            Self::MissingTransportObject => "missing_transport_object",
            Self::TerminalTransport(_) => "terminal_transport",
            Self::DisconnectedGraceElapsed { .. } => "disconnected_grace_elapsed",
            Self::UnansweredLivenessProbe { .. } => "unanswered_liveness_probe",
            Self::LocalFailureLimit(_) => "local_failure_limit",
        }
    }

    const fn transport_state(self) -> Option<WebrtcConnectionState> {
        match self {
            Self::TerminalTransport(state) => Some(state),
            _ => None,
        }
    }

    const fn disconnected_for_ms(self) -> Option<i64> {
        match self {
            Self::DisconnectedGraceElapsed {
                disconnected_for_ms,
                ..
            } => Some(disconnected_for_ms),
            _ => None,
        }
    }

    const fn disconnected_grace_ms(self) -> Option<i64> {
        match self {
            Self::DisconnectedGraceElapsed { grace_ms, .. } => Some(grace_ms),
            _ => None,
        }
    }

    const fn liveness_unanswered_for_ms(self) -> Option<i64> {
        match self {
            Self::UnansweredLivenessProbe {
                unanswered_for_ms, ..
            } => Some(unanswered_for_ms),
            _ => None,
        }
    }

    const fn liveness_timeout_ms(self) -> Option<i64> {
        match self {
            Self::UnansweredLivenessProbe { timeout_ms, .. } => Some(timeout_ms),
            _ => None,
        }
    }

    const fn measurement(self) -> Option<PeerMeasurement> {
        match self {
            Self::LocalFailureLimit(measurement) => Some(measurement),
            _ => None,
        }
    }

    const fn should_disconnect_transport(self) -> bool {
        !matches!(self, Self::NoAdmittedTransport)
    }
}

const fn is_terminal_transport_state(state: WebrtcConnectionState) -> bool {
    matches!(
        state,
        WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
    )
}

/// The stabilization runner.
#[derive(Clone)]
pub struct Stabilizer {
    transport: Arc<SwarmTransport>,
    dht: Arc<PeerRing>,
    storage_repair_cursor: Arc<Mutex<usize>>,
}

impl Stabilizer {
    /// Create a new stabilization runner.
    pub fn new(transport: Arc<SwarmTransport>) -> Self {
        let dht = transport.dht.clone();
        Self {
            transport,
            dht,
            storage_repair_cursor: Arc::new(Mutex::new(0)),
        }
    }

    /// Run stabilization once.
    pub async fn stabilize(&self) -> Result<()> {
        self.stabilize_with_step_timeout(STABILIZATION_STEP_TIMEOUT)
            .await
    }

    pub(crate) async fn stabilize_with_step_timeout(&self, timeout: Duration) -> Result<()> {
        self.run_step("notify_predecessor", timeout, self.notify_predecessor())
            .await;
        self.run_step("fix_fingers", timeout, self.fix_fingers())
            .await;
        self.run_step("probe_peer_liveness", timeout, self.probe_peer_liveness())
            .await;
        self.run_step(
            "clean_unavailable_connections",
            timeout,
            self.clean_unavailable_connections(),
        )
        .await;
        // Default HMCC/Zave stabilization path. The pure operation is specified
        // as `CorrectStabilize` in tests/default/test_dht_convergence.rs.
        self.run_step("correct_stabilize", timeout, self.correct_stabilize())
            .await;
        self.run_step("repair_storage", timeout, self.repair_storage())
            .await;
        Ok(())
    }

    async fn run_step<F>(&self, step: &'static str, timeout: Duration, future: F)
    where F: Future<Output = Result<()>> {
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            step,
            timeout_ms = timeout.as_millis(),
            "STABILIZATION step start"
        );

        let result = {
            let future = future.fuse();
            let timer = sleep(timeout).fuse();
            pin_mut!(future, timer);

            select! {
                result = future => Some(result),
                _ = timer => None,
            }
        };

        match result {
            Some(Ok(())) => {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    step,
                    "STABILIZATION step end"
                );
            }
            Some(Err(e)) => {
                tracing::error!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    step,
                    error = ?e,
                    "STABILIZATION step failed"
                );
            }
            None => self.log_step_timeout(step, timeout),
        }
    }

    fn log_step_timeout(&self, step: &'static str, timeout: Duration) {
        let topology = TopoInfo::try_from(self.dht.as_ref()).ok();
        let mut connections: Vec<(Did, WebrtcConnectionState)> = self
            .transport
            .admitted_connections()
            .into_iter()
            .map(|(did, conn)| (did, conn.webrtc_connection_state()))
            .collect();
        connections.sort_by_key(|(did, _)| *did);

        tracing::warn!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            step,
            timeout_ms = timeout.as_millis(),
            reason = "stabilization_step_future_pending",
            topology = ?topology,
            connections = ?connections,
            "STABILIZATION step timed out"
        );
    }

    async fn handle_storage_repair_action(&self, act: PeerRingAction) -> Result<()> {
        let deliveries = self.storage_repair_window(act.coalesced_storage_sync_deliveries()?);
        if deliveries.is_empty() {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                "STABILIZATION storage repair has no deliveries"
            );
            return Ok(());
        }

        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            deliveries = deliveries.len(),
            "STABILIZATION storage repair deliveries prepared"
        );

        let now_ms = get_epoch_ms_i64();
        let mut sent = 0usize;
        let mut deferred = 0usize;
        for delivery in deliveries {
            let msg = SyncEntriesWithSuccessor::from_delivery(delivery);
            let purpose = msg.purpose;
            let destination = msg.destination;
            let destination_did = destination.did();
            let entries = msg.data.len();
            let next_hop = self
                .dht
                .next_hop_for_storage_sync(destination)
                .ok()
                .flatten();
            let next_hop_state = next_hop
                .and_then(|did| self.transport.get_connection(did))
                .map(|conn| conn.webrtc_connection_state());

            if let Some(reason) = self.storage_repair_defer_reason(destination, next_hop, now_ms)? {
                deferred = deferred.saturating_add(1);
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    purpose = ?purpose,
                    destination = ?destination,
                    destination_did = %destination_did,
                    next_hop = ?next_hop,
                    next_hop_state = ?next_hop_state,
                    entries,
                    reason = reason.as_str(),
                    connected_for_ms = ?reason.connected_for_ms(),
                    grace_ms = STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS,
                    "STABILIZATION storage repair deferred"
                );
                continue;
            }

            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                purpose = ?purpose,
                destination = ?destination,
                destination_did = %destination_did,
                next_hop = ?next_hop,
                next_hop_state = ?next_hop_state,
                entries,
                "STABILIZATION storage repair send start"
            );

            match self.transport.send_storage_sync(msg).await {
                Ok(tx_id) => tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    tx_id = %tx_id,
                    purpose = ?purpose,
                    destination = ?destination,
                    destination_did = %destination_did,
                    next_hop = ?next_hop,
                    entries,
                        "STABILIZATION storage repair send complete"
                ),
                Err(e) => {
                    tracing::error!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        purpose = ?purpose,
                        destination = ?destination,
                        destination_did = %destination_did,
                        next_hop = ?next_hop,
                        next_hop_state = ?next_hop_state,
                        entries,
                        error = ?e,
                        "STABILIZATION storage repair send failed"
                    );
                    return Err(e);
                }
            }
            sent = sent.saturating_add(1);
        }

        if deferred > 0 {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                sent,
                deferred,
                "STABILIZATION storage repair deliveries finished with deferrals"
            );
        }
        Ok(())
    }

    fn storage_repair_window(
        &self,
        mut deliveries: Vec<StorageSyncDelivery>,
    ) -> Vec<StorageSyncDelivery> {
        let total = deliveries.len();
        if total <= STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP {
            return deliveries;
        }

        let start = match self.storage_repair_cursor.lock() {
            Ok(mut cursor) => {
                let start = *cursor % total;
                *cursor = (start + STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP) % total;
                start
            }
            Err(_) => {
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    "STABILIZATION storage repair cursor lock failed"
                );
                0
            }
        };
        deliveries.rotate_left(start);
        deliveries.truncate(STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP);
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            total_deliveries = total,
            selected_deliveries = deliveries.len(),
            start,
            "STABILIZATION storage repair delivery window selected"
        );
        deliveries
    }

    fn storage_repair_defer_reason(
        &self,
        destination: StorageSyncDestination,
        next_hop: Option<Did>,
        now_ms: i64,
    ) -> Result<Option<StorageRepairDeferReason>> {
        let Some(next_hop) = next_hop else {
            return Ok(Some(StorageRepairDeferReason::MissingNextHop));
        };
        if !self.transport.is_admitted_connection(next_hop) {
            return Ok(Some(StorageRepairDeferReason::NextHopNotAdmitted));
        }
        if let Some(connected_for_ms) = self.peer_connected_for_ms(next_hop, now_ms) {
            if connected_for_ms < STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS {
                return Ok(Some(StorageRepairDeferReason::NextHopFresh {
                    connected_for_ms,
                }));
            }
        }

        if let Some(owner) = self.dht.observed_storage_sync_physical_owner(destination)? {
            if !self.transport.is_admitted_connection(owner) {
                return Ok(Some(StorageRepairDeferReason::PhysicalOwnerNotAdmitted));
            }
            if let Some(connected_for_ms) = self.peer_connected_for_ms(owner, now_ms) {
                if connected_for_ms < STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS {
                    return Ok(Some(StorageRepairDeferReason::PhysicalOwnerFresh {
                        connected_for_ms,
                    }));
                }
            }
        }

        Ok(None)
    }

    fn peer_connected_for_ms(&self, peer: Did, now_ms: i64) -> Option<i64> {
        match self.transport.peer_connected_for_ms(peer, now_ms) {
            Ok(age) => age,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    error = %error,
                    "STABILIZATION storage repair connection age check failed"
                );
                None
            }
        }
    }

    /// Republish locally-held entries to their current affine owners.
    pub async fn repair_storage(&self) -> Result<()> {
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            redundancy = self.transport.storage_redundancy(),
            "STABILIZATION repair_storage republish start"
        );
        let action = self
            .dht
            .republish_local_entries(self.transport.storage_redundancy())
            .await?;
        let (action_kind, action_count) = match &action {
            PeerRingAction::None => ("None", 0),
            PeerRingAction::Some(_) => ("Some", 1),
            PeerRingAction::SomeEntry(_) => ("SomeEntry", 1),
            PeerRingAction::EntryMisses(misses) => ("EntryMisses", misses.len()),
            PeerRingAction::RemoteAction(_, _) => ("RemoteAction", 1),
            PeerRingAction::MultiActions(actions) => ("MultiActions", actions.len()),
        };
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            action_kind,
            action_count,
            "STABILIZATION repair_storage republish action prepared"
        );
        self.handle_storage_repair_action(action).await?;
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            "STABILIZATION repair_storage republish complete"
        );
        Ok(())
    }

    /// Clean unavailable connections in transport.
    ///
    /// State relation:
    /// - `TopologyPeer(n, p)` iff `p` appears in `n`'s successor list,
    ///   predecessor slot, or finger table.
    /// - `Routable(n, p)` iff `p` has an admitted local transport whose raw
    ///   connection object is non-terminal.
    /// - `Evictable(n, p)` iff `p` has no admitted transport, has no raw
    ///   connection object, is terminal, stayed disconnected past grace, or has
    ///   reached the local failure-evidence limit.
    ///
    /// Post: after this step returns `Ok`, every observed local
    /// `TopologyPeer(n, p) ∪ AdmittedPeer(n, p)` that was `Evictable(n, p)` at
    /// the step's snapshot time has been removed through `PeerRing::remove`, so
    /// successor, predecessor, and finger state are cleaned together.
    pub async fn clean_unavailable_connections(&self) -> Result<()> {
        self.transport.expire_pending_connections().await?;
        let admitted_states = self.admitted_connection_states();
        let topology_peers = self.dht_topology_peers()?;
        let mut candidates = topology_peers;
        candidates.extend(self.transport.admitted_connection_ids());
        let now_ms = get_epoch_ms_i64();

        for did in candidates {
            let admitted = self.transport.is_admitted_connection(did);
            let transport_state = admitted_states.get(&did).copied();
            if let Some(reason) = self
                .topology_peer_removal_reason(did, admitted, transport_state, now_ms)
                .await
            {
                self.remove_unavailable_peer(did, reason).await?;
            }
        }

        Ok(())
    }

    fn admitted_connection_states(&self) -> BTreeMap<Did, WebrtcConnectionState> {
        self.transport
            .admitted_connections()
            .into_iter()
            .map(|(did, conn)| (did, conn.webrtc_connection_state()))
            .collect()
    }

    fn dht_topology_peers(&self) -> Result<BTreeSet<Did>> {
        let mut peers = BTreeSet::new();

        for did in self.dht.successors().list()? {
            if did != self.dht.did {
                peers.insert(did);
            }
        }

        if let Some(predecessor) = *self.dht.lock_predecessor()? {
            if predecessor != self.dht.did {
                peers.insert(predecessor);
            }
        }

        {
            let finger = self.dht.lock_finger()?;
            for did in finger.list().iter().flatten().copied() {
                if did != self.dht.did {
                    peers.insert(did);
                }
            }
        }

        Ok(peers)
    }

    async fn topology_peer_removal_reason(
        &self,
        did: Did,
        admitted: bool,
        transport_state: Option<WebrtcConnectionState>,
        now_ms: i64,
    ) -> Option<TopologyPeerRemovalReason> {
        if !admitted {
            return Some(TopologyPeerRemovalReason::NoAdmittedTransport);
        }

        let Some(state) = transport_state else {
            return Some(TopologyPeerRemovalReason::MissingTransportObject);
        };

        if is_terminal_transport_state(state) {
            return Some(TopologyPeerRemovalReason::TerminalTransport(state));
        }

        if let Some(measurement) = self.transport.peer_measurement(did).await {
            if measurement
                .evidence
                .reaches_failure_limit(DHT_TOPOLOGY_EVICTION_THRESHOLDS)
            {
                return Some(TopologyPeerRemovalReason::LocalFailureLimit(measurement));
            }
        }

        match self.transport.peer_liveness_expiry(did, now_ms) {
            Ok(Some(expiry)) => {
                return Some(TopologyPeerRemovalReason::UnansweredLivenessProbe {
                    unanswered_for_ms: expiry.unanswered_for_ms,
                    timeout_ms: expiry.timeout_ms,
                });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %did,
                    error = %error,
                    "STABILIZATION clean_unavailable liveness state check failed"
                );
            }
        }

        if matches!(state, WebrtcConnectionState::Disconnected) {
            if let Some(disconnected_since_ms) = self.transport.peer_disconnected_since_ms(did) {
                let disconnected_for_ms = now_ms.saturating_sub(disconnected_since_ms);
                if disconnected_for_ms >= DISCONNECTED_CONNECTION_GRACE_MS {
                    return Some(TopologyPeerRemovalReason::DisconnectedGraceElapsed {
                        disconnected_for_ms,
                        grace_ms: DISCONNECTED_CONNECTION_GRACE_MS,
                    });
                }
            }
        }

        None
    }

    async fn remove_unavailable_peer(
        &self,
        did: Did,
        reason: TopologyPeerRemovalReason,
    ) -> Result<()> {
        let should_repair = self
            .dht
            .peer_may_share_storage_responsibility(did, self.transport.storage_redundancy())
            .await?;
        tracing::info!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            peer = %did,
            reason = reason.as_str(),
            state = ?reason.transport_state(),
            disconnected_for_ms = ?reason.disconnected_for_ms(),
            disconnected_grace_ms = ?reason.disconnected_grace_ms(),
            liveness_unanswered_for_ms = ?reason.liveness_unanswered_for_ms(),
            liveness_timeout_ms = ?reason.liveness_timeout_ms(),
            measurement = ?reason.measurement(),
            should_repair,
            "STABILIZATION clean_unavailable selected peer"
        );

        if reason.should_disconnect_transport() {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                "STABILIZATION clean_unavailable disconnect start"
            );
            self.transport.disconnect(did).await?;
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                "STABILIZATION clean_unavailable disconnect complete"
            );
        } else {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                "STABILIZATION clean_unavailable topology remove start"
            );
            self.dht.remove(did)?;
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                "STABILIZATION clean_unavailable topology remove complete"
            );
        }

        if should_repair {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                "STABILIZATION clean_unavailable repair start"
            );
            self.repair_storage().await?;
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                "STABILIZATION clean_unavailable repair complete"
            );
        }

        Ok(())
    }

    async fn probe_peer_liveness(&self) -> Result<()> {
        let now_ms = get_epoch_ms_i64();
        let candidates = self.transport.liveness_probe_candidates(now_ms)?;
        for peer in candidates {
            let state = self
                .transport
                .get_connection(peer)
                .map(|conn| conn.webrtc_connection_state());
            let msg = Message::PeerLivenessProbe(PeerLivenessProbe { sent_at_ms: now_ms });
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %peer,
                state = ?state,
                idle_ms = PEER_LIVENESS_IDLE_MS,
                "STABILIZATION peer liveness probe send start"
            );
            match self.transport.send_direct_message(msg, peer).await {
                Ok(tx_id) => {
                    self.transport
                        .record_peer_liveness_probe_sent(peer, now_ms)?;
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %tx_id,
                        "STABILIZATION peer liveness probe send complete"
                    );
                }
                Err(error) => {
                    self.transport.record_peer_message_send_failed(peer).await;
                    tracing::warn!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        peer = %peer,
                        state = ?state,
                        error = ?error,
                        "STABILIZATION peer liveness probe send failed"
                    );
                }
            }
        }
        Ok(())
    }

    /// Notify predecessor, this is a DHT operation.
    pub async fn notify_predecessor(&self) -> Result<()> {
        let (successor_min, successor_list) = {
            let successor = self.dht.successors();
            (successor.min()?, successor.list()?)
        };

        let msg = Message::NotifyPredecessorSend(NotifyPredecessorSend { did: self.dht.did });
        if self.dht.did != successor_min {
            for s in successor_list {
                let payload =
                    MessagePayload::new_send(msg.clone(), self.transport.session_sk(), s, s)?;
                let tx_id = payload.transaction.tx_id;
                let target_state = self
                    .transport
                    .get_connection(s)
                    .map(|conn| conn.webrtc_connection_state());
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    successor = %s,
                    tx_id = %tx_id,
                    target_state = ?target_state,
                    "STABILIZATION notify_predecessor send start"
                );
                if let Err(e) = self.transport.send_payload(payload).await {
                    tracing::error!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        successor = %s,
                        tx_id = %tx_id,
                        target_state = ?target_state,
                        error = ?e,
                        "STABILIZATION notify_predecessor send failed"
                    );
                    return Err(e);
                }
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    successor = %s,
                    tx_id = %tx_id,
                    "STABILIZATION notify_predecessor send complete"
                );
            }
            Ok(())
        } else {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                successor = %successor_min,
                "STABILIZATION notify_predecessor skip local successor"
            );
            Ok(())
        }
    }

    /// Fix fingers from finger table, this is a DHT operation.
    async fn fix_fingers(&self) -> Result<()> {
        match self.dht.fix_fingers() {
            Ok(action) => match action {
                PeerRingAction::None => {
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        "STABILIZATION fix_fingers no remote action"
                    );
                    Ok(())
                }
                PeerRingAction::RemoteAction(
                    closest_predecessor,
                    PeerRingRemoteAction::FindSuccessorForFix {
                        did: finger_did,
                        index,
                    },
                ) => {
                    let msg = Message::FindSuccessorSend(FindSuccessorSend {
                        did: finger_did,
                        then: FindSuccessorThen::Report(
                            FindSuccessorReportHandler::FixFingerTable { index },
                        ),
                        strict: false,
                    });
                    let payload = MessagePayload::new_send(
                        msg.clone(),
                        self.transport.session_sk(),
                        closest_predecessor,
                        closest_predecessor,
                    )?;
                    let tx_id = payload.transaction.tx_id;
                    let next_hop_state = self
                        .transport
                        .get_connection(closest_predecessor)
                        .map(|conn| conn.webrtc_connection_state());
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        next_hop = %closest_predecessor,
                        next_hop_state = ?next_hop_state,
                        finger_did = %finger_did,
                        index,
                        tx_id = %tx_id,
                        "STABILIZATION fix_fingers send start"
                    );
                    if let Err(e) = self.transport.send_payload(payload).await {
                        tracing::error!(
                            target: "rings_core::dht::stabilization",
                            local = %self.dht.did,
                            next_hop = %closest_predecessor,
                            next_hop_state = ?next_hop_state,
                            finger_did = %finger_did,
                            index,
                            tx_id = %tx_id,
                            error = ?e,
                            "STABILIZATION fix_fingers send failed"
                        );
                        return Err(e);
                    }
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        next_hop = %closest_predecessor,
                        finger_did = %finger_did,
                        index,
                        tx_id = %tx_id,
                        "STABILIZATION fix_fingers send complete"
                    );
                    Ok(())
                }
                _ => {
                    tracing::error!("Invalid PeerRing Action");
                    Err(Error::PeerRingInvalidAction)
                }
            },
            Err(e) => {
                tracing::error!("{:?}", e);
                Err(e)
            }
        }
    }

    /// Call stabilization from correct chord implementation
    pub async fn correct_stabilize(&self) -> Result<()> {
        match self.dht.pre_stabilize()? {
            PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::QueryForSuccessorListAndPred,
            ) => {
                let next_hop_state = self
                    .transport
                    .get_connection(next)
                    .map(|conn| conn.webrtc_connection_state());
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    next = %next,
                    next_hop_state = ?next_hop_state,
                    "STABILIZATION correct_stabilize query start"
                );
                match self
                    .transport
                    .send_direct_message(
                        Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_stab(next)),
                        next,
                    )
                    .await
                {
                    Ok(tx_id) => tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        next = %next,
                        tx_id = %tx_id,
                        "STABILIZATION correct_stabilize query complete"
                    ),
                    Err(e) => {
                        tracing::error!(
                            target: "rings_core::dht::stabilization",
                            local = %self.dht.did,
                            next = %next,
                            next_hop_state = ?next_hop_state,
                            error = ?e,
                            "STABILIZATION correct_stabilize query failed"
                        );
                        return Err(e);
                    }
                }
            }
            action => {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    action = ?action,
                    "STABILIZATION correct_stabilize no remote query"
                );
            }
        }
        Ok(())
    }
}

mod stabilizer {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    impl Stabilizer {
        /// Run stabilization in a loop.
        pub async fn wait(self: Arc<Self>, interval: Duration) {
            self.wait_with(interval, StopToken::never()).await;
        }

        /// Run stabilization until `stop` asks this loop to exit.
        ///
        /// The token is checked between ticks and before each stabilization run.
        /// It intentionally does not cancel an in-flight stabilization future;
        /// browser IndexedDB requests are not cancellation-safe.
        pub async fn wait_with(self: Arc<Self>, interval: Duration, stop: StopToken) {
            loop {
                if stop.should_stop() {
                    return;
                }
                sleep(interval).await;
                if stop.should_stop() {
                    return;
                }
                self.stabilize()
                    .await
                    .unwrap_or_else(|e| tracing::error!("failed to stabilize {:?}", e));
            }
        }
    }
}
