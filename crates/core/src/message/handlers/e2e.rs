use async_trait::async_trait;

use crate::error::Result;
use crate::message::e2e::E2eHandshakeRequest;
use crate::message::e2e::E2eHandshakeResponse;
use crate::message::e2e::E2eStreamFrame;
use crate::message::effects::CoreEffect;
use crate::message::handlers::HandlerContext;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

fn e2e_local_or_forward_effects<'payload>(
    local: crate::dht::Did,
    ctx: &'payload MessagePayload,
) -> Option<CoreEffect<'payload>> {
    if ctx.should_forward_from(local) {
        Some(CoreEffect::forward_payload(ctx, None))
    } else {
        None
    }
}

async fn run_e2e_local_or_forward<'payload>(
    handler: &MessageHandler,
    ctx: &'payload MessagePayload,
    local_effects: impl FnOnce() -> Result<Vec<CoreEffect<'payload>>>,
) -> Result<()> {
    if let Some(effect) = e2e_local_or_forward_effects(handler.dht.did, ctx) {
        return handler.run_effects([effect]).await;
    }

    handler.run_effects(local_effects()?).await
}

fn e2e_handshake_response_effect<'payload>(
    ctx: &MessagePayload,
    msg: &E2eHandshakeRequest,
    responder_public_key: crate::ecc::PublicKey<33>,
) -> Result<CoreEffect<'payload>> {
    msg.verify_requester(ctx.origin())?;
    Ok(CoreEffect::send_message(
        Message::E2eHandshakeResponse(E2eHandshakeResponse::new(responder_public_key)),
        ctx.origin(),
    ))
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<E2eHandshakeRequest> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &E2eHandshakeRequest) -> Result<()> {
        let ctx = context.payload();
        run_e2e_local_or_forward(self, ctx, || {
            let responder_public_key = self.transport.delegation().delegator_pubkey()?;
            Ok(vec![e2e_handshake_response_effect(
                ctx,
                msg,
                responder_public_key,
            )?])
        })
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<E2eHandshakeResponse> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &E2eHandshakeResponse) -> Result<()> {
        let ctx = context.payload();
        run_e2e_local_or_forward(self, ctx, || {
            msg.verify_responder(ctx.origin())?;
            Ok(Vec::new())
        })
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<E2eStreamFrame> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &E2eStreamFrame) -> Result<()> {
        let ctx = context.payload();
        run_e2e_local_or_forward(self, ctx, || {
            msg.verify_sender(ctx.origin())?;
            Ok(Vec::new())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use super::*;
    use crate::delegation::DelegateeKey;
    use crate::ecc::SecretKey;
    use crate::error::Error;
    use crate::message::e2e::encrypt_stream_with_rng;
    use crate::message::e2e::E2eHandshakeRequest;
    use crate::message::MessageSigner;
    use crate::tests::TEST_NETWORK_ID;

    fn e2e_payload(destination: crate::dht::Did) -> Result<MessagePayload> {
        let sender = SecretKey::random();
        let recipient = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey(&sender)?;
        let mut rng = rand_hc::Hc128Rng::from_entropy();
        let mut frames = encrypt_stream_with_rng(
            b"hello",
            uuid::Uuid::new_v4(),
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )?;
        let encrypted = frames
            .pop()
            .ok_or_else(|| Error::InvalidMessage("expected one E2E stream frame".to_string()))?;
        MessagePayload::new_send(
            Message::E2eStreamFrame(encrypted),
            MessageSigner::new(&delegatee_key, TEST_NETWORK_ID),
            destination,
            destination,
        )
    }

    fn signed_handshake_request(
        signer: &SecretKey,
        request: E2eHandshakeRequest,
        destination: crate::dht::Did,
    ) -> Result<MessagePayload> {
        let delegatee_key = DelegateeKey::new_with_seckey(signer)?;
        MessagePayload::new_send(
            Message::E2eHandshakeRequest(request),
            MessageSigner::new(&delegatee_key, TEST_NETWORK_ID),
            destination,
            destination,
        )
    }

    #[test]
    fn test_local_handshake_request_sends_responder_key_to_signer() -> Result<()> {
        let requester = SecretKey::random();
        let responder = SecretKey::random();
        let request = E2eHandshakeRequest::new(requester.pubkey());
        let payload = signed_handshake_request(&requester, request, responder.address().into())?;
        let effect = e2e_handshake_response_effect(&payload, &request, responder.pubkey())?;

        match effect {
            CoreEffect::SendMessage { msg, destination } => {
                assert_eq!(destination, requester.address().into());
                match *msg {
                    Message::E2eHandshakeResponse(response) => {
                        assert_eq!(response.responder_public_key, responder.pubkey());
                        response.verify_responder(responder.address().into())?;
                    }
                    msg => {
                        return Err(Error::InvalidMessage(format!(
                            "expected E2eHandshakeResponse, got {msg:?}"
                        )))
                    }
                }
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendMessage effect, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_local_handshake_request_rejects_key_not_owned_by_signer() -> Result<()> {
        let requester = SecretKey::random();
        let responder = SecretKey::random();
        let request = E2eHandshakeRequest::new(responder.pubkey());
        let payload = signed_handshake_request(&requester, request, responder.address().into())?;

        assert!(matches!(
            e2e_handshake_response_effect(&payload, &request, responder.pubkey()),
            Err(Error::E2ePublicKeyDidMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn test_remote_e2e_message_forwards_payload() -> Result<()> {
        let local = SecretKey::random().address().into();
        let remote = SecretKey::random().address().into();
        let payload = e2e_payload(remote)?;
        let effect = e2e_local_or_forward_effects(local, &payload)
            .ok_or_else(|| Error::InvalidMessage("expected ForwardPayload effect".to_string()))?;

        match effect {
            CoreEffect::ForwardPayload {
                payload: forwarded,
                next_hop,
            } => {
                assert!(std::ptr::eq(forwarded, &payload));
                assert_eq!(next_hop, None);
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected ForwardPayload, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_relayed_handshake_request_verifies_origin_and_replies_to_origin() -> Result<()> {
        let requester = SecretKey::random();
        let relay = SecretKey::random();
        let responder = SecretKey::random();
        let requester_did: crate::dht::Did = requester.address().into();
        let relay_did: crate::dht::Did = relay.address().into();
        let responder_did: crate::dht::Did = responder.address().into();

        let request = E2eHandshakeRequest::new(requester.pubkey());
        let initial = signed_handshake_request(&requester, request, responder_did)?;
        let relay_delegatee = DelegateeKey::new_with_seckey(&relay)?;
        let relayed = MessagePayload::new(
            initial.transaction,
            MessageSigner::new(&relay_delegatee, TEST_NETWORK_ID),
            initial.relay,
        )?;
        assert_eq!(relayed.origin(), requester_did);
        assert_eq!(relayed.last_hop_signer(), relay_did);
        assert_ne!(relayed.origin(), relayed.last_hop_signer());

        let effect = e2e_handshake_response_effect(&relayed, &request, responder.pubkey())?;
        match effect {
            CoreEffect::SendMessage { msg, destination } => {
                assert_eq!(destination, requester_did);
                assert_ne!(destination, relay_did);
                assert!(matches!(
                    *msg,
                    Message::E2eHandshakeResponse(response)
                        if response.verify_responder(responder_did).is_ok()
                ));
            }
            other => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendMessage effect, got {other:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_relayed_handshake_response_and_stream_frame_verify_origin() -> Result<()> {
        let sender = SecretKey::random();
        let relay = SecretKey::random();
        let recipient = SecretKey::random();
        let sender_did: crate::dht::Did = sender.address().into();
        let relay_did: crate::dht::Did = relay.address().into();
        let recipient_did: crate::dht::Did = recipient.address().into();
        let relay_delegatee = DelegateeKey::new_with_seckey(&relay)?;
        let relay_signer = MessageSigner::new(&relay_delegatee, TEST_NETWORK_ID);

        let response = E2eHandshakeResponse::new(sender.pubkey());
        let sender_delegatee = DelegateeKey::new_with_seckey(&sender)?;
        let initial_resp = MessagePayload::new_send(
            Message::E2eHandshakeResponse(response),
            MessageSigner::new(&sender_delegatee, TEST_NETWORK_ID),
            recipient_did,
            recipient_did,
        )?;
        let relayed_resp = MessagePayload::new(
            initial_resp.transaction,
            relay_signer.by_ref(),
            initial_resp.relay,
        )?;
        assert_eq!(relayed_resp.last_hop_signer(), relay_did);
        response.verify_responder(relayed_resp.origin())?;

        let mut rng = rand_hc::Hc128Rng::from_entropy();
        let frame = encrypt_stream_with_rng(
            b"relayed-e2e",
            uuid::Uuid::new_v4(),
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )?
        .pop()
        .ok_or_else(|| Error::InvalidMessage("expected one E2E stream frame".to_string()))?;
        let initial_frame = MessagePayload::new_send(
            Message::E2eStreamFrame(frame.clone()),
            MessageSigner::new(&sender_delegatee, TEST_NETWORK_ID),
            recipient_did,
            recipient_did,
        )?;
        let relayed_frame = MessagePayload::new(
            initial_frame.transaction,
            relay_signer,
            initial_frame.relay,
        )?;
        assert_eq!(relayed_frame.origin(), sender_did);
        assert_eq!(relayed_frame.last_hop_signer(), relay_did);
        frame.verify_sender(relayed_frame.origin())?;
        Ok(())
    }
}
