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
/// message is forwarded one hop further along its Chord route.
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

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ecc::SecretKey;
    use crate::error::Error;
    use crate::message::HopBudget;
    use crate::message::Message;
    use crate::message::MessagePayload;
    use crate::message::MessageSigner;
    use crate::session::SessionSk;
    use crate::swarm::callback::SwarmCallback;
    use crate::tests::default::assert_no_more_msg;
    use crate::tests::default::prepare_node;
    use crate::tests::default::wait_for_msgs;
    use crate::tests::default::Node;
    use crate::tests::manually_establish_connection;
    use crate::tests::TEST_NETWORK_ID;

    struct NoopCallback;

    impl SwarmCallback for NoopCallback {}

    /// An application payload from a stranger, addressed through `relay` to `destination`.
    fn payload_through(
        relay: &Node,
        destination: &Node,
        hop_budget: HopBudget,
    ) -> Result<(MessagePayload, CustomMessage)> {
        let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
        let message = CustomMessage(b"through".to_vec());
        let payload = MessagePayload::new_send(
            Message::CustomMessage(message.clone()),
            MessageSigner::new(&origin, TEST_NETWORK_ID),
            relay.did(),
            destination.did(),
            hop_budget,
        )?;
        Ok((payload, message))
    }

    /// A hop drops a payload whose carrier has no forward left, with the typed error, and the
    /// destination never sees it; the same payload with one forward left is forwarded and
    /// arrives with its budget spent.
    #[tokio::test]
    async fn test_forwarding_observes_the_hop_budget() -> Result<()> {
        let relay = prepare_node(SecretKey::random()).await;
        let destination = prepare_node(SecretKey::random()).await;
        manually_establish_connection(&relay.swarm, &destination.swarm).await;
        wait_for_msgs([&relay, &destination]).await;
        assert_no_more_msg([&relay, &destination]).await;
        let handler = MessageHandler::new(relay.swarm.transport.clone(), Arc::new(NoopCallback));

        let (exhausted, message) = payload_through(&relay, &destination, HopBudget::EXHAUSTED)?;
        assert!(matches!(
            handler.handle(&exhausted, &message).await,
            Err(Error::RelayHopBudgetExhausted)
        ));
        assert_no_more_msg([&destination]).await;

        let last_forward = HopBudget::try_from(1)?;
        let (forwardable, message) = payload_through(&relay, &destination, last_forward)?;
        handler.handle(&forwardable, &message).await?;
        let received = destination
            .listen_once()
            .await
            .ok_or_else(|| Error::InvalidMessage("forwarded payload not delivered".to_string()))?;

        assert_eq!(received.transaction, forwardable.transaction);
        assert_eq!(received.relay.next_hop, destination.did());
        assert_eq!(received.relay.destination, destination.did());
        assert_eq!(received.relay.hop_budget, HopBudget::EXHAUSTED);
        Ok(())
    }
}
