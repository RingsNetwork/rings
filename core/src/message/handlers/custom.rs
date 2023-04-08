use async_trait::async_trait;

use crate::err::Result;
use crate::message::types::CustomMessage;
use crate::message::types::MaybeEncrypted;
use crate::message::types::Message;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::transports::manager::TransportManager;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<MaybeEncrypted<CustomMessage>> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        _: &MaybeEncrypted<CustomMessage>,
    ) -> Result<()> {
        if self.dht.did != ctx.relay.destination {
            if self
                .swarm
                .get_and_check_transport(ctx.relay.destination)
                .await
                .is_some()
            {
                return self.forward_payload(ctx, Some(ctx.relay.destination)).await;
            } else {
                return self.forward_payload(ctx, None).await;
            }
        }

        Ok(())
    }
}
