//! Stabilization run daemons to maintain dht.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::ready;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use futures::stream;
use futures::StreamExt;
use futures::TryStreamExt;
use rings_transport::core::transport::WebrtcConnectionState;

pub use self::storage_repair::StorageRepairOutcome;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;
use crate::message::QueryForTopoInfoSend;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::TransportReadiness;
use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;
use crate::swarm::transport::TRACKED_PAYLOAD_COMPLETION_BOUND;
use crate::utils::get_epoch_ms_i64;
use crate::utils::sleep;
use crate::utils::Instant;

/// Selects whether a topology pass also advances finger convergence or only
/// publishes the intent for the independently paced maintenance phase.
///
/// The distinction preserves the eager semantics of direct stabilization
/// callers while preventing the long-running scheduler from coupling finger
/// lookup traffic to the topology period.
#[derive(Clone, Copy)]
enum FingerMaintenanceMode {
    /// Start revalidation and advance its first effect in the same pass.
    ///
    /// Direct callers use this mode so one explicit stabilization request keeps
    /// the pre-scheduler behavior of making immediate finger-table progress.
    Immediate,
    /// Mark a range for revalidation without issuing its lookup in this pass.
    ///
    /// The maintenance scheduler later advances that range after applying the
    /// node-specific initial jitter or failure backoff.
    Jittered,
}

/// Maximum wall-clock budget for one stabilization sub-step.
const STABILIZATION_STEP_TIMEOUT: Duration =
    TRACKED_PAYLOAD_COMPLETION_BOUND.saturating_add(Duration::from_secs(1));
/// Cooperative polling interval used while sleeping for the next maintenance phase.
const STABILIZATION_STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a transport may stay in a non-productive state before it is
/// reclaimed: disconnected here, unreferenced under admission pressure in
/// `swarm::transport::retention`.
pub(crate) const DISCONNECTED_CONNECTION_GRACE_MS: i64 = 30_000;
/// Run one repair delivery per maintenance phase. Every frame has a bounded
/// data-channel admission wait, and tracked completion prevents a chunk tail
/// from escaping into the following topology phase.
pub(crate) const STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP: usize = 1;

/// Liveness probe data after signing but before the send is recorded.
struct PreparedLivenessProbe {
    /// Transport attempt whose state owns the probe.
    attempt: PendingConnectionAttempt,
    /// Challenge registered against the outbound payload transaction.
    request: ProbeRequest,
    /// Signed probe payload ready for transport delivery.
    payload: MessagePayload,
    /// Best-effort state snapshot for diagnostics around the send.
    peer_state: Option<WebrtcConnectionState>,
}

/// Reason the stabilization cleaner decided a peer should leave local topology
/// and possibly the transport map.
#[derive(Clone, Copy, Debug)]
enum TopologyPeerRemovalReason {
    /// The peer is referenced by DHT state but no admitted transport attempt exists.
    NoAdmittedTransport,
    /// The attempt remains admitted but the connection object has already gone away.
    MissingTransportObject,
    /// The payload layer marked this attempt as permanently unable to send.
    SendTerminal,
    /// The WebRTC connection itself reached a terminal state.
    TerminalTransport(WebrtcConnectionState),
    /// WebRTC is connected, but its data channel is not usable for payloads.
    DataChannelNotOpen(WebrtcConnectionState),
    /// The peer remained disconnected longer than the configured grace period.
    DisconnectedGraceElapsed {
        /// Milliseconds elapsed since the attempt was first observed disconnected.
        disconnected_for_ms: i64,
        /// Grace period that had to elapse before full removal.
        grace_ms: i64,
    },
    /// The disconnected successor head has a live successor fallback ready.
    DisconnectedSuccessorFailover {
        /// Milliseconds elapsed since the attempt was first observed disconnected.
        disconnected_for_ms: i64,
    },
    /// The peer is disconnected and appears only in non-head topology hints.
    DisconnectedTopologyPrune {
        /// Milliseconds elapsed since the attempt was first observed disconnected.
        disconnected_for_ms: i64,
    },
    /// A registered liveness probe was not answered before its timeout.
    UnansweredLivenessProbe {
        /// Milliseconds elapsed since the liveness probe was sent.
        unanswered_for_ms: i64,
        /// Probe timeout that was exceeded.
        timeout_ms: i64,
    },
}

