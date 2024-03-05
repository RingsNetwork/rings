use async_trait::async_trait;

use crate::error::Result;
use crate::message::types::CustomMessage;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<CustomMessage> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, _: &CustomMessage) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            self.transport.forward_payload(ctx, None).await?;
        }
        Ok(())
    }
}
