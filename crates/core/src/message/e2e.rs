//! End-to-end ElGamal encryption for byte payloads.
//!
//! This module models E2E encryption as two separate protocol facts:
//!
//! - peers discover encryption public keys with signed handshake messages;
//! - message bodies are encrypted directly with secp256k1 ElGamal stream
//!   frames, each carried by its own signed
//!   [`MessagePayload`](crate::message::MessagePayload).
//!
//! Rings intentionally uses the DID/account secp256k1 public key as the
//! ElGamal key. The public key carried by a handshake or encrypted message must
//! resolve to the signed DID; this is the protocol's key-ownership proof. The
//! module does not derive a separate encryption subkey.
//!
//! The direct ElGamal body is deliberately not KEM/DEM or AEAD. Keeping the
//! ciphertext in group-element form preserves the algebraic structure needed by
//! future homomorphic operations. That also means ciphertext frames are
//! malleable if detached from the signed message envelope. Integrity, sender
//! authentication, public-key ownership, and per-frame integrity are provided by
//! the surrounding [`MessagePayload`](crate::message::MessagePayload)
//! signature. The stream id, sequence, and final marker make truncation
//! observable and let the decryptor release reordered frames as a gapless
//! plaintext stream.

use std::collections::BTreeMap;

use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::ecc::elgamal::impls::secp256k1;
use crate::ecc::group::Point;
use crate::ecc::group::Secp256k1;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// Plaintext bytes carried by one ElGamal body block.
pub const E2E_PLAINTEXT_BLOCK_LEN: usize = secp256k1::PLAINTEXT_BLOCK_SIZE;

/// Default plaintext bytes per encrypted E2E stream frame.
pub const DEFAULT_E2E_PLAINTEXT_FRAME_LEN: usize = E2E_PLAINTEXT_BLOCK_LEN * 16;

/// Default maximum number of out-of-order future frames buffered per stream.
pub const DEFAULT_E2E_REORDER_WINDOW_FRAMES: u64 = 64;

/// Identifier shared by all frames of one encrypted E2E stream.
pub type E2eStreamId = uuid::Uuid;

/// An invitation to start E2E encryption with the requester's public key.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct E2eHandshakeRequest {
    /// Public key owned by the requester DID.
    pub requester_public_key: PublicKey<33>,
}

/// An acceptance of an E2E handshake with the responder's public key.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct E2eHandshakeResponse {
    /// Public key owned by the responder DID.
    pub responder_public_key: PublicKey<33>,
}

/// One ordered ElGamal-encrypted stream frame.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct E2eStreamFrame {
    /// Stream identifier shared by all frames in this body stream.
    pub stream_id: E2eStreamId,
    /// Public key owned by the signed sender DID.
    pub sender_public_key: PublicKey<33>,
    /// Monotonic frame counter checked by the decryptor.
    pub sequence: u64,
    /// End-of-stream marker authenticated by this frame's message signature.
    pub is_final: bool,
    /// ElGamal ciphertext blocks for this frame's plaintext bytes.
    pub ciphertext: Vec<secp256k1::CiphertextBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct E2ePlaintextFrame<'a> {
    plaintext: &'a [u8],
    is_final: bool,
}

struct E2ePlaintextFrames<'a> {
    chunks: std::iter::Peekable<std::slice::Chunks<'a, u8>>,
    plaintext_is_empty: bool,
    emitted_empty_final: bool,
}

/// Lazy iterator that encrypts one E2E stream frame per iteration.
pub struct E2eStreamFrames<'a, R: RngCore> {
    plaintext_frames: E2ePlaintextFrames<'a>,
    encryptor: E2eStreamEncryptor,
    rng: R,
}

/// Stateful streaming encryptor for direct ElGamal body frames.
pub struct E2eStreamEncryptor {
    stream_id: E2eStreamId,
    sender_public_key: PublicKey<33>,
    recipient_public_key: PublicKey<33>,
    next_sequence: u64,
    closed: bool,
}

