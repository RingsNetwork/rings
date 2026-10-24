//! Handlers of relay custody (see `dht::relay_holder`): the previous holder a predecessor passes,
//! a recipient's fetch of its inbox, the holder's report back, a holder's acknowledgement or a
//! target's refusal of a hold, back to its writer, and a keeper's query of a key's owner for its
//! holder, with the answer.

use async_trait::async_trait;

use crate::dht::entry::inbox::inbox_key;
use crate::dht::relay_holder::owner_answer;
use crate::dht::relay_holder::PreviousHolder;
use crate::dht::relay_holder::RelayHolder;
use crate::dht::Chord;
use crate::dht::PeerRingAction;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::handlers::inbox::send_relay_hold;
use crate::message::types::AnnounceRelayHolder;
use crate::message::types::FetchRelayInbox;
use crate::message::types::FindRelayHolder;
use crate::message::types::Message;
use crate::message::types::RelayCarrierHeld;
use crate::message::types::RelayCarrierRefused;
use crate::message::types::RelayHolderFound;
use crate::message::types::RelayInboxReport;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::swarm::inbox::SwarmInboxDelivery;
use crate::utils::get_epoch_ms;

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<AnnounceRelayHolder> for MessageHandler {
    /// Take the previous holder the authenticated origin passed, iff it is this node's
    /// predecessor. A record this node does not take is ignored: the predecessor passes it again
    /// on its next repair pass.
    async fn handle(&self, ctx: &MessagePayload, msg: &AnnounceRelayHolder) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let from = ctx.transaction.origin();
        let previous = PreviousHolder {
            holder: msg.holder,
            run_start: msg.run_start,
        };
        if !self.dht.accept_previous_relay_holder(from, previous)? {
            tracing::debug!(
                local = %self.dht.did,
                %from,
                holder = %msg.holder,
                run_start = %msg.run_start,
                "previous relay holder not taken"
            );
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FetchRelayInbox> for MessageHandler {
    /// Report the live inbox of the authenticated origin to it, iff this node holds its inbox key
    /// under its own view; otherwise the fetch is refused with `RelayCarrierNotHeldHere`.
    async fn handle(&self, ctx: &MessagePayload, _: &FetchRelayInbox) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let recipient = ctx.transaction.origin();
        if !self.dht.holds_relay_key(inbox_key(recipient))? {
            return Err(Error::RelayCarrierNotHeldHere);
        }
        let inbox = self
            .dht
            .live_storage_entry(StorageKey::inbox_of(recipient), get_epoch_ms())
            .await?;
        self.run_effects([CoreEffect::send_report_message(
            ctx,
            Message::RelayInboxReport(RelayInboxReport { inbox }),
        )])
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayInboxReport> for MessageHandler {
    /// Drain the inbox the authenticated origin reported, iff it holds this node's inbox key
    /// under this node's own view (see `SwarmInboxDelivery::deliver_fetched`).
    async fn handle(&self, ctx: &MessagePayload, msg: &RelayInboxReport) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        SwarmInboxDelivery::new(self.transport.clone())
            .deliver_fetched(ctx.transaction.origin(), msg.inbox.clone())
            .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayCarrierHeld> for MessageHandler {
    /// Stop keeping a hold the node it was last sent to acknowledges; an acknowledgement from any
    /// other node, or of a hold this node does not keep, changes nothing.
    async fn handle(&self, ctx: &MessagePayload, msg: &RelayCarrierHeld) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        self.dht
            .acknowledge_relay_hold(msg.hold, ctx.transaction.origin())
            .map(drop)
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayCarrierRefused> for MessageHandler {
    /// Send again at once a kept hold the node it was last sent to refuses, iff this node's view
    /// now names another holder; otherwise the next repair pass sends it again. A refusal of a
    /// hold this node does not keep, or from another node, changes nothing.
    async fn handle(&self, ctx: &MessagePayload, msg: &RelayCarrierRefused) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let refuser = ctx.transaction.origin();
        let Some(hold) = self.dht.refused_relay_hold(msg.hold, refuser)? else {
            return Ok(());
        };
        match self.dht.relay_holder(hold.entry().did)? {
            RelayHolder::Holder(holder) if holder != refuser => {
                send_relay_hold(&self.transport, hold).await
            }
            _ => Ok(()),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FindRelayHolder> for MessageHandler {
    /// Answer with the holder of `msg.key` as this node's view names it, when this node owns the
    /// key (`Law (Owner places)`); otherwise route the query on toward the owner, as a lookup is.
    async fn handle(&self, ctx: &MessagePayload, msg: &FindRelayHolder) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        match self.dht.find_successor(msg.key)? {
            PeerRingAction::Some(_) => {
                let holder = owner_answer(&self.dht.relay_segment()?, self.dht.did, msg.key);
                self.run_effects([CoreEffect::send_report_message(
                    ctx,
                    Message::RelayHolderFound(RelayHolderFound {
                        key: msg.key,
                        holder,
                    }),
                )])
                .await
            }
            PeerRingAction::RemoteAction(next, _) => {
                self.run_effects([CoreEffect::reset_destination(ctx, next)])
                    .await
            }
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayHolderFound> for MessageHandler {
    /// Take the answer to this node's outstanding query for `msg.key` (`Law (Asked)`): record
    /// the holder the owner named, and ask for a repair pass, which offers the carrier there,
    /// only when that hint changed. An answer nobody asked for, a second answer, or a wrong one
    /// costs no pass and moves nothing.
    async fn handle(&self, ctx: &MessagePayload, msg: &RelayHolderFound) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        if self.dht.name_relay_holder(msg.key, msg.holder)? {
            self.run_effects([CoreEffect::request_storage_repair()])
                .await?;
        }
        Ok(())
    }
}