/// Snapshot of one admitted transport attempt used for a cleaner decision.
#[derive(Clone, Copy)]
struct AdmittedPeerState {
    /// Stable attempt identity used to avoid removing a superseding connection.
    attempt: PendingConnectionAttempt,
    /// Readiness snapshot, or `None` when the connection object is missing.
    readiness: Option<TransportReadiness>,
    /// Whether the payload layer has recorded terminal send evidence.
    send_terminal: bool,
}

/// Exact topology/transport removal selected from one cleaner snapshot.
#[derive(Clone, Copy)]
struct TopologyPeerRemoval {
    /// Transport attempt to disconnect or prune, absent for topology-only ghosts.
    attempt: Option<PendingConnectionAttempt>,
    /// Diagnostic and policy reason for the removal.
    reason: TopologyPeerRemovalReason,
}

impl TopologyPeerRemovalReason {
    /// Stable log label for this removal class.
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

    /// WebRTC state to include in logs when the reason came from readiness.
    const fn transport_state(self) -> Option<WebrtcConnectionState> {
        match self {
            Self::TerminalTransport(state) | Self::DataChannelNotOpen(state) => Some(state),
            _ => None,
        }
    }

    /// Disconnected duration carried by reasons that depend on a disconnected peer.
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

    /// Configured disconnected grace period, when that exact threshold fired.
    const fn disconnected_grace_ms(self) -> Option<i64> {
        match self {
            Self::DisconnectedGraceElapsed { grace_ms, .. } => Some(grace_ms),
            _ => None,
        }
    }

    /// Probe age carried by unanswered-liveness decisions.
    const fn liveness_unanswered_for_ms(self) -> Option<i64> {
        match self {
            Self::UnansweredLivenessProbe {
                unanswered_for_ms, ..
            } => Some(unanswered_for_ms),
            _ => None,
        }
    }

    /// Probe timeout carried by unanswered-liveness decisions.
    const fn liveness_timeout_ms(self) -> Option<i64> {
        match self {
            Self::UnansweredLivenessProbe { timeout_ms, .. } => Some(timeout_ms),
            _ => None,
        }
    }

    /// Whether this reason owns transport teardown in addition to topology removal.
    const fn should_disconnect_transport(self) -> bool {
        !matches!(
            self,
            Self::NoAdmittedTransport | Self::DisconnectedTopologyPrune { .. }
        )
    }
}

/// Result of racing one stabilization sub-step against its deadline.
enum StepDeadline<T> {
    /// The sub-step completed before the timeout future fired.
    Completed(Result<T>),
    /// The timeout future fired first and the sub-step future was dropped.
    TimedOut,
}

/// Await a sub-step until either it completes or its wall-clock deadline fires.
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
    /// The layer that owns the application, which interprets the intent to deliver the inbox.
    inbox: SharedInboxDelivery,
}

/// The one intent the storage maintenance phase emits toward the layer that owns the
/// application: deliver this node's own relay inbox. The DHT names the intent; the swarm, which
/// owns the callback and the inbound pipeline, interprets it.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub(crate) trait InboxDelivery {
    /// Deliver every witnessed element of this node's inbox to the application and retire it.
    async fn deliver_inbox(&self) -> Result<()>;
}

/// A shared interpreter of the inbox-delivery intent.
pub(crate) type SharedInboxDelivery = Arc<rings_runtime::maybe_send_sync!(dyn InboxDelivery)>;

impl Stabilizer {
    /// Create a new stabilization runner whose inbox-delivery intent `inbox` interprets.
    pub(crate) fn new(transport: Arc<SwarmTransport>, inbox: SharedInboxDelivery) -> Self {
        let dht = transport.dht.clone();
        Self {
            transport,
            dht,
            inbox,
        }
    }