/// Stateful streaming decryptor for direct ElGamal body frames.
pub struct E2eStreamDecryptor {
    stream_id: E2eStreamId,
    expected_sender: Did,
    recipient_secret_key: SecretKey,
    next_sequence: u64,
    final_sequence: Option<u64>,
    seen_final: bool,
    reorder_window: u64,
    pending_frames: BTreeMap<u64, E2eStreamFrame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameAcceptance {
    New,
    Duplicate,
}

impl<'a> E2ePlaintextFrames<'a> {
    fn new(plaintext: &'a [u8], max_plaintext_frame_len: usize) -> Self {
        Self {
            chunks: plaintext.chunks(max_plaintext_frame_len.max(1)).peekable(),
            plaintext_is_empty: plaintext.is_empty(),
            emitted_empty_final: false,
        }
    }
}

impl<R: RngCore> Iterator for E2eStreamFrames<'_, R> {
    type Item = Result<E2eStreamFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        self.plaintext_frames
            .next()
            .map(|frame| self.encryptor.encrypt_plaintext_frame(frame, &mut self.rng))
    }
}

impl<'a> Iterator for E2ePlaintextFrames<'a> {
    type Item = E2ePlaintextFrame<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.plaintext_is_empty {
            if self.emitted_empty_final {
                return None;
            }
            self.emitted_empty_final = true;
            return Some(E2ePlaintextFrame {
                plaintext: &[],
                is_final: true,
            });
        }

        let plaintext = self.chunks.next()?;
        Some(E2ePlaintextFrame {
            plaintext,
            is_final: self.chunks.peek().is_none(),
        })
    }
}

impl E2eHandshakeRequest {
    /// Build a handshake request for a sender public key.
    pub fn new(requester_public_key: PublicKey<33>) -> Self {
        Self {
            requester_public_key,
        }
    }

    /// Verify that the requester public key belongs to the signed requester DID.
    pub fn verify_requester(&self, requester: Did) -> Result<()> {
        ensure_public_key_matches_did(self.requester_public_key, requester)
    }
}

impl E2eHandshakeResponse {
    /// Build a handshake response for a responder public key.
    pub fn new(responder_public_key: PublicKey<33>) -> Self {
        Self {
            responder_public_key,
        }
    }

    /// Verify that the responder public key belongs to the signed responder DID.
    pub fn verify_responder(&self, responder: Did) -> Result<()> {
        ensure_public_key_matches_did(self.responder_public_key, responder)
    }
}

impl E2eStreamFrame {
    /// Verify that the carried sender key belongs to the signed sender DID.
    pub fn verify_sender(&self, sender: Did) -> Result<()> {
        ensure_public_key_matches_did(self.sender_public_key, sender)
    }
}

impl E2eStreamEncryptor {
    /// Create a direct ElGamal streaming encryptor for `recipient_public_key`.
    pub fn new(
        stream_id: E2eStreamId,
        sender_public_key: PublicKey<33>,
        recipient_public_key: PublicKey<33>,
    ) -> Result<Self> {
        ensure_valid_public_key(sender_public_key)?;
        ensure_valid_public_key(recipient_public_key)?;
        Ok(Self {
            stream_id,
            sender_public_key,
            recipient_public_key,
            next_sequence: 0,
            closed: false,
        })
    }

    /// Encrypt one non-final plaintext frame.
    pub fn encrypt_next(
        &mut self,
        plaintext: &[u8],
        rng: &mut impl RngCore,
    ) -> Result<E2eStreamFrame> {
        self.encrypt_frame(plaintext, false, rng)
    }

    /// Encrypt one non-final plaintext frame with the default thread-local RNG.
    pub fn encrypt_next_with_default_rng(&mut self, plaintext: &[u8]) -> Result<E2eStreamFrame> {
        let mut rng = rand::thread_rng();
        self.encrypt_next(plaintext, &mut rng)
    }

