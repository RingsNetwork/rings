use async_trait::async_trait;

use crate::dht::Did;
use crate::error::Result;
use crate::message::effects::CoreEffect;
#[cfg(test)]
use crate::message::effects::PayloadRelayFunctor;
use crate::message::types::CustomMessage;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

pub(crate) fn custom_message_effects(local: Did, ctx: &MessagePayload) -> Vec<CoreEffect> {
    if local != ctx.relay.destination {
        vec![CoreEffect::forward_payload(ctx, None)]
    } else {
        Vec::new()
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<CustomMessage> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, _: &CustomMessage) -> Result<()> {
        self.run_effects(custom_message_effects(self.dht.did, ctx))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;
    use crate::message::Message;
    use crate::session::SessionSk;

    fn custom_payload(destination: Did) -> Result<MessagePayload> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        MessagePayload::new_send(
            Message::custom(b"hello")?,
            &session_sk,
            destination,
            destination,
        )
    }

    #[test]
    fn local_custom_message_has_no_core_effects() -> Result<()> {
        let local = SecretKey::random().address().into();
        let payload = custom_payload(local)?;

        assert!(custom_message_effects(local, &payload).is_empty());
        Ok(())
    }

    #[test]
    fn remote_custom_message_forwards_payload() -> Result<()> {
        let local = SecretKey::random().address().into();
        let remote = SecretKey::random().address().into();
        let payload = custom_payload(remote)?;
        let effects = custom_message_effects(local, &payload);

        assert_eq!(effects.len(), 1);
        match &effects[0] {
            CoreEffect::Payload(PayloadRelayFunctor::ForwardPayload {
                payload: forwarded,
                next_hop,
            }) => {
                assert_eq!(forwarded.as_ref(), &payload);
                assert_eq!(*next_hop, None);
            }
            effect => panic!("expected ForwardPayload, got {effect:?}"),
        }
        Ok(())
    }
}