    /// Run stabilization once.
    pub async fn stabilize(&self) -> Result<()> {
        self.stabilize_with_step_timeout(STABILIZATION_STEP_TIMEOUT)
            .await
    }

    /// Run one full eager stabilization pass with a caller-supplied per-step timeout.
    pub(crate) async fn stabilize_with_step_timeout(&self, timeout: Duration) -> Result<()> {
        self.stabilize_topology_with_step_timeout(timeout).await;
        self.transport.claim_storage_repair();
        self.maintain_storage_with_step_timeout(timeout).await;
        Ok(())
    }

    /// Run topology maintenance with the eager finger-maintenance behavior.
    async fn stabilize_topology_with_step_timeout(&self, timeout: Duration) {
        self.stabilize_topology_with_finger_mode(timeout, FingerMaintenanceMode::Immediate)
            .await;
    }

    /// Run the topology portion of scheduled maintenance without coupling a
    /// finger lookup to the topology deadline.
    ///
    /// The pass may mark one range as pending, but the scheduler owns the later
    /// call that advances it. Each topology sub-step still has the supplied
    /// deadline and logs its own failure without aborting subsequent steps.
    async fn stabilize_scheduled_topology_with_step_timeout(&self, timeout: Duration) {
        self.stabilize_topology_with_finger_mode(timeout, FingerMaintenanceMode::Jittered)
            .await;
    }

    /// Execute the ordered topology sub-steps under a selected finger policy.
    ///
    /// Cleaning runs before finger maintenance; liveness probing and Chord
    /// stabilization run afterward. The completed topology report notifies
    /// the selected head. [`Self::run_step`]
    /// contains errors and timeouts per sub-step, so one failed effect cannot
    /// prevent the remaining topology obligations from being attempted.
    async fn stabilize_topology_with_finger_mode(
        &self,
        timeout: Duration,
        finger_mode: FingerMaintenanceMode,
    ) {
        self.run_step(
            "clean_unavailable_connections",
            timeout,
            self.clean_unavailable_connections(),
        )
        .await;
        match finger_mode {
            FingerMaintenanceMode::Immediate => {
                self.run_step("fix_fingers", timeout, self.fix_fingers())
                    .await;
            }
            FingerMaintenanceMode::Jittered => {
                self.run_step(
                    "schedule_finger_revalidation",
                    timeout,
                    self.begin_finger_revalidation(),
                )
                .await;
            }
        }
        self.run_step("probe_peer_liveness", timeout, self.probe_peer_liveness())
            .await;
        // Default HMCC/Zave stabilization path. The pure operation is specified
        // as `CorrectStabilize` in tests/default/test_dht_convergence.rs.
        self.run_step("correct_stabilize", timeout, self.correct_stabilize())
            .await;
    }

    /// Run one named stabilization sub-step, logging success, failure, and timeout.
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

    /// Log the topology and admitted transports observed after a step exceeded
    /// its configured deadline.
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
        // Include both topology references and admitted transports so either a
        // dangling DHT slot or an unreferenced broken connection can be cleaned.
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

    /// Snapshot every admitted attempt into the compact state needed by the cleaner.
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

    /// Decide whether one referenced or admitted peer should be removed.
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
        // Preserve the attempt identity observed in this snapshot; the transport
        // removal path rejects it if a newer connection superseded the evidence.
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

    /// Decide the policy for one admitted peer currently observed as disconnected.
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

    /// Apply the selected removal while preserving superseding connection evidence.
    async fn remove_unavailable_peer(&self, did: Did, removal: TopologyPeerRemoval) -> Result<()> {
        let reason = removal.reason;
        // Logged before teardown so diagnostics show what failover evidence
        // justified removing or pruning the topology peer.
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
            "STABILIZATION clean_unavailable selected peer"
        );

