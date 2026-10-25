use async_trait::async_trait;

use crate::dht::types::Chord;
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

/// QueryForTopoInfoSend is direct message
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<QueryForTopoInfoSend> for MessageHandler {
    async fn handle(&self, ctx: &HandlerContext<'_>, msg: &QueryForTopoInfoSend) -> Result<()> {
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
    async fn handle(&self, ctx: &HandlerContext<'_>, msg: &ConnectNodeSend) -> Result<()> {
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
            match drop_unlinked_return(answered, "CONNECT_NODE offer reply") {
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
    async fn handle(&self, ctx: &HandlerContext<'_>, msg: &ConnectNodeReport) -> Result<()> {
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

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FindSuccessorSend> for MessageHandler {
    async fn handle(&self, ctx: &HandlerContext<'_>, msg: &FindSuccessorSend) -> Result<()> {
        match self.dht.find_successor(msg.did)? {
            PeerRingAction::Some(did) => {
                if msg.accepts_local_successor(self.dht.did) {
                    match &msg.then {
                        FindSuccessorThen::Report(handler) => {
                            let did = match handler {
                                FindSuccessorReportHandler::Connect => connect_successor_hint(
                                    self.dht.as_ref(),
                                    ctx.transaction.origin(),
                                    did,
                                )?,
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
                    self.run_effects([CoreEffect::forward_payload(ctx, Some(did))])
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
    async fn handle(&self, ctx: &HandlerContext<'_>, msg: &FindSuccessorReport) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }

        match &msg.handler {
            FindSuccessorReportHandler::FixFingerTable { request } => {
                // First classify the reported successor against the current
                // transport generation. This may apply the finger immediately,
                // queue it behind an existing handshake, or retain an
                // admission proof while the missing connection is opened below.
                let disposition = self.transport.record_finger_candidate(msg.did, *request)?;
                if disposition.proof_awaits_owner() && msg.reports_remote_successor(self.dht.did) {
                    if let Err(error) = self.connect_dht_peer(msg.did).await {
                        self.dht.cancel_finger_lookup(*request)?;
                        return Err(error);
                    }
                    // The async connect may have admitted the same peer,
                    // replaced the pending slot, or failed after a partial
                    // lifecycle transition. Re-run admission with the same
                    // request id so the deferred proof either gains an owner or
                    // is explicitly released.
                    let admitted = self.transport.record_finger_candidate(msg.did, *request)?;
                    if admitted.proof_awaits_owner() {
                        self.dht.cancel_finger_lookup(*request)?;
                    }
                }
            }
            FindSuccessorReportHandler::Connect if msg.reports_remote_successor(self.dht.did) => {
                self.connect_dht_peer(msg.did).await?;
            }
            FindSuccessorReportHandler::Connect => {}
            // An application lookup: resolve the handle the application awaits. A report no
            // live lookup awaits is unsolicited and dropped here; the application never sees it.
            FindSuccessorReportHandler::None => {
                self.transport
                    .application()
                    .lookups
                    .answer(ctx.transaction.tx_id, msg.did);
            }
        }

        Ok(())
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod test_connection;
