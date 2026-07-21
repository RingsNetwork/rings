//! Stabilization run daemons to maintain dht.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use futures_timer::Delay;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::dht::successor::SuccessorReader;
use crate::dht::types::ChordStorageRepair;
use crate::dht::types::CorrectChord;
use crate::dht::Chord;
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
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::QueryForTopoInfoSend;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::SwarmTransport;

const STABILIZATION_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// The stabilization runner.
#[derive(Clone)]
pub struct Stabilizer {
    transport: Arc<SwarmTransport>,
    dht: Arc<PeerRing>,
}

impl Stabilizer {
    /// Create a new stabilization runner.
    pub fn new(transport: Arc<SwarmTransport>) -> Self {
        let dht = transport.dht.clone();
        Self { transport, dht }
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
    where
        F: Future<Output = Result<()>>,
    {
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            step,
            timeout_ms = timeout.as_millis(),
            "STABILIZATION step start"
        );

        let result = {
            let future = future.fuse();
            let timer = Delay::new(timeout).fuse();
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
        let deliveries = act.storage_sync_deliveries()?;
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
        }
        Ok(())
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
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            action = ?action,
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
    pub async fn clean_unavailable_connections(&self) -> Result<()> {
        self.transport.expire_pending_connections().await?;
        let conns = self.transport.admitted_connections();

        for (did, conn) in conns.into_iter() {
            let state = conn.webrtc_connection_state();
            // Only terminal states are cleaned. `Disconnected` is transient: ICE
            // can recover from it, so tearing it down here (the stabilizer runs
            // every few seconds) would kill connections during a brief blip
            // before WebRTC self-heals. This mirrors the swarm callback, which
            // also only leaves the DHT on `Failed`/`Closed`.
            if matches!(
                state,
                WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
            ) {
                tracing::info!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %did,
                    state = ?state,
                    "STABILIZATION clean_unavailable selected terminal transport"
                );
                let should_repair = self
                    .dht
                    .peer_may_share_storage_responsibility(did, self.transport.storage_redundancy())
                    .await?;
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %did,
                    state = ?state,
                    should_repair,
                    "STABILIZATION clean_unavailable disconnect start"
                );
                self.transport.disconnect(did).await?;
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %did,
                    state = ?state,
                    should_repair,
                    "STABILIZATION clean_unavailable disconnect complete"
                );
                if should_repair {
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        peer = %did,
                        "STABILIZATION clean_unavailable repair start"
                    );
                    self.repair_storage().await?;
                    tracing::debug!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        peer = %did,
                        "STABILIZATION clean_unavailable repair complete"
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

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
mod stabilizer {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::future::FutureExt;
    use futures::pin_mut;
    use futures::select;
    use futures_timer::Delay;

    use super::*;

    impl Stabilizer {
        /// Run stabilization in a loop.
        pub async fn wait(self: Arc<Self>, interval: Duration) {
            loop {
                let timeout = Delay::new(interval).fuse();
                pin_mut!(timeout);
                select! {
                    _ = timeout => self
                        .stabilize()
                        .await
                        .unwrap_or_else(|e| tracing::error!("failed to stabilize {:?}", e)),
                }
            }
        }
    }
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
mod stabilizer {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::poll;

    impl Stabilizer {
        /// Run stabilization in a loop.
        pub async fn wait(self: Arc<Self>, interval: Duration) {
            let millis = i32::try_from(interval.as_millis()).unwrap_or(i32::MAX);
            let stabilizer = self;
            poll!(
                {
                    let stabilizer = Arc::clone(&stabilizer);
                    async move {
                        stabilizer
                            .stabilize()
                            .await
                            .unwrap_or_else(|e| tracing::error!("failed to stabilize {:?}", e));
                    }
                },
                millis
            );
        }
    }
}
