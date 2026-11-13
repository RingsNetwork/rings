use async_trait::async_trait;

use crate::dht::types::Chord;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::TopoInfo;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::types::ConnectNodeReport;
use crate::message::types::ConnectNodeSend;
use crate::message::types::FindSuccessorReport;
use crate::message::types::FindSuccessorSend;
use crate::message::types::Message;
use crate::message::types::QueryForTopoInfoSend;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::HandleMsg;
use crate::message::MessageHandler;

/// Correlated topology-report admission and bounded candidate processing.
///
/// The submodule owns the request-token checks and connection-plan state
/// machine used by topology reports. Keeping that flow separate prevents the
/// general connection dispatcher from mutating DHT topology before transport
/// evidence is current.
mod topology_view;

use topology_view::connect_successor_hint;

use crate::message::handlers::HandlerContext;

/// `result` with the expected drop of a connect-signaling reply whose recorded path names a hop
/// this node lost the link to (#947) logged at debug and absorbed: the initiator's pending
/// timeout covers it, and it is no fault of this node.
fn drop_unlinked_return(result: Result<()>, context: &'static str) -> Result<()> {
    match result {
        Err(Error::RelayPathHopUnlinked { hop }) => {
            tracing::debug!(%hop, context, "connect signaling reply dropped: return hop unlinked");
            Ok(())
        }
        other => other,
    }
}

/// `result` of answering `peer`'s offer with the refusal of the peer's slot (#1152) logged at
/// debug and absorbed: `AlreadyConnected` is the slot's answer, an admitted connection or a
/// handshake with `peer` still under way (`Law (Crossing offer)`), so no reply is owed; that
/// handshake settles the edge, or the peer's next offer does once it ends.
fn absorb_refused_offer(result: Result<()>, peer: Did) -> Result<()> {
    match result {
        Err(Error::AlreadyConnected) => {
            tracing::debug!(%peer, "CONNECT_NODE offer refused: a handshake with the peer holds the slot");
            Ok(())
        }
        other => other,
    }
}

