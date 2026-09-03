//! Stabilization run daemons to maintain dht.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::transport::WebrtcConnectionState;

pub use self::storage_repair::StorageRepairOutcome;
use crate::dht::successor::SuccessorReader;
use crate::dht::types::CorrectChord;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::message::handlers::inbox::drain_inbox;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::PeerLivenessProbe;
use crate::message::QueryForTopoInfoSend;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::TransportReadiness;
use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;
use crate::swarm::transport::TRACKED_PAYLOAD_COMPLETION_BOUND;
use crate::utils::get_epoch_ms_i64;
use crate::utils::sleep;
use crate::utils::Instant;

const STABILIZATION_STEP_TIMEOUT: Duration =
    TRACKED_PAYLOAD_COMPLETION_BOUND.saturating_add(Duration::from_secs(1));
const STABILIZATION_STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a transport may stay in a non-productive state before it is
/// reclaimed: disconnected here, unreferenced under admission pressure in
/// `swarm::transport::retention`.
pub(crate) const DISCONNECTED_CONNECTION_GRACE_MS: i64 = 30_000;
/// Run one repair delivery per maintenance phase. Every frame has a bounded
/// data-channel admission wait, and tracked completion prevents a chunk tail
/// from escaping into the following topology phase.
pub(crate) const STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP: usize = 1;
pub(crate) const STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS: i64 = 30_000;

#[derive(Clone, Copy, Debug)]
enum TopologyPeerRemovalReason {
    NoAdmittedTransport,
    MissingTransportObject,
    SendTerminal,
    TerminalTransport(WebrtcConnectionState),
    DataChannelNotOpen(WebrtcConnectionState),
    DisconnectedGraceElapsed {
        disconnected_for_ms: i64,
        grace_ms: i64,
    },
    DisconnectedSuccessorFailover {
        disconnected_for_ms: i64,
    },
    DisconnectedTopologyPrune {
        disconnected_for_ms: i64,
    },
    UnansweredLivenessProbe {
        unanswered_for_ms: i64,
        timeout_ms: i64,
    },
}

#[derive(Clone, Copy)]
struct AdmittedPeerState {
    attempt: PendingConnectionAttempt,
    readiness: Option<TransportReadiness>,
    send_terminal: bool,
}

#[derive(Clone, Copy)]
struct TopologyPeerRemoval {
    attempt: Option<PendingConnectionAttempt>,
    reason: TopologyPeerRemovalReason,
}