    /// Encrypt the final plaintext frame and close this stream.
    pub fn encrypt_final(
        &mut self,
        plaintext: &[u8],
        rng: &mut impl RngCore,
    ) -> Result<E2eStreamFrame> {
        self.encrypt_frame(plaintext, true, rng)
    }

    /// Encrypt the final plaintext frame with the default thread-local RNG.
    pub fn encrypt_final_with_default_rng(&mut self, plaintext: &[u8]) -> Result<E2eStreamFrame> {
        let mut rng = rand::thread_rng();
        self.encrypt_final(plaintext, &mut rng)
    }

    fn encrypt_plaintext_frame(
        &mut self,
        frame: E2ePlaintextFrame<'_>,
        rng: &mut impl RngCore,
    ) -> Result<E2eStreamFrame> {
        if frame.is_final {
            self.encrypt_final(frame.plaintext, rng)
        } else {
            self.encrypt_next(frame.plaintext, rng)
        }
    }

    fn encrypt_frame(
        &mut self,
        plaintext: &[u8],
        is_final: bool,
        rng: &mut impl RngCore,
    ) -> Result<E2eStreamFrame> {
        if self.closed {
            return Err(Error::E2eFrameAfterFinal);
        }

        let sequence = self.next_sequence;
        if is_final {
            self.closed = true;
        } else {
            self.next_sequence = next_sequence(sequence)?;
        }

        let ciphertext =
            secp256k1::encrypt_bytes_with_rng(plaintext, self.recipient_public_key, rng)?;
        Ok(E2eStreamFrame {
            stream_id: self.stream_id,
            sender_public_key: self.sender_public_key,
            sequence,
            is_final,
            ciphertext,
        })
    }
}

impl E2eStreamDecryptor {
    /// Create a direct ElGamal streaming decryptor for a recipient secret key.
    pub fn new(
        stream_id: E2eStreamId,
        expected_sender: Did,
        recipient_secret_key: SecretKey,
    ) -> Self {
        Self::with_reorder_window(
            stream_id,
            expected_sender,
            recipient_secret_key,
            DEFAULT_E2E_REORDER_WINDOW_FRAMES,
        )
    }

    /// Create a decryptor with an explicit future-frame reorder window.
    pub fn with_reorder_window(
        stream_id: E2eStreamId,
        expected_sender: Did,
        recipient_secret_key: SecretKey,
        reorder_window: u64,
    ) -> Self {
        Self {
            stream_id,
            expected_sender,
            recipient_secret_key,
            next_sequence: 0,
            final_sequence: None,
            seen_final: false,
            reorder_window,
            pending_frames: BTreeMap::new(),
        }
    }

    /// Decrypt one frame and return newly contiguous plaintext bytes.
    ///
    /// Out-of-order future frames are buffered and return an empty vector until
    /// the missing lower sequence numbers arrive.
    pub fn decrypt_next(&mut self, frame: &E2eStreamFrame) -> Result<Vec<u8>> {
        if self.validate_frame(frame)? == FrameAcceptance::Duplicate {
            return Ok(Vec::new());
        }

        if frame.is_final {
            self.final_sequence = Some(frame.sequence);
        }
        self.pending_frames.insert(frame.sequence, frame.clone());
        self.decrypt_ready_frames()
    }

    /// Verify that the stream ended with an authenticated final frame.
    pub fn finish(&self) -> Result<()> {
        if self.seen_final {
            Ok(())
        } else {
            Err(Error::E2eMissingFinalFrame)
        }
    }

