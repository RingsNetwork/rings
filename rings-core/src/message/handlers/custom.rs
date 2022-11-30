use async_trait::async_trait;

use crate::dht::Chord;
use crate::dht::PeerRingAction;
use crate::err::Error;
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
        let mut relay = ctx.relay.clone();

        if self.dht.did != relay.destination {
            if self.swarm.get_transport(relay.destination).is_some() {
                relay.relay(self.dht.did, Some(relay.destination))?;
                return self.forward_payload(ctx, relay).await;
            } else {
                let next_node = match self.dht.find_successor(relay.destination)? {
                    PeerRingAction::Some(node) => Some(node),
                    PeerRingAction::RemoteAction(node, _) => Some(node),
                    _ => None,
                }
                .ok_or(Error::MessageHandlerMissNextNode)?;
                relay.relay(self.dht.did, Some(next_node))?;
                return self.forward_payload(ctx, relay).await;
            }
        }

        Ok(())
    }
}