impl TopologyPeerRemovalReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoAdmittedTransport => "no_admitted_transport",
            Self::MissingTransportObject => "missing_transport_object",
            Self::SendTerminal => "send_terminal",
            Self::TerminalTransport(_) => "terminal_transport",
            Self::DataChannelNotOpen(_) => "data_channel_not_open",
            Self::DisconnectedGraceElapsed { .. } => "disconnected_grace_elapsed",
            Self::DisconnectedSuccessorFailover { .. } => "disconnected_successor_failover",
            Self::DisconnectedTopologyPrune { .. } => "disconnected_topology_prune",
            Self::UnansweredLivenessProbe { .. } => "unanswered_liveness_probe",
        }
    }

    const fn transport_state(self) -> Option<WebrtcConnectionState> {
        match self {
            Self::TerminalTransport(state) | Self::DataChannelNotOpen(state) => Some(state),
            _ => None,
        }
    }

    const fn disconnected_for_ms(self) -> Option<i64> {
        match self {
            Self::DisconnectedGraceElapsed {
                disconnected_for_ms,
                ..
            }
            | Self::DisconnectedSuccessorFailover {
                disconnected_for_ms,
            }
            | Self::DisconnectedTopologyPrune {
                disconnected_for_ms,
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

    const fn should_disconnect_transport(self) -> bool {
        !matches!(
            self,
            Self::NoAdmittedTransport | Self::DisconnectedTopologyPrune { .. }
        )
    }
}

enum StepDeadline<T> {
    Completed(Result<T>),
    TimedOut,
}

async fn await_step_deadline<F, T>(future: F, timeout: Duration) -> StepDeadline<T>
where F: Future<Output = Result<T>> {
    let future = future.fuse();
    let timer = sleep(timeout).fuse();
    pin_mut!(future, timer);
    select! {
        result = future => StepDeadline::Completed(result),
        _ = timer => StepDeadline::TimedOut,
    }
}

/// The stabilization runner.
#[derive(Clone)]
pub struct Stabilizer {
    transport: Arc<SwarmTransport>,
    dht: Arc<PeerRing>,
    /// The application the drained relay inbox is delivered to.
    swarm_callback: SharedSwarmCallback,
}

impl Stabilizer {
    /// Create a new stabilization runner delivering drained inbox messages to `swarm_callback`.
    pub fn new(transport: Arc<SwarmTransport>, swarm_callback: SharedSwarmCallback) -> Self {
        let dht = transport.dht.clone();
        Self {
            transport,
            dht,
            swarm_callback,
        }
    }

    /// Run stabilization once.
    pub async fn stabilize(&self) -> Result<()> {
        self.stabilize_with_step_timeout(STABILIZATION_STEP_TIMEOUT)
            .await
    }

    pub(crate) async fn stabilize_with_step_timeout(&self, timeout: Duration) -> Result<()> {
        self.stabilize_topology_with_step_timeout(timeout).await;
        self.run_step(
            "drain_inbox",
            timeout,
            drain_inbox(self.transport.clone(), &self.swarm_callback),
        )
        .await;
        self.transport.claim_storage_repair();
        let repair_outcome = self
            .run_step("repair_storage", timeout, self.repair_storage())
            .await;
        if !matches!(repair_outcome, Some(StorageRepairOutcome::Complete)) {
            self.transport.request_storage_repair();
        }
        Ok(())
    }

    async fn stabilize_topology_with_step_timeout(&self, timeout: Duration) {
        self.run_step(
            "clean_unavailable_connections",
            timeout,
            self.clean_unavailable_connections(),
        )
        .await;
        self.run_step("notify_predecessor", timeout, self.notify_predecessor())
            .await;
        self.run_step("fix_fingers", timeout, self.fix_fingers())
            .await;
        self.run_step("probe_peer_liveness", timeout, self.probe_peer_liveness())
            .await;
        // Default HMCC/Zave stabilization path. The pure operation is specified
        // as `CorrectStabilize` in tests/default/test_dht_convergence.rs.
        self.run_step("correct_stabilize", timeout, self.correct_stabilize())
            .await;
    }

    async fn run_step<F, T>(&self, step: &'static str, timeout: Duration, future: F) -> Option<T>
    where F: Future<Output = Result<T>> {
        let started_at = Instant::now();
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            step,
            timeout_ms = timeout.as_millis(),
            "STABILIZATION step start"
        );

        let result = match await_step_deadline(future, timeout).await {
            StepDeadline::Completed(result) => result,
            StepDeadline::TimedOut => {
                self.log_step_timeout(step, timeout, elapsed_since(started_at));
                return None;
            }
        };

        match result {
            Ok(output) => {
                let elapsed_ms = elapsed_since(started_at);
                if u128::try_from(elapsed_ms).unwrap_or(0) > timeout.as_millis() {
                    self.log_step_timeout(step, timeout, elapsed_ms);
                }
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    step,
                    elapsed_ms,
                    "STABILIZATION step end"
                );
                Some(output)
            }
            Err(e) => {
                tracing::error!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    step,
                    error = ?e,
                    "STABILIZATION step failed"
                );
                None
            }
        }
    }

    fn log_step_timeout(&self, step: &'static str, timeout: Duration, elapsed_ms: i64) {
        let topology = TopoInfo::try_from(self.dht.as_ref()).ok();
        let mut connections: Vec<(Did, WebrtcConnectionState)> = self
            .transport
            .admitted_connections()
            .into_iter()
            .map(|(attempt, conn)| (attempt.peer(), conn.webrtc_connection_state()))
            .collect();
        connections.sort_by_key(|(did, _)| *did);

        tracing::warn!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            step,
            timeout_ms = timeout.as_millis(),
            elapsed_ms,
            reason = "stabilization_step_overran_deadline",
            topology = ?topology,
            connections = ?connections,
            "STABILIZATION step exceeded timeout"
        );
    }

    /// Clean unavailable connections in transport.
    ///
    /// State relation:
    /// - `TopologyPeer(n, p)` iff `p` appears in `n`'s successor list,
    ///   predecessor slot, or finger table.
    /// - `Routable(n, p)` iff `p` has an admitted local transport with a stable
    ///   readiness observation in `Ready = ({Connecting, Connected}, Open)`.
    /// - `Evictable(n, p)` iff `p` has no admitted transport, has no raw
    ///   connection object, is terminal, is `Connected` with a data channel that
    ///   is not open, is the disconnected successor head while a live
    ///   successor-tail or finger fallback exists, stayed disconnected past
    ///   grace, left a liveness probe unanswered past its deadline, or reached
    ///   the local failure-evidence limit, including an admitted connection
    ///   explicitly terminalized after an irrevocable send or delivery failure.
    /// - `PrunableTopologyPeer(n, p)` iff `p` is disconnected and appears only
    ///   in non-head topology slots. These slots are hints, so they are removed
    ///   from local DHT state immediately while the transport is allowed to
    ///   recover until the disconnected grace elapses.
    ///
    /// Post: after this step returns `Ok`, every observed local
    /// `TopologyPeer(n, p) ∪ AdmittedPeer(n, p)` that was `Evictable(n, p)` at
    /// snapshot time and still owns the same active transport evidence has been
    /// removed through `PeerRing::remove`, so successor, predecessor, and finger
    /// state are cleaned together. Evidence superseded by a newer connection is
    /// a successful no-op that preserves the replacement and its topology.
    pub async fn clean_unavailable_connections(&self) -> Result<()> {
        self.transport.expire_pending_connections().await?;
        let admitted_states = self.admitted_connection_states()?;
        let mut candidates = self.dht.topology_state()?.referenced_peers();
        candidates.extend(self.transport.admitted_connection_ids());
        let now_ms = get_epoch_ms_i64();

        for did in candidates {
            if let Some(removal) = self
                .topology_peer_removal_reason(did, admitted_states.get(&did).copied(), now_ms)
                .await?
            {
                self.remove_unavailable_peer(did, removal).await?;
            }
        }

        Ok(())
    }

    fn admitted_connection_states(&self) -> Result<BTreeMap<Did, AdmittedPeerState>> {
        self.transport
            .admitted_connection_snapshots()?
            .into_iter()
            .map(|(attempt, connection)| {
                let readiness = connection.as_ref().map(|connection| connection.readiness());
                let send_terminal = self.transport.is_send_terminal_attempt(attempt)?;
                Ok((attempt.peer(), AdmittedPeerState {
                    attempt,
                    readiness,
                    send_terminal,
                }))
            })
            .collect()
    }

    async fn topology_peer_removal_reason(
        &self,
        did: Did,
        admitted: Option<AdmittedPeerState>,
        now_ms: i64,
    ) -> Result<Option<TopologyPeerRemoval>> {
        let Some(admitted) = admitted else {
            return Ok(Some(TopologyPeerRemoval {
                attempt: None,
                reason: TopologyPeerRemovalReason::NoAdmittedTransport,
            }));
        };
        let removal = |reason| {
            Some(TopologyPeerRemoval {
                attempt: Some(admitted.attempt),
                reason,
            })
        };
        if admitted.send_terminal {
            return Ok(removal(TopologyPeerRemovalReason::SendTerminal));
        }
        let Some(readiness) = admitted.readiness else {
            return Ok(removal(TopologyPeerRemovalReason::MissingTransportObject));
        };
        let state = readiness.state();

        if readiness.is_terminal() {
            return Ok(removal(TopologyPeerRemovalReason::TerminalTransport(state)));
        }

        if matches!(state, WebrtcConnectionState::Connected) && !readiness.data_channel_open() {
            return Ok(removal(TopologyPeerRemovalReason::DataChannelNotOpen(
                state,
            )));
        }

        if let Some(expiry) = self
            .transport
            .peer_liveness_expiry(admitted.attempt, now_ms)?
        {
            return Ok(removal(
                TopologyPeerRemovalReason::UnansweredLivenessProbe {
                    unanswered_for_ms: expiry.unanswered_for_ms,
                    timeout_ms: expiry.timeout_ms,
                },
            ));
        }

        if matches!(state, WebrtcConnectionState::Disconnected) {
            if let Some(reason) = self
                .disconnected_peer_removal_reason(did, admitted, now_ms)
                .await?
            {
                return Ok(removal(reason));
            }
        } else {
            self.transport.clear_peer_disconnected(admitted.attempt);
        }

        Ok(None)
    }

    async fn disconnected_peer_removal_reason(
        &self,
        did: Did,
        admitted: AdmittedPeerState,
        now_ms: i64,
    ) -> Result<Option<TopologyPeerRemovalReason>> {
        let disconnected_for_ms = if let Some(disconnected_since_ms) = self
            .transport
            .peer_disconnected_since_attempt_ms(admitted.attempt)
        {
            now_ms.saturating_sub(disconnected_since_ms)
        } else {
            self.transport
                .record_peer_disconnected(admitted.attempt)
                .await;
            tracing::warn!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                "STABILIZATION clean_unavailable observed disconnected peer without prior callback"
            );
            0
        };
        if self.transport.live_successor_fallback(did)?.is_some() {
            return Ok(Some(
                TopologyPeerRemovalReason::DisconnectedSuccessorFailover {
                    disconnected_for_ms,
                },
            ));
        }
        if self.disconnected_topology_prune_candidate(did)? {
            return Ok(Some(TopologyPeerRemovalReason::DisconnectedTopologyPrune {
                disconnected_for_ms,
            }));
        }
        Ok(
            (disconnected_for_ms >= DISCONNECTED_CONNECTION_GRACE_MS).then_some(
                TopologyPeerRemovalReason::DisconnectedGraceElapsed {
                    disconnected_for_ms,
                    grace_ms: DISCONNECTED_CONNECTION_GRACE_MS,
                },
            ),
        )
    }

    /// `PrunableTopologyPeer(n, p)`: referenced by a non-head slot only.
    fn disconnected_topology_prune_candidate(&self, peer: Did) -> Result<bool> {
        self.dht.with_topology_state(|topology| {
            topology.references(peer) && topology.successors.first().copied() != Some(peer)
        })
    }

    async fn remove_unavailable_peer(&self, did: Did, removal: TopologyPeerRemoval) -> Result<()> {
        let reason = removal.reason;
        let should_repair = self.dht.peer_may_share_storage_responsibility(did)?;
        let fallback_snapshot = self.transport.live_successor_fallback(did)?;
        tracing::info!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            peer = %did,
            reason = reason.as_str(),
            state = ?reason.transport_state(),
            disconnected_for_ms = ?reason.disconnected_for_ms(),
            disconnected_grace_ms = ?reason.disconnected_grace_ms(),
            fallback = ?fallback_snapshot,
            liveness_unanswered_for_ms = ?reason.liveness_unanswered_for_ms(),
            liveness_timeout_ms = ?reason.liveness_timeout_ms(),
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
            let outcome = match removal.attempt {
                Some(attempt) => self.transport.disconnect_unavailable(attempt).await?,
                None => None,
            };
            let Some(outcome) = outcome else {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %did,
                    reason = reason.as_str(),
                    "STABILIZATION clean_unavailable skipped superseded evidence"
                );
                return Ok(());
            };
            let fallback = outcome.fallback();
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                fallback = ?fallback,
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
            let Some(outcome) = self
                .transport
                .remove_unavailable_topology(did, removal.attempt)?
            else {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %did,
                    reason = reason.as_str(),
                    "STABILIZATION clean_unavailable skipped superseded topology evidence"
                );
                return Ok(());
            };
            let fallback = outcome.fallback();
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                fallback = ?fallback,
                "STABILIZATION clean_unavailable topology remove complete"
            );
        }

        if should_repair {
            self.transport.request_storage_repair();
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                "STABILIZATION clean_unavailable deferred storage repair to its scheduled phase"
            );
        }

        Ok(())
    }

    async fn probe_peer_liveness(&self) -> Result<()> {
        let now_ms = get_epoch_ms_i64();
        let candidates = self.transport.liveness_probe_candidates(now_ms)?;
        for attempt in candidates {
            let peer = attempt.peer();
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
                        .record_peer_liveness_probe_sent(attempt, now_ms)?;
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %tx_id,
                        "STABILIZATION peer liveness probe send complete"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        peer = %peer,
                        state = ?state,
                        error = ?error,
                        records_peer_failure = error.records_peer_send_failure(),
                        "STABILIZATION peer liveness probe send failed"
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn probe_peer_liveness_for_simulation(&self) -> Result<()> {
        self.probe_peer_liveness().await
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
                    MessagePayload::new_send(msg.clone(), self.transport.message_signer(), s, s)?;
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
                        self.transport.message_signer(),
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

fn elapsed_since(started_at: Instant) -> i64 {
    i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX)
}

mod maintenance;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::maintenance_phase_trace_for_test;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::reset_maintenance_phase_trace_for_test;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::MaintenancePhaseEvent;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::MaintenancePhaseKind;
mod storage_repair;

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use super::*;

    struct DropWitness(Arc<AtomicBool>);

    impl Drop for DropWitness {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    async fn test_step_deadline_drops_work_that_does_not_complete() {
        let dropped = Arc::new(AtomicBool::new(false));
        let witness = dropped.clone();
        let future = async move {
            let _witness = DropWitness(witness);
            futures::future::pending::<()>().await;
            Ok(())
        };

        let result = await_step_deadline(future, Duration::from_millis(1)).await;

        assert!(matches!(result, StepDeadline::TimedOut));
        assert!(dropped.load(Ordering::Acquire));
    }
}