    fn validate_frame(&self, frame: &E2eStreamFrame) -> Result<FrameAcceptance> {
        if frame.stream_id != self.stream_id {
            return Err(Error::E2eStreamIdMismatch {
                expected: self.stream_id,
                actual: frame.stream_id,
            });
        }

        frame.verify_sender(self.expected_sender)?;

        if self.is_consumed_duplicate(frame) {
            return Ok(FrameAcceptance::Duplicate);
        }

        if let Some(pending_frame) = self.pending_frames.get(&frame.sequence) {
            if pending_frame == frame {
                return Ok(FrameAcceptance::Duplicate);
            }

            return Err(Error::E2eFrameSequenceMismatch {
                expected: self.next_sequence,
                actual: frame.sequence,
            });
        }

        if self.seen_final {
            return Err(Error::E2eFrameAfterFinal);
        }

        if self.exceeds_reorder_window(frame.sequence) {
            return Err(Error::E2eFrameReorderWindowExceeded {
                next_sequence: self.next_sequence,
                actual: frame.sequence,
                window: self.reorder_window,
            });
        }

        if let Some(final_sequence) = self.final_sequence {
            if frame.sequence > final_sequence {
                return Err(Error::E2eFrameAfterFinal);
            }

            if frame.is_final && frame.sequence != final_sequence {
                return Err(Error::E2eFrameSequenceMismatch {
                    expected: final_sequence,
                    actual: frame.sequence,
                });
            }
        }

        if frame.is_final && self.has_pending_frame_after(frame.sequence) {
            return Err(Error::E2eFrameAfterFinal);
        }

        Ok(FrameAcceptance::New)
    }

    fn decrypt_ready_frames(&mut self) -> Result<Vec<u8>> {
        let mut plaintext = Vec::new();

        while let Some(frame) = self.pending_frames.get(&self.next_sequence) {
            let frame_plaintext =
                secp256k1::decrypt_bytes(&frame.ciphertext, self.recipient_secret_key)?;
            let is_final = frame.is_final;
            plaintext.extend_from_slice(&frame_plaintext);
            self.pending_frames.remove(&self.next_sequence);

            if is_final {
                self.seen_final = true;
                break;
            }

            self.next_sequence = next_sequence(self.next_sequence)?;
        }

        Ok(plaintext)
    }

    fn has_pending_frame_after(&self, sequence: u64) -> bool {
        self.pending_frames
            .last_key_value()
            .is_some_and(|(pending_sequence, _)| *pending_sequence > sequence)
    }

    fn is_consumed_duplicate(&self, frame: &E2eStreamFrame) -> bool {
        if frame.sequence < self.next_sequence {
            return true;
        }

        self.seen_final
            && self
                .final_sequence
                .is_some_and(|final_sequence| frame.sequence <= final_sequence)
    }

    fn exceeds_reorder_window(&self, sequence: u64) -> bool {
        sequence.saturating_sub(self.next_sequence) > self.reorder_window
    }
}

fn plaintext_stream_frames(
    plaintext: &[u8],
    max_plaintext_frame_len: usize,
) -> E2ePlaintextFrames<'_> {
    E2ePlaintextFrames::new(plaintext, max_plaintext_frame_len)
}

/// Encrypt a byte slice lazily into direct-ElGamal E2E stream frames.
pub fn encrypt_stream_frames_with_rng<R: RngCore>(
    plaintext: &[u8],
    stream_id: E2eStreamId,
    sender_public_key: PublicKey<33>,
    recipient_public_key: PublicKey<33>,
    max_plaintext_frame_len: usize,
    rng: R,
) -> Result<E2eStreamFrames<'_, R>> {
    Ok(E2eStreamFrames {
        plaintext_frames: plaintext_stream_frames(plaintext, max_plaintext_frame_len),
        encryptor: E2eStreamEncryptor::new(stream_id, sender_public_key, recipient_public_key)?,
        rng,
    })
}

/// Encrypt a byte slice lazily with the default thread-local RNG.
pub fn encrypt_stream_frames(
    plaintext: &[u8],
    stream_id: E2eStreamId,
    sender_public_key: PublicKey<33>,
    recipient_public_key: PublicKey<33>,
    max_plaintext_frame_len: usize,
) -> Result<E2eStreamFrames<'_, rand::rngs::ThreadRng>> {
    encrypt_stream_frames_with_rng(
        plaintext,
        stream_id,
        sender_public_key,
        recipient_public_key,
        max_plaintext_frame_len,
        rand::thread_rng(),
    )
}

