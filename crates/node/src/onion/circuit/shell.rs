use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::session::SessionSk;
use rings_core::utils::get_epoch_ms;

use super::codec::encode_local_message;
use super::codec::OnionLocalMessage;
use super::crypto::decrypt_client_payload;
use super::crypto::decrypt_forward_layer;
use super::limiter::OnionCryptoGate;
use super::OnionAuthenticatedPayload;
use super::OnionCircuitEffect;
use super::OnionCircuitId;
use super::OnionCircuitPayload;
use super::OnionClientReturn;
use super::OnionForwardNonce;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Interpret;
use crate::extension::ext::Scope;

/// Interpreter for route-aware circuit effects.
pub struct OnionCircuitShell<H> {
    session_sk: SessionSk,
    crypto_gate: OnionCryptoGate,
    handler: H,
}

impl<H> OnionCircuitShell<H> {
    /// Create a circuit interpreter backed by `handler`.
    pub fn new(session_sk: SessionSk, handler: H) -> Self {
        Self {
            session_sk,
            crypto_gate: OnionCryptoGate::default(),
            handler,
        }
    }

    fn admit_crypto(&self, from: Did, now_ms: u128) -> Result<()> {
        self.crypto_gate.admit(from, now_ms)
    }

    fn decrypt_forward_reinject(
        &self,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: &rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext,
    ) -> Result<Option<Bytes>> {
        let received_at_ms = get_epoch_ms();
        match self.admit_crypto(from, received_at_ms) {
            Ok(()) => {}
            Err(Error::NoPermission) => {
                drop_bad_crypto("forward admission denied", Error::NoPermission);
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let layer = match decrypt_forward_layer(&self.session_sk, circuit_id, payload) {
            Ok(layer) => layer,
            Err(error) => {
                drop_bad_crypto("forward decrypt", error);
                return Ok(None);
            }
        };
        encode_local_message(OnionLocalMessage::ForwardReady {
            from,
            received_at_ms,
            circuit_id,
            layer,
        })
        .map(Some)
    }

    fn timestamp_backward_reinject(
        &self,
        from: Did,
        frame: super::OnionBackwardFrame,
    ) -> Result<Bytes> {
        encode_local_message(OnionLocalMessage::BackwardReady {
            from,
            received_at_ms: get_epoch_ms(),
            frame,
        })
    }

    fn decrypt_client_payload(
        &self,
        from: Did,
        payload: &rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext,
    ) -> Result<Option<OnionAuthenticatedPayload>> {
        let received_at_ms = get_epoch_ms();
        match self.admit_crypto(from, received_at_ms) {
            Ok(()) => {}
            Err(Error::NoPermission) => {
                drop_bad_crypto("client admission denied", Error::NoPermission);
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        match decrypt_client_payload(&self.session_sk, payload) {
            Ok(payload) => Ok(Some(payload)),
            Err(error) => {
                drop_bad_crypto("client decrypt", error);
                Ok(None)
            }
        }
    }
}

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl<H> Interpret for OnionCircuitShell<H>
where H: OnionCircuitHandler + crate::extension::ext::MaybeSend + 'static
{
    type Effect = OnionCircuitEffect;

    async fn run(&self, scope: &Scope, effect: OnionCircuitEffect) -> Result<Vec<Bytes>> {
        match effect {
            OnionCircuitEffect::DecryptForward {
                from,
                circuit_id,
                payload,
            } => Ok(self
                .decrypt_forward_reinject(from, circuit_id, &payload)?
                .into_iter()
                .collect()),
            OnionCircuitEffect::TimestampBackward { from, frame } => self
                .timestamp_backward_reinject(from, frame)
                .map(|payload| vec![payload]),
            OnionCircuitEffect::Send { to, payload } => {
                scope.send(to, payload).await?;
                Ok(Vec::new())
            }
            OnionCircuitEffect::Exit {
                from,
                circuit_id,
                return_peer,
                client,
                forward_nonce,
                payload,
            } => {
                self.handler
                    .handle_exit(scope, OnionCircuitExitFrame {
                        from,
                        circuit_id,
                        return_peer,
                        client,
                        forward_nonce,
                        payload,
                    })
                    .await?;
                Ok(Vec::new())
            }
            OnionCircuitEffect::DecryptClient {
                from,
                circuit_id,
                payload,
            } => {
                if let Some(payload) = self.decrypt_client_payload(from, &payload)? {
                    self.handler
                        .handle_client(scope, from, circuit_id, payload)
                        .await?;
                }
                Ok(Vec::new())
            }
        }
    }
}

/// Fully decrypted forward frame that has reached the exit adapter.
#[derive(Clone, Debug)]
pub struct OnionCircuitExitFrame {
    /// Previous peer that delivered this exit frame.
    pub from: Did,
    /// Edge-local circuit id for the exit-to-return-peer edge.
    pub circuit_id: OnionCircuitId,
    /// Relay peer that should receive backward frames from the exit.
    pub return_peer: Did,
    /// Client return key encrypted into the exit layer.
    pub client: OnionClientReturn,
    /// Exit-layer nonce consumed once by the adapter before side effects.
    pub forward_nonce: OnionForwardNonce,
    /// Adapter payload carried by the exit layer.
    pub payload: OnionCircuitPayload,
}

/// Runtime-specific circuit handling.
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub trait OnionCircuitHandler {
    /// Handle a frame that reached this node as the exit.
    async fn handle_exit(&self, scope: &Scope, frame: OnionCircuitExitFrame) -> Result<()>;

    /// Handle a frame that reached this node as the client.
    async fn handle_client(
        &self,
        scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> Result<()>;
}

fn drop_bad_crypto(context: &str, error: Error) {
    tracing::debug!("drop onion circuit message after {context}: {error}");
}