        let outcome = if reason.should_disconnect_transport() {
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
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                fallback = ?outcome.fallback(),
                removal = ?outcome.removal(),
                "STABILIZATION clean_unavailable disconnect complete"
            );
            outcome
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
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                peer = %did,
                reason = reason.as_str(),
                fallback = ?outcome.fallback(),
                removal = ?outcome.removal(),
                "STABILIZATION clean_unavailable topology remove complete"
            );
            outcome
        };

        // `StorageResponsible(n, p) ⟺ Referenced(n, p)`: the cleaner also
        // removes admitted peers no slot references, and those change no
        // placement, so only a vacated slot makes a repair round due (#612).
        if outcome.removal().storage_repair_due() {
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

    /// Send liveness probes for idle admitted peers that do not already have one pending.
    async fn probe_peer_liveness(&self) -> Result<()> {
        let now_ms = get_epoch_ms_i64();
        // Probe epochs use wall-clock seconds; topology deadlines use the
        // monotonic ring clock elsewhere.
        let unix_seconds = u64::try_from(now_ms).unwrap_or(0) / 1_000;
        let epoch = ProvisionalEpoch::from_unix_seconds(unix_seconds);
        let candidates = self.transport.liveness_probe_candidates(now_ms)?;
        stream::iter(candidates)
            .then(|attempt| self.prepare_liveness_probe(attempt, epoch))
            .try_filter_map(|probe| ready(self.register_liveness_probe(probe)))
            .try_for_each(|probe| self.send_registered_liveness_probe(probe, now_ms))
            .await
    }

    /// Build and sign one liveness probe before it is registered as pending.
    async fn prepare_liveness_probe(
        &self,
        attempt: PendingConnectionAttempt,
        epoch: ProvisionalEpoch,
    ) -> Result<PreparedLivenessProbe> {
        let peer = attempt.peer();
        let peer_state = self
            .transport
            .get_connection(peer)
            .map(|conn| conn.webrtc_connection_state());
        let request = ProbeRequest::random_for_epoch(epoch);
        let payload = self
            .transport
            .signed_payload(Message::ProbeRequest(request), peer, peer)
            .await?;
        Ok(PreparedLivenessProbe {
            attempt,
            request,
            payload,
            peer_state,
        })
    }

    /// Register the probe transaction; returns `None` if another current probe
    /// already owns this attempt.
    fn register_liveness_probe(
        &self,
        probe: PreparedLivenessProbe,
    ) -> Result<Option<PreparedLivenessProbe>> {
        self.transport
            .register_pending_liveness_probe(
                probe.attempt,
                probe.payload.transaction.tx_id,
                probe.request,
            )
            .map(|registered| registered.then_some(probe))
    }

    /// Send a registered probe and either mark it sent or cancel the pending record.
    async fn send_registered_liveness_probe(
        &self,
        probe: PreparedLivenessProbe,
        now_ms: i64,
    ) -> Result<()> {
        let PreparedLivenessProbe {
            attempt,
            request,
            payload,
            peer_state,
        } = probe;
        let peer = attempt.peer();
        let tx_id = payload.transaction.tx_id;
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            peer = %peer,
            state = ?peer_state,
            idle_ms = PEER_LIVENESS_IDLE_MS,
            "STABILIZATION peer liveness probe send start"
        );
        match self.transport.send_payload(payload).await {
            Ok(()) => {
                let matching_probe_recorded = self
                    .transport
                    .record_peer_liveness_probe_sent(attempt, now_ms, tx_id, request)?;
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    tx_id = %tx_id,
                    matching_probe_recorded,
                    "STABILIZATION peer liveness probe send complete"
                );
            }
            Err(error) => {
                self.transport
                    .cancel_pending_liveness_probe(attempt, tx_id, request)?;
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    state = ?peer_state,
                    error = ?error,
                    records_peer_failure = error.records_peer_send_failure(),
                    "STABILIZATION peer liveness probe send failed"
                );
            }
        }
        Ok(())
    }

    /// Test-only hook that exposes liveness probing to the simulator.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn probe_peer_liveness_for_simulation(&self) -> Result<()> {
        self.probe_peer_liveness().await
    }

    /// Fix fingers from finger table, this is a DHT operation.
    async fn fix_fingers(&self) -> Result<()> {
        self.begin_finger_revalidation().await?;
        self.advance_finger_convergence().await
    }

    /// Ask the peer-ring state machine to mark its next unproved finger range.
    ///
    /// This boundary does not choose scheduling delay. It only converts the
    /// local state transition into the restricted action vocabulary accepted by
    /// [`Self::interpret_finger_action`]; a no-op means no range currently needs
    /// work.
    async fn begin_finger_revalidation(&self) -> Result<()> {
        self.interpret_finger_action(self.dht.begin_finger_revalidation())
            .await
    }

    /// Advance finger convergence by at most one state-machine effect.
    ///
    /// A runnable range may emit one `FindSuccessorForFix` request, while an
    /// inactive or still-waiting range emits no network work. The emitted action
    /// is interpreted through the same signing, send, and cancellation boundary
    /// as eager stabilization.
    async fn advance_finger_convergence(&self) -> Result<()> {
        self.interpret_finger_action(self.dht.advance_finger_convergence())
            .await
    }

    /// Advance one scheduled finger turn from a native dummy-network simulation.
    ///
    /// The hook deliberately exposes the production transition unchanged so
    /// model tests can control phase ordering without running the wall-clock
    /// maintenance loop.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn converge_fingers_for_simulation(&self) -> Result<()> {
        self.advance_finger_convergence().await
    }

    /// Interpret the closed set of peer-ring actions emitted by finger convergence.
    ///
    /// `None` completes locally. `FindSuccessorForFix` is signed and sent to the
    /// selected predecessor. A signing or send failure cancels the matching
    /// lookup lease before the error is returned, preventing an unsent request
    /// from leaving the range stuck in an awaiting-report phase. Any other action
    /// is rejected as an internal protocol mismatch.
    async fn interpret_finger_action(&self, action: Result<PeerRingAction>) -> Result<()> {
        match action {
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
                        request,
                    },
                ) => {
                    let msg = Message::FindSuccessorSend(FindSuccessorSend {
                        did: finger_did,
                        then: FindSuccessorThen::Report(
                            FindSuccessorReportHandler::FixFingerTable { request },
                        ),
                        strict: false,
                    });
                    let payload = match self
                        .transport
                        .signed_payload(msg.clone(), closest_predecessor, closest_predecessor)
                        .await
                    {
                        Ok(payload) => payload,
                        Err(error) => {
                            let _ = self.dht.cancel_finger_lookup(request);
                            return Err(error);
                        }
                    };
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
                        finger_slot = request.slot(),
                        request_id = %request.request_id(),
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
                            finger_slot = request.slot(),
                            request_id = %request.request_id(),
                            tx_id = %tx_id,
                            error = ?e,
                            "STABILIZATION fix_fingers send failed"
                        );
                        let _ = self.dht.cancel_finger_lookup(request);
                        return Err(e);
                    }
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        next_hop = %closest_predecessor,
                        finger_did = %finger_did,
                        finger_slot = request.slot(),
                        request_id = %request.request_id(),
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
                PeerRingRemoteAction::QueryForSuccessorListAndPred { request_id },
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
                        Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_stab(
                            next, request_id,
                        )),
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
                        self.dht.cancel_stabilization(request_id)?;
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

/// Monotonic elapsed milliseconds since `started_at`, saturating for diagnostics.
fn elapsed_since(started_at: Instant) -> i64 {
    i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// Scheduled topology, storage, and finger maintenance loop.
mod maintenance;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use maintenance::finger_awaiting_report_deadline_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use maintenance::finger_schedule_deadline_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use maintenance::finger_schedule_resumed_deadline_for_test;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::maintenance_phase_trace_for_test;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::reset_maintenance_phase_trace_for_test;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::MaintenancePhaseEvent;
#[cfg(all(test, target_family = "wasm"))]
pub(crate) use maintenance::MaintenancePhaseKind;
mod storage_repair;

/// Deadline behavior for stabilization sub-steps.
#[cfg(test)]
mod deadline_tests;