/// Encrypt a byte slice into direct-ElGamal E2E stream frames.
pub fn encrypt_stream_with_rng(
    plaintext: &[u8],
    stream_id: E2eStreamId,
    sender_public_key: PublicKey<33>,
    recipient_public_key: PublicKey<33>,
    max_plaintext_frame_len: usize,
    rng: &mut impl RngCore,
) -> Result<Vec<E2eStreamFrame>> {
    encrypt_stream_frames_with_rng(
        plaintext,
        stream_id,
        sender_public_key,
        recipient_public_key,
        max_plaintext_frame_len,
        rng,
    )?
    .collect()
}

/// Decrypt a complete direct-ElGamal E2E frame sequence into bytes.
pub fn decrypt_stream(
    frames: &[E2eStreamFrame],
    stream_id: E2eStreamId,
    expected_sender: Did,
    recipient_secret_key: SecretKey,
) -> Result<Vec<u8>> {
    let mut decryptor = E2eStreamDecryptor::new(stream_id, expected_sender, recipient_secret_key);
    let mut plaintext = Vec::new();

    for frame in frames {
        let frame_plaintext = decryptor.decrypt_next(frame)?;
        plaintext.extend_from_slice(&frame_plaintext);
    }

    decryptor.finish()?;
    Ok(plaintext)
}

/// Verify that a public key hashes to the expected DID.
pub fn ensure_public_key_matches_did(public_key: PublicKey<33>, expected: Did) -> Result<()> {
    let actual = Did::from(public_key.address());
    if actual == expected {
        Ok(())
    } else {
        Err(Error::E2ePublicKeyDidMismatch { expected, actual })
    }
}

fn ensure_valid_public_key(public_key: PublicKey<33>) -> Result<()> {
    let _: Point<Secp256k1> = public_key.try_into()?;
    Ok(())
}

