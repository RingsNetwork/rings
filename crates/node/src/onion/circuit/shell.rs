use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::session::SessionSk;
use rings_core::utils::get_epoch_ms;

use super::cell::open_cell;
use super::cell::seal_encoded_message;
use super::codec::encode_local_message;
use super::codec::OnionLocalMessage;
use super::crypto::decrypt_client_payload;
use super::crypto::decrypt_forward_layer;
use super::limiter::OnionCryptoGate;
use super::send_outbox::OnionLinkSender;
#[cfg(all(test, rings_native))]
use super::send_outbox::OnionSendTestHook;
use super::OnionAuthenticatedPayload;
use super::OnionCellBucket;
use super::OnionCircuitEffect;
use super::OnionCircuitId;
use super::OnionCircuitPayload;
use super::OnionClientReturn;
use super::OnionForwardNonce;
use super::OnionForwardSequence;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::EffectScope;
use crate::extension::ext::Interpret;
use crate::extension::ext::Scope;
use crate::extension::transport::platform::spawn_detached;

/// Interpreter for route-aware circuit effects.
pub struct OnionCircuitShell<H> {
    session_sk: SessionSk,
    crypto_gate: OnionCryptoGate,
    link_sender: OnionLinkSender,
    handler: Arc<H>,
}

impl<H> OnionCircuitShell<H> {
    /// Create a circuit interpreter backed by `handler`.
    pub fn new(session_sk: SessionSk, handler: H) -> Self {
        Self {
            session_sk,
            crypto_gate: OnionCryptoGate::default(),
            link_sender: OnionLinkSender::default(),
            handler: Arc::new(handler),
        }
    }

    #[cfg(all(test, rings_native))]
    pub(super) fn new_with_send_test_hook(
        session_sk: SessionSk,
        handler: H,
        test_hook: Arc<OnionSendTestHook>,
    ) -> Self {
        Self {
            session_sk,
            crypto_gate: OnionCryptoGate::default(),
            link_sender: OnionLinkSender::with_test_hook(test_hook),
            handler: Arc::new(handler),
        }
    }

    /// Create an interpreter sharing one node-level link sender with endpoint adapters.
    pub(crate) fn with_link_sender(
        session_sk: SessionSk,
        handler: H,
        link_sender: OnionLinkSender,
    ) -> Self {
        Self {
            session_sk,
            crypto_gate: OnionCryptoGate::default(),
            link_sender,
            handler: Arc::new(handler),
        }
    }

    fn admit_crypto(&self, from: Did, now_ms: u128, visible_cell_bytes: u64) -> Result<()> {
        self.crypto_gate.admit(from, now_ms, visible_cell_bytes)
    }

    fn decrypt_cell_reinject(
        &self,
        from: Did,
        bucket: OnionCellBucket,
        sealed: &rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext,
    ) -> Result<Option<Bytes>> {
        let received_at_ms = get_epoch_ms();
        let visible_cell_bytes = u64::try_from(bucket.plaintext_len())
            .map_err(|_| Error::OnionRouteError(crate::onion::OnionRouteError::InvalidCell))?;
        match self.admit_crypto(from, received_at_ms, visible_cell_bytes) {
            Ok(()) => {}
            Err(Error::NoPermission) => {
                drop_bad_crypto("forward admission denied", Error::NoPermission);
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let message = match open_cell(&self.session_sk, bucket, sealed) {
            Ok(message) => message,
            Err(error) => {
                drop_bad_crypto("cell decrypt", error);
                return Ok(None);
            }
        };
        encode_local_message(OnionLocalMessage::CellReady {
            from,
            received_at_ms,
            bucket,
            message,
        })
        .map(Some)
    }

    fn decrypt_forward_reinject(
        &self,
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        circuit_id: OnionCircuitId,
        payload: &rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext,
    ) -> Result<Option<Bytes>> {
        match self.admit_crypto(from, received_at_ms, 0) {
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
            bucket,
            circuit_id,
            layer,
        })
        .map(Some)
    }

    fn decrypt_client_payload(
        &self,
        from: Did,
        payload: &rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext,
    ) -> Result<Option<OnionAuthenticatedPayload>> {
        let received_at_ms = get_epoch_ms();
        match self.admit_crypto(from, received_at_ms, 0) {
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

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl<H> Interpret for OnionCircuitShell<H>
where H: OnionCircuitHandler + crate::extension::ext::MaybeSend + 'static
{
    type Effect = OnionCircuitEffect;

    async fn run(&self, scope: &EffectScope, effect: OnionCircuitEffect) -> Result<Vec<Bytes>> {
        match effect {
            OnionCircuitEffect::DecryptCell {
                from,
                bucket,
                sealed,
            } => Ok(self
                .decrypt_cell_reinject(from, bucket, &sealed)?
                .into_iter()
                .collect()),
            OnionCircuitEffect::DecryptForward {
                from,
                received_at_ms,
                bucket,
                circuit_id,
                payload,
            } => Ok(self
                .decrypt_forward_reinject(from, received_at_ms, bucket, circuit_id, &payload)?
                .into_iter()
                .collect()),
            OnionCircuitEffect::SealAndSend {
                to,
                recipient,
                bucket,
                encoded_message,
            } => {
                let payload = seal_encoded_message(&encoded_message, recipient, Some(bucket))?;
                self.link_sender.enqueue_sealed(
                    scope.lifecycle(),
                    super::OnionLink::new(to, recipient),
                    payload,
                )?;
                Ok(Vec::new())
            }
            OnionCircuitEffect::Exit {
                from,
                circuit_id,
                return_peer,
                return_session_public_key,
                client,
                forward_nonce,
                forward_sequence,
                payload,
            } => {
                let lifecycle = scope.lifecycle();
                let handler = Arc::clone(&self.handler);
                spawn_detached(async move {
                    let result = handler
                        .handle_exit(&lifecycle, OnionCircuitExitFrame {
                            from,
                            circuit_id,
                            return_peer,
                            return_session_public_key,
                            client,
                            forward_nonce,
                            forward_sequence,
                            payload,
                        })
                        .await;
                    if let Err(error) = result {
                        tracing::warn!(%error, "onion exit effect failed");
                    }
                });
                Ok(Vec::new())
            }
            OnionCircuitEffect::DecryptClient {
                from,
                circuit_id,
                payload,
            } => {
                if let Some(payload) = self.decrypt_client_payload(from, &payload)? {
                    let lifecycle = scope.lifecycle();
                    self.handler
                        .handle_client(&lifecycle, from, circuit_id, payload)
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
    /// Session key of the immediate return peer used to encrypt the first backward cell.
    pub return_session_public_key: PublicKey<33>,
    /// Client return key encrypted into the exit layer.
    pub client: OnionClientReturn,
    /// One-shot replay token consumed by `Open`/HTTPS exit operations before side effects.
    pub forward_nonce: OnionForwardNonce,
    /// Monotonic client-to-exit sequence within this circuit.
    pub forward_sequence: OnionForwardSequence,
    /// Adapter payload carried by the exit layer.
    pub payload: OnionCircuitPayload,
}

/// Runtime-specific circuit handling.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
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
