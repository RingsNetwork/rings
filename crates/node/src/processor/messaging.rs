//! Messages a processor sends: plain and direct custom messages, extension envelopes, and the
//! end-to-end encrypted handshake and stream.

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::ecc::SecretKey;
use rings_core::message::e2e;
use rings_core::message::e2e::E2eHandshakeRequest;
use rings_core::message::e2e::E2eHandshakeResponse;
use rings_core::message::e2e::E2eStreamDecryptor;
use rings_core::message::e2e::E2eStreamFrame;
use rings_core::message::Message;

use super::Processor;
use crate::error::Error;
use crate::error::Result;

impl Processor {
    /// Send custom message to a did.
    pub async fn send_message(&self, destination: Did, msg: &[u8]) -> Result<uuid::Uuid> {
        tracing::trace!("send_message, message size: {:?}", msg.len());

        let msg = Message::custom(msg).map_err(Error::SendMessage)?;

        self.swarm
            .send_message(msg, destination)
            .await
            .map_err(Error::SendMessage)
    }

    /// Send a custom message to an already connected peer without Chord routing.
    ///
    /// Protocols with their own authenticated hop selection, such as onion circuits, use this
    /// to keep the core transport from replacing their selected next hop.
    pub async fn send_direct_message(&self, destination: Did, msg: &[u8]) -> Result<uuid::Uuid> {
        tracing::trace!("send_direct_message, message size: {:?}", msg.len());

        let msg = Message::custom(msg).map_err(Error::SendMessage)?;

        self.swarm
            .send_direct_message(msg, destination)
            .await
            .map_err(Error::SendMessage)
    }

    /// Send an E2E handshake request to a DID.
    ///
    /// The negotiated key is the peer's account/identity secp256k1 key, not
    /// the ephemeral delegatee key.
    pub async fn send_e2e_handshake(&self, destination: Did) -> Result<uuid::Uuid> {
        let public_key = self.swarm.delegator_pubkey().map_err(Error::SendMessage)?;
        self.swarm
            .send_message(
                Message::E2eHandshakeRequest(E2eHandshakeRequest::new(public_key)),
                destination,
            )
            .await
            .map_err(Error::SendMessage)
    }

    /// Send an ElGamal-encrypted E2E message to a DID with a verified recipient key.
    ///
    /// Returns the stream id shared by all emitted E2E stream frames.
    pub async fn send_e2e_message(
        &self,
        destination: Did,
        recipient_public_key: PublicKey<33>,
        msg: &[u8],
    ) -> Result<uuid::Uuid> {
        self.send_e2e_message_with_frame_len(
            destination,
            recipient_public_key,
            msg,
            e2e::DEFAULT_E2E_PLAINTEXT_FRAME_LEN,
        )
        .await
    }

    /// Send an ElGamal-encrypted E2E stream with an explicit plaintext frame size.
    ///
    /// Returns the stream id shared by all emitted E2E stream frames.
    pub async fn send_e2e_message_with_frame_len(
        &self,
        destination: Did,
        recipient_public_key: PublicKey<33>,
        msg: &[u8],
        max_plaintext_frame_len: usize,
    ) -> Result<uuid::Uuid> {
        e2e::ensure_public_key_matches_did(recipient_public_key, destination)
            .map_err(Error::SendMessage)?;
        let sender_public_key = self.swarm.delegator_pubkey().map_err(Error::SendMessage)?;
        let stream_id = uuid::Uuid::new_v4();
        let frames = e2e::encrypt_stream_frames(
            msg,
            stream_id,
            sender_public_key,
            recipient_public_key,
            max_plaintext_frame_len,
        )
        .map_err(Error::SendMessage)?
        .collect::<rings_core::error::Result<Vec<_>>>()
        .map_err(Error::SendMessage)?;

        for frame in frames {
            self.swarm
                .send_message(Message::E2eStreamFrame(frame), destination)
                .await
                .map_err(Error::SendMessage)?;
        }

        Ok(stream_id)
    }

    /// Verify an E2E handshake request and return the requester's identity public key.
    pub fn verify_e2e_handshake_request(
        &self,
        requester: Did,
        request: &E2eHandshakeRequest,
    ) -> Result<PublicKey<33>> {
        request
            .verify_requester(requester)
            .map_err(Error::CoreError)?;
        Ok(request.requester_public_key)
    }

    /// Verify an E2E handshake response and return the responder's identity public key.
    pub fn verify_e2e_handshake_response(
        &self,
        responder: Did,
        response: &E2eHandshakeResponse,
    ) -> Result<PublicKey<33>> {
        response
            .verify_responder(responder)
            .map_err(Error::CoreError)?;
        Ok(response.responder_public_key)
    }

    /// Create an E2E stream decryptor with this node's identity/signing secret key.
    ///
    /// The ciphertext is encrypted to the DID/account key negotiated by the
    /// handshake. A session private key cannot decrypt it unless the delegatee key
    /// is also the account key, so callers must supply the local identity key
    /// explicitly.
    pub fn e2e_stream_decryptor(
        &self,
        expected_sender: Did,
        stream_id: e2e::E2eStreamId,
        recipient_identity_key: SecretKey,
    ) -> Result<E2eStreamDecryptor> {
        e2e::ensure_public_key_matches_did(recipient_identity_key.pubkey(), self.did())
            .map_err(Error::CoreError)?;
        Ok(E2eStreamDecryptor::new(
            stream_id,
            expected_sender,
            recipient_identity_key,
        ))
    }

    /// Decrypt one E2E stream frame with an already-created stream decryptor.
    pub fn decrypt_e2e_stream_frame(
        &self,
        decryptor: &mut E2eStreamDecryptor,
        frame: &E2eStreamFrame,
    ) -> Result<Vec<u8>> {
        decryptor.decrypt_next(frame).map_err(Error::CoreError)
    }

    /// Send a namespaced [`Envelope`](crate::extension::ext::Envelope) to a did over the
    /// P2P transport (the wire codec
    /// of the extension layer). `send_envelope : (Did, Envelope) → IO TxId`.
    pub async fn send_envelope(
        &self,
        destination: Did,
        envelope: &crate::extension::ext::Envelope,
    ) -> Result<uuid::Uuid> {
        let msg_bytes = envelope.encode()?;
        self.send_message(destination, &msg_bytes).await
    }

    /// Send a namespaced envelope directly to an already connected peer.
    ///
    /// This bypasses Chord routing while retaining the normal custom-message envelope codec.
    pub async fn send_direct_envelope(
        &self,
        destination: Did,
        envelope: &crate::extension::ext::Envelope,
    ) -> Result<uuid::Uuid> {
        let msg_bytes = envelope.encode()?;
        self.send_direct_message(destination, &msg_bytes).await
    }
}
