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
use crate::message::MessagePayload;

/// Correlated topology-report admission and bounded candidate processing.
///
/// The submodule owns the request-token checks and connection-plan state
/// machine used by topology reports. Keeping that flow separate prevents the
/// general connection dispatcher from mutating DHT topology before transport
/// evidence is current.
mod topology_view;

use topology_view::connect_successor_hint;

/// QueryForTopoInfoSend is direct message
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<QueryForTopoInfoSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &QueryForTopoInfoSend) -> Result<()> {
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
    async fn handle(&self, ctx: &MessagePayload, msg: &ConnectNodeSend) -> Result<()> {
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
            self.run_effects([CoreEffect::forward_payload(ctx, None)])
                .await
        } else {
            let peer = ctx.transaction.origin();
            tracing::trace!(
                local = %self.dht.did,
                peer = %peer,
                tx_id = %ctx.transaction.tx_id,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE offer answer start"
            );
            let answer = match self
                .transport
                .answer_remote_connection(peer, self.inner_callback(), msg)
                .await
            {
                Ok(answer) => {
                    tracing::trace!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        sdp_bytes = answer.sdp.len(),
                        "CONNECT_NODE offer answer complete"
                    );
                    answer
                }
                Err(error) => {
                    tracing::warn!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        error = ?error,
                        "CONNECT_NODE offer answer failed"
                    );
                    return Err(error);
                }
            };
            self.run_effects([CoreEffect::send_report_message(
                ctx,
                Message::ConnectNodeReport(answer),
            )])
            .await
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ConnectNodeReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ConnectNodeReport) -> Result<()> {
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
            self.run_effects([CoreEffect::forward_payload(ctx, None)])
                .await
        } else {
            let peer = ctx.transaction.origin();
            tracing::trace!(
                local = %self.dht.did,
                peer = %peer,
                tx_id = %ctx.transaction.tx_id,
                sdp_bytes = msg.sdp.len(),
                "CONNECT_NODE answer accept start"
            );
            match self
                .transport
                .accept_remote_connection(peer, msg, None)
                .await
            {
                Ok(()) => {
                    tracing::trace!(
                        local = %self.dht.did,
                        peer = %peer,
                        tx_id = %ctx.transaction.tx_id,
                        "CONNECT_NODE answer accept complete"
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
    async fn handle(&self, ctx: &MessagePayload, msg: &FindSuccessorSend) -> Result<()> {
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
    async fn handle(&self, ctx: &MessagePayload, msg: &FindSuccessorReport) -> Result<()> {
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
pub mod tests;
