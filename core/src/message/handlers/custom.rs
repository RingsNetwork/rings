use async_trait::async_trait;

use crate::error::Result;
use crate::message::types::CustomMessage;
use crate::message::types::Message;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<CustomMessage> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        _: &CustomMessage,
    ) -> Result<Vec<MessageHandlerEvent>> {
        if self.dht.did != ctx.relay.destination {
            Ok(vec![MessageHandlerEvent::ForwardPayload(ctx.clone(), None)])
        } else {
            Ok(vec![])
        }
    }
}
