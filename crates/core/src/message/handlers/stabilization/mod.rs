use async_trait::async_trait;

use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::handlers::HandlerContext;
use crate::message::types::Leave;
use crate::message::types::NotifyPredecessorSend;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<NotifyPredecessorSend> for MessageHandler {
    async fn handle(
        &self,
        context: &HandlerContext<'_>,
        msg: &NotifyPredecessorSend,
    ) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }

        let origin = self.verified_notify_predecessor_origin(ctx, msg)?;
        if self
            .transport
            .notify_admitted_predecessor(origin)?
            .is_none()
        {
            return Err(Error::NotifyPredecessorOriginNotAdmitted { origin });
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<Leave> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &Leave) -> Result<()> {
        let ctx = context.payload();
        let repair_due = (!ctx.should_forward_from(self.dht.did))
            .then(|| {
                self.transport
                    .handle_peer_leave(ctx.transaction.origin(), &msg.successor_list)
            })
            .transpose()?
            .flatten()
            .is_some_and(|removal| removal.storage_repair_due());
        match repair_due {
            true => {
                self.run_effects([CoreEffect::request_storage_repair()])
                    .await
            }
            false => Ok(()),
        }
    }
}

impl MessageHandler {
    fn verified_notify_predecessor_origin(
        &self,
        ctx: &MessagePayload,
        msg: &NotifyPredecessorSend,
    ) -> Result<crate::dht::Did> {
        let origin = ctx.transaction.origin();
        if msg.did != origin {
            return Err(Error::NotifyPredecessorOriginMismatch {
                claimed: msg.did,
                origin,
            });
        }
        Ok(origin)
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod tests;