/// QueryForTopoInfoSend is direct message
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<QueryForTopoInfoSend> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &QueryForTopoInfoSend) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let info: TopoInfo = TopoInfo::try_from(self.dht.as_ref())?;
        if msg.targets(self.dht.did) {
            self.run_effects([CoreEffect::send_report_message(
                ctx,
                Message::QueryForTopoInfoReport(msg.resp(info)),
            )])
            .await?
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ConnectNodeSend> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &ConnectNodeSend) -> Result<()> {
        let ctx = context.payload();
        if !self.transport.accepts_connection_offer(msg) {
            tracing::warn!(
                local = %self.dht.did,
                tx_id = %ctx.transaction.tx_id,
                origin = %ctx.transaction.origin(),
                relay_destination = %ctx.relay.destination,
                transaction_destination = %ctx.transaction.destination,
                mode = ?msg.dht_protocol_mode,
                "CONNECT_NODE offer rejected by DHT protocol mismatch"
            );
            return Ok(());
        }

        if ctx.should_forward_from(self.dht.did) {
            tracing::trace!(
                local = %self.dht.did,
                tx_id = %ctx.transaction.tx_id,
                origin = %ctx.transaction.origin(),
                next_hop = %ctx.relay.next_hop,
                relay_destination = %ctx.relay.destination,
                transaction_destination = %ctx.transaction.destination,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE offer forward"
            );
            drop_unlinked_return(
                self.run_effects([CoreEffect::forward_payload(ctx, None)])
                    .await,
                "CONNECT_NODE forward",
            )
        } else {
            let peer = ctx.transaction.origin();
            tracing::trace!(
                local = %self.dht.did,
                peer = %peer,
                tx_id = %ctx.transaction.tx_id,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE offer answer start"
            );
            // The reply, an answer or a counter-offer, retraces the offer's path (#947); the
            // answer waits on ICE gathering, so it is negotiated and sent apart from this inbound
            // event (inbound locality).
            let answered = self
                .transport
                .answer_signaled_offer(self.inner_callback(), ctx, msg)
                .await;
            match absorb_refused_offer(
                drop_unlinked_return(answered, "CONNECT_NODE offer reply"),
                peer,
            ) {
                Ok(()) => {
                    tracing::trace!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        "CONNECT_NODE offer reply complete"
                    );
                    Ok(())
                }
                Err(error) => {
                    tracing::warn!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        error = ?error,
                        "CONNECT_NODE offer answer failed"
                    );
                    Err(error)
                }
            }
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &ConnectNodeReport) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            tracing::trace!(
                local = %self.dht.did,
                tx_id = %ctx.transaction.tx_id,
                origin = %ctx.transaction.origin(),
                next_hop = %ctx.relay.next_hop,
                relay_destination = %ctx.relay.destination,
                transaction_destination = %ctx.transaction.destination,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE answer forward"
            );
            drop_unlinked_return(
                self.run_effects([CoreEffect::forward_payload(ctx, None)])
                    .await,
                "CONNECT_NODE forward",
            )
        } else {
            let peer = ctx.transaction.origin();
            tracing::trace!(
                local = %self.dht.did,
                peer = %peer,
                tx_id = %ctx.transaction.tx_id,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE answer accept start"
            );
            // The answer applies only to the generation whose offer left under this
            // transaction (#947); a late one, for an offer since released, touches nothing.
            match self
                .transport
                .accept_signaled_answer(peer, ctx.transaction.tx_id, msg)
                .await
            {
                Ok(true) => {
                    tracing::trace!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        "CONNECT_NODE answer accept complete"
                    );
                    Ok(())
                }
                Ok(false) => {
                    tracing::debug!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        "CONNECT_NODE answer for no pending offer dropped"
                    );
                    Ok(())
                }
                Err(error) => {
                    tracing::warn!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        error = ?error,
                        "CONNECT_NODE answer accept failed"
                    );
                    Err(error)
                }
            }
        }
    }
}

