use async_trait::async_trait;

use crate::dht::ChordStorageSync;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::types::Message;
use crate::message::types::NotifyPredecessorReport;
use crate::message::types::NotifyPredecessorSend;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<NotifyPredecessorSend> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &NotifyPredecessorSend) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }

        let origin = self.verified_notify_predecessor_origin(ctx, msg)?;
        let Some(predecessor) = self.transport.notify_admitted_predecessor(origin)? else {
            return Err(Error::NotifyPredecessorOriginNotAdmitted { origin });
        };

        if predecessor != origin {
            return self
                .run_effects([CoreEffect::send_report_message(
                    ctx,
                    Message::NotifyPredecessorReport(NotifyPredecessorReport { did: predecessor }),
                )])
                .await;
        }

        Ok(())
    }
}

impl MessageHandler {
    fn verified_notify_predecessor_origin(
        &self,
        ctx: &MessagePayload,
        msg: &NotifyPredecessorSend,
    ) -> Result<crate::dht::Did> {
        let origin = ctx.relay.try_origin_sender()?;
        if msg.did != origin {
            return Err(Error::NotifyPredecessorOriginMismatch {
                claimed: msg.did,
                origin,
            });
        }
        Ok(origin)
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<NotifyPredecessorReport> for MessageHandler {
    async fn handle(&self, _ctx: &MessagePayload, msg: &NotifyPredecessorReport) -> Result<()> {
        self.run_effects([CoreEffect::connect_dht_peer(msg.did)])
            .await?;

        let deliveries = self
            .dht
            .sync_entries_with_successor(msg.did)
            .await?
            .coalesced_storage_sync_deliveries()?;
        let effects = deliveries.into_iter().map(|delivery| {
            let msg = SyncEntriesWithSuccessor::from_delivery(delivery);
            CoreEffect::send_storage_sync(msg)
        });
        self.run_effects(effects).await?;

        Ok(())
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod tests;
