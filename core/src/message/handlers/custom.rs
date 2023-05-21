use async_trait::async_trait;

use crate::err::Result;
use crate::message::types::CustomMessage;
use crate::message::types::MaybeEncrypted;
use crate::message::types::Message;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<MaybeEncrypted<CustomMessage>> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        _: &MaybeEncrypted<CustomMessage>,
    ) -> Result<Vec<MessageHandlerEvent>> {
        if self.dht.did != ctx.relay.destination {
            Ok(vec![MessageHandlerEvent::ForwardPayload].into())
        } else {
            Ok(vec![])
        }
    }
}