fn next_sequence(sequence: u64) -> Result<u64> {
    sequence
        .checked_add(1)
        .ok_or(Error::E2eFrameSequenceOverflow)
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand_hc::Hc128Rng;

    use super::*;

    fn recipient_key() -> SecretKey {
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap()
    }

    fn sender_key() -> SecretKey {
        SecretKey::try_from("1f9275dbafdfba81942eb3330b07f38cbee4ebb86bdc2174af9648d5f5509a54")
            .unwrap()
    }

    #[test]
    fn public_key_must_match_did() {
        let sender = sender_key();
        let recipient = recipient_key();

        assert!(ensure_public_key_matches_did(sender.pubkey(), sender.address().into()).is_ok());
        assert!(matches!(
            ensure_public_key_matches_did(sender.pubkey(), recipient.address().into()),
            Err(Error::E2ePublicKeyDidMismatch { .. })
        ));
    }

    #[test]
    fn handshake_messages_verify_signed_owner() {
        let sender = sender_key();
        let recipient = recipient_key();
        let request = E2eHandshakeRequest::new(sender.pubkey());
        let response = E2eHandshakeResponse::new(recipient.pubkey());

        request.verify_requester(sender.address().into()).unwrap();
        response
            .verify_responder(recipient.address().into())
            .unwrap();
        assert!(request
            .verify_requester(recipient.address().into())
            .is_err());
        assert!(response.verify_responder(sender.address().into()).is_err());
    }

    #[test]
    fn round_trip_random_binary_payloads_and_frame_sizes() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(608);
        let payload_lens = [0usize, 1, 15, 16, 17, 31, 32, 255, 1024, 4097];
        let frame_limits = [1usize, E2E_PLAINTEXT_BLOCK_LEN, 64, 511];

        for payload_len in payload_lens {
            for frame_limit in frame_limits {
                let mut payload = vec![0u8; payload_len];
                rng.fill_bytes(&mut payload);
                let stream_id = uuid::Uuid::new_v4();

                let frames = encrypt_stream_with_rng(
                    &payload,
                    stream_id,
                    sender.pubkey(),
                    recipient.pubkey(),
                    frame_limit,
                    &mut rng,
                )
                .unwrap();
                assert_eq!(
                    frames.len(),
                    payload_len.div_ceil(frame_limit.max(1)).max(1)
                );
                assert_eq!(
                    decrypt_stream(&frames, stream_id, sender.address().into(), recipient).unwrap(),
                    payload
                );
            }
        }
    }

    #[test]
    fn empty_plaintext_sends_final_stream_frame() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(7);
        let stream_id = uuid::Uuid::new_v4();

        let frames = encrypt_stream_with_rng(
            &[],
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            8,
            &mut rng,
        )
        .unwrap();

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].stream_id, stream_id);
        assert_eq!(frames[0].sequence, 0);
        assert!(frames[0].is_final);
        assert_eq!(
            decrypt_stream(&frames, stream_id, sender.address().into(), recipient).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn streaming_decryptor_accepts_ordered_final_frame() {
        let sender = sender_key();
        let recipient = recipient_key();
        let stream_id = uuid::Uuid::new_v4();
        let mut rng = Hc128Rng::seed_from_u64(42);
        let mut encryptor =
            E2eStreamEncryptor::new(stream_id, sender.pubkey(), recipient.pubkey()).unwrap();

        let first = encryptor.encrypt_next(b"\0hello", &mut rng).unwrap();
        let second = encryptor.encrypt_final(b"\xFFworld", &mut rng).unwrap();
        let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient);

        let mut plaintext = decryptor.decrypt_next(&first).unwrap();
        plaintext.extend_from_slice(&decryptor.decrypt_next(&second).unwrap());
        decryptor.finish().unwrap();

        assert_eq!(plaintext, b"\0hello\xFFworld");
    }

    #[test]
    fn truncation_is_rejected_without_final_frame() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(9);
        let stream_id = uuid::Uuid::new_v4();
        let payload = vec![9u8; 96];
        let mut frames = encrypt_stream_with_rng(
            &payload,
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )
        .unwrap();

        frames.pop();

        assert!(matches!(
            decrypt_stream(&frames, stream_id, sender.address().into(), recipient),
            Err(Error::E2eMissingFinalFrame)
        ));
    }

    #[test]
    fn reordered_frames_are_buffered_until_contiguous() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(10);
        let stream_id = uuid::Uuid::new_v4();
        let payload = vec![10u8; 96];
        let mut frames = encrypt_stream_with_rng(
            &payload,
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )
        .unwrap();

        frames.swap(0, 1);

        assert_eq!(
            decrypt_stream(&frames, stream_id, sender.address().into(), recipient).unwrap(),
            payload
        );
    }

    #[test]
    fn replayed_consumed_frame_is_idempotent() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(16);
        let stream_id = uuid::Uuid::new_v4();
        let payload = vec![16u8; 96];
        let frames = encrypt_stream_with_rng(
            &payload,
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )
        .unwrap();
        let final_frame = frames.last().unwrap().clone();
        let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient);
        let mut plaintext = Vec::new();

        plaintext.extend_from_slice(&decryptor.decrypt_next(&frames[0]).unwrap());
        assert_eq!(
            decryptor.decrypt_next(&frames[0]).unwrap(),
            Vec::<u8>::new()
        );

        for frame in &frames[1..] {
            plaintext.extend_from_slice(&decryptor.decrypt_next(frame).unwrap());
        }
        decryptor.finish().unwrap();

        assert_eq!(
            decryptor.decrypt_next(&final_frame).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(plaintext, payload);
    }

    #[test]
    fn replayed_buffered_frame_is_idempotent() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(17);
        let stream_id = uuid::Uuid::new_v4();
        let payload = vec![17u8; 96];
        let frames = encrypt_stream_with_rng(
            &payload,
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )
        .unwrap();
        let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient);
        let mut plaintext = Vec::new();

        assert_eq!(
            decryptor.decrypt_next(&frames[1]).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            decryptor.decrypt_next(&frames[1]).unwrap(),
            Vec::<u8>::new()
        );

        for frame in &frames {
            plaintext.extend_from_slice(&decryptor.decrypt_next(frame).unwrap());
        }
        decryptor.finish().unwrap();

        assert_eq!(plaintext, payload);
    }

    #[test]
    fn future_frame_outside_reorder_window_is_rejected() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(18);
        let stream_id = uuid::Uuid::new_v4();
        let frames = encrypt_stream_with_rng(
            &[18u8; 5],
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            1,
            &mut rng,
        )
        .unwrap();
        let mut decryptor = E2eStreamDecryptor::with_reorder_window(
            stream_id,
            sender.address().into(),
            recipient,
            2,
        );

        assert!(matches!(
            decryptor.decrypt_next(&frames[3]),
            Err(Error::E2eFrameReorderWindowExceeded {
                next_sequence: 0,
                actual: 3,
                window: 2
            })
        ));
    }

    #[test]
    fn final_frame_can_arrive_before_gap_is_filled() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(14);
        let stream_id = uuid::Uuid::new_v4();
        let payload = vec![14u8; 96];
        let mut frames = encrypt_stream_with_rng(
            &payload,
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )
        .unwrap();
        frames.rotate_right(1);

        let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient);
        let mut plaintext = Vec::new();

        assert_eq!(
            decryptor.decrypt_next(&frames[0]).unwrap(),
            Vec::<u8>::new()
        );
        assert!(matches!(
            decryptor.finish(),
            Err(Error::E2eMissingFinalFrame)
        ));

        for frame in &frames[1..] {
            plaintext.extend_from_slice(&decryptor.decrypt_next(frame).unwrap());
        }
        decryptor.finish().unwrap();

        assert_eq!(plaintext, payload);
    }

    #[test]
    fn frame_after_buffered_final_is_rejected() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(15);
        let stream_id = uuid::Uuid::new_v4();
        let frames = encrypt_stream_with_rng(
            &[15u8; 96],
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )
        .unwrap();
        let final_frame = frames.last().unwrap();
        let mut frame_after_final = frames.first().unwrap().clone();
        frame_after_final.sequence = final_frame.sequence.checked_add(1).unwrap();

        let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient);
        assert_eq!(
            decryptor.decrypt_next(final_frame).unwrap(),
            Vec::<u8>::new()
        );
        assert!(matches!(
            decryptor.decrypt_next(&frame_after_final),
            Err(Error::E2eFrameAfterFinal)
        ));
    }

    #[test]
    fn wrong_stream_id_is_rejected() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(12);
        let stream_id = uuid::Uuid::new_v4();
        let other_stream_id = uuid::Uuid::new_v4();
        let frames = encrypt_stream_with_rng(
            b"hello",
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            &mut rng,
        )
        .unwrap();

        assert!(matches!(
            decrypt_stream(&frames, other_stream_id, sender.address().into(), recipient),
            Err(Error::E2eStreamIdMismatch { .. })
        ));
    }

    #[test]
    fn wrong_sender_key_is_rejected() {
        let sender = sender_key();
        let recipient = recipient_key();
        let mut rng = Hc128Rng::seed_from_u64(11);
        let stream_id = uuid::Uuid::new_v4();
        let payload = vec![11u8; 32];
        let mut frames = encrypt_stream_with_rng(
            &payload,
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            32,
            &mut rng,
        )
        .unwrap();
        frames[0].sender_public_key = recipient.pubkey();

        assert!(matches!(
            decrypt_stream(&frames, stream_id, sender.address().into(), recipient),
            Err(Error::E2ePublicKeyDidMismatch { .. })
        ));
    }
}
