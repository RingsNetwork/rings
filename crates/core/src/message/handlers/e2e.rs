use async_trait::async_trait;

use crate::error::Result;
use crate::message::e2e::E2eHandshakeRequest;
use crate::message::e2e::E2eHandshakeResponse;
use crate::message::e2e::E2eStreamFrame;
use crate::message::effects::CoreEffect;
use crate::message::effects::MessageSendFunctor;
use crate::message::effects::PayloadRelayFunctor;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;

fn e2e_local_or_forward_effects<'payload>(
    local: crate::dht::Did,
    ctx: &'payload MessagePayload,
) -> Option<CoreEffect<'payload>> {
    if ctx.should_forward_from(local) {
        Some(PayloadRelayFunctor::forward_payload(ctx, None).into())
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
    msg.verify_requester(ctx.signer())?;
    Ok(MessageSendFunctor::send_message(
        Message::E2eHandshakeResponse(E2eHandshakeResponse::new(responder_public_key)),
        ctx.signer(),
    )
    .into())
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<E2eHandshakeRequest> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &E2eHandshakeRequest) -> Result<()> {
        run_e2e_local_or_forward(self, ctx, || {
            let responder_public_key = self.transport.session_sk().session().account_pubkey()?;
            Ok(vec![e2e_handshake_response_effect(
                ctx,
                msg,
                responder_public_key,
            )?])
        })
        .await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<E2eHandshakeResponse> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &E2eHandshakeResponse) -> Result<()> {
        run_e2e_local_or_forward(self, ctx, || {
            msg.verify_responder(ctx.signer())?;
            Ok(Vec::new())
        })
        .await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<E2eStreamFrame> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &E2eStreamFrame) -> Result<()> {
        run_e2e_local_or_forward(self, ctx, || {
            msg.verify_sender(ctx.signer())?;
            Ok(Vec::new())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use super::*;
    use crate::ecc::SecretKey;
    use crate::error::Error;
    use crate::message::e2e::encrypt_stream_with_rng;
    use crate::message::e2e::E2eHandshakeRequest;
    use crate::session::SessionSk;

    fn e2e_payload(destination: crate::dht::Did) -> Result<MessagePayload> {
        let sender = SecretKey::random();
        let recipient = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&sender)?;
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
            &session_sk,
            destination,
            destination,
        )
    }

    fn signed_handshake_request(
        signer: &SecretKey,
        request: E2eHandshakeRequest,
        destination: crate::dht::Did,
    ) -> Result<MessagePayload> {
        let session_sk = SessionSk::new_with_seckey(signer)?;
        MessagePayload::new_send(
            Message::E2eHandshakeRequest(request),
            &session_sk,
            destination,
            destination,
        )
    }

    #[test]
    fn local_handshake_request_sends_responder_key_to_signer() -> Result<()> {
        let requester = SecretKey::random();
        let responder = SecretKey::random();
        let request = E2eHandshakeRequest::new(requester.pubkey());
        let payload = signed_handshake_request(&requester, request, responder.address().into())?;
        let effect = e2e_handshake_response_effect(&payload, &request, responder.pubkey())?;

        match effect {
            CoreEffect::Message(MessageSendFunctor::SendMessage { msg, destination }) => {
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
    fn local_handshake_request_rejects_key_not_owned_by_signer() -> Result<()> {
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
    fn remote_e2e_message_forwards_payload() -> Result<()> {
        let local = SecretKey::random().address().into();
        let remote = SecretKey::random().address().into();
        let payload = e2e_payload(remote)?;
        let effect = e2e_local_or_forward_effects(local, &payload)
            .ok_or_else(|| Error::InvalidMessage("expected ForwardPayload effect".to_string()))?;

        match effect {
            CoreEffect::Payload(PayloadRelayFunctor::ForwardPayload {
                payload: forwarded,
                next_hop,
            }) => {
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
}