impl MessageHandler {
    /// Resolve `msg` against the local Chord topology, consulting passive 1-hop
    /// bridge hints (`TopologyState::bridged`) only for query-level resolution:
    /// - `Connect` and `FixFingerTable` reports require a directly connected
    ///   topology peer (`head`) so bootstrap `ConnectNodeSend` and finger table
    ///   updates do not target an unreachable bridged peer.
    /// - Application lookups (`FindSuccessorReportHandler::None`) report the
    ///   closest bridged peer in `[msg.did, head)` when `msg` accepts a local
    ///   successor.
    /// - Strict lookups (`msg.strict && msg.did != self.dht.did`) forward
    ///   directly to `msg.did` when `msg.did` is already connected or
    ///   referenced in local topology, or route via the bridge's connected
    ///   `via` successor when `msg.did` is recorded in `state.bridged`.
    pub(crate) fn find_successor_for_query(
        &self,
        msg: &FindSuccessorSend,
    ) -> Result<PeerRingAction> {
        if !msg.accepts_local_successor(self.dht.did) {
            if self.transport.get_connection(msg.did).is_some() {
                return Ok(PeerRingAction::Some(msg.did));
            }
            let state = self.dht.topology_state()?;
            if state.references(msg.did) {
                return Ok(PeerRingAction::Some(msg.did));
            }
            if let Some(via) = state.bridged.get(&msg.did).copied().filter(|&via| {
                via != state.local
                    && state.references(via)
                    && self.transport.get_connection(via).is_some()
            }) {
                return Ok(PeerRingAction::RemoteAction(
                    via,
                    crate::dht::PeerRingRemoteAction::FindSuccessor(msg.did),
                ));
            }
            return self.dht.find_successor(msg.did);
        }

        let action = self.dht.find_successor(msg.did)?;
        let PeerRingAction::Some(head) = action else {
            return Ok(action);
        };
        if head == self.dht.did || msg.did == self.dht.did {
            return Ok(PeerRingAction::Some(head));
        }
        let state = self.dht.topology_state()?;
        let target = crate::dht::topology::dist(state.local, msg.did);
        let head_dist = crate::dht::topology::dist(state.local, head);
        let bridged = state
            .bridged
            .iter()
            .filter(|(&peer, &via)| {
                peer != state.local
                    && via != state.local
                    && state.references(via)
                    && self.transport.get_connection(via).is_some()
                    && !state.references(peer)
            })
            .filter_map(|(&peer, &via)| {
                let peer_dist = crate::dht::topology::dist(state.local, peer);
                (target <= peer_dist && peer_dist < head_dist).then_some((peer_dist, peer, via))
            })
            .min_by(|(dist_a, _, _), (dist_b, _, _)| dist_a.cmp(dist_b))
            .map(|(_, peer, via)| (peer, via));
        let Some((bridged_peer, _via)) = bridged else {
            return Ok(PeerRingAction::Some(head));
        };
        let FindSuccessorThen::Report(handler) = &msg.then;
        match handler {
            FindSuccessorReportHandler::None => Ok(PeerRingAction::Some(bridged_peer)),
            FindSuccessorReportHandler::Connect { .. }
            | FindSuccessorReportHandler::FixFingerTable { .. } => {
                if self.transport.get_connection(bridged_peer).is_some() {
                    Ok(PeerRingAction::Some(bridged_peer))
                } else {
                    Ok(PeerRingAction::Some(head))
                }
            }
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &FindSuccessorSend) -> Result<()> {
        let ctx = context.payload();
        match self.find_successor_for_query(msg)? {
            PeerRingAction::Some(did) => {
                if msg.accepts_local_successor(self.dht.did) {
                    match &msg.then {
                        FindSuccessorThen::Report(handler) => {
                            let did = match handler {
                                FindSuccessorReportHandler::Connect { .. } => {
                                    connect_successor_hint(
                                        self.dht.as_ref(),
                                        ctx.transaction.origin(),
                                        did,
                                    )?
                                }
                                FindSuccessorReportHandler::FixFingerTable { .. }
                                    if did != self.dht.did
                                        && self.transport.get_connection(did).is_none() =>
                                {
                                    self.dht
                                        .successors()
                                        .list()?
                                        .into_iter()
                                        .find(|succ| self.transport.get_connection(*succ).is_some())
                                        .unwrap_or(did)
                                }
                                _ => did,
                            };
                            self.run_effects([CoreEffect::send_report_message(
                                ctx,
                                Message::FindSuccessorReport(FindSuccessorReport {
                                    did,
                                    handler: handler.clone(),
                                }),
                            )])
                            .await
                        }
                    }
                } else {
                    self.run_effects([CoreEffect::reset_destination(ctx, did)])
                        .await
                }
            }
            PeerRingAction::RemoteAction(next, _) => {
                self.run_effects([CoreEffect::reset_destination(ctx, next)])
                    .await
            }
            act => Err(Error::unexpected_peer_ring_action(act)),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FindSuccessorReport> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &FindSuccessorReport) -> Result<()> {
        let ctx = context.payload();
        let effect = if ctx.should_forward_from(self.dht.did) {
            CoreEffect::forward_payload(ctx, None)
        } else {
            let reports_remote = msg.reports_remote_successor(self.dht.did);
            match &msg.handler {
                FindSuccessorReportHandler::FixFingerTable { request } => {
                    CoreEffect::admit_finger_candidate(msg.did, *request, reports_remote)
                }
                FindSuccessorReportHandler::Connect { request_id } => {
                    CoreEffect::claim_connect_candidate(msg.did, *request_id, reports_remote)
                }
                // An application lookup: resolve the handle the application awaits. A report no
                // live lookup awaits is unsolicited and dropped here; the application never sees it.
                FindSuccessorReportHandler::None => {
                    CoreEffect::answer_application_lookup(ctx.transaction.tx_id, msg.did)
                }
            }
        };
        self.run_effects([effect]).await
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod test_connection;
