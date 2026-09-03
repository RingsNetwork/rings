use async_trait::async_trait;

use crate::dht::Did;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::types::CustomMessage;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

/// The effect an application message requires at `local`.
///
/// Post: a message for `local` needs no effect; a message for a destination this node is
/// responsible for but cannot reach is held in that destination's relay inbox; every other
/// message is forwarded along the relay path.
pub(crate) fn custom_message_effects<'payload>(
    local: Did,
    ctx: &'payload MessagePayload,
    destination_offline: bool,
) -> Option<CoreEffect<'payload>> {
    if !ctx.should_forward_from(local) {
        None
    } else if destination_offline {
        Some(CoreEffect::hold_for_offline_destination(ctx))
    } else {
        Some(CoreEffect::forward_payload(ctx, None))
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<CustomMessage> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, _: &CustomMessage) -> Result<()> {
        let destination_offline = self.destination_is_offline(ctx.transaction.destination)?;
        self.run_effects(custom_message_effects(
            self.dht.did,
            ctx,
            destination_offline,
        ))
        .await
    }
}
