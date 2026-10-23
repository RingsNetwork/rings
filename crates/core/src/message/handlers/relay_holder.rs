//! Handlers of relay custody (see `dht::relay_holder`): the previous holder a predecessor passes,
//! a recipient's fetch of its inbox, and the holder's report back.

use async_trait::async_trait;

use crate::dht::entry::inbox::inbox_key;
use crate::dht::relay_holder::PreviousHolder;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::types::AnnounceRelayHolder;
use crate::message::types::FetchRelayInbox;
use crate::message::types::Message;
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
