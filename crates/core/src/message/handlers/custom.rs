use async_trait::async_trait;

use crate::dht::Did;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::types::CustomMessage;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

/// How this node stands to a message's destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reachability {
    /// The destination can be reached, or is somebody else's to reach: forward.
    Reachable,
    /// This node is responsible for the destination's position and it has no connection: hold.
    Offline,
}

/// The effect an application message requires at `local`.
///
/// Post: a message for `local` needs no effect; a message for a destination this node is
/// responsible for but cannot reach is held in that destination's relay inbox; every other
/// message is forwarded along the relay path.
pub(crate) fn custom_message_effects<'payload>(
    local: Did,
    ctx: &'payload MessagePayload,
    destination: Reachability,
) -> Option<CoreEffect<'payload>> {
    if !ctx.should_forward_from(local) {
        None
    } else {
        Some(match destination {
            Reachability::Offline => CoreEffect::hold_for_offline_destination(ctx),
            Reachability::Reachable => CoreEffect::forward_payload(ctx, None),
        })
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<CustomMessage> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, _: &CustomMessage) -> Result<()> {
        let destination = self.destination_reachability(ctx.transaction.destination)?;
        self.run_effects(custom_message_effects(self.dht.did, ctx, destination))
            .await
    }
}
