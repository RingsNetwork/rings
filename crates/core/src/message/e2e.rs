//! End-to-end authenticated encryption for byte payloads.
//!
//! This module models E2E encryption as two separate protocol facts:
//!
//! - peers discover encryption public keys with signed handshake messages;
//! - message bodies are encrypted with per-frame secp256k1 ephemeral-key ECIES
//!   (ECDH + HKDF-SHA256 + ChaCha20-Poly1305) stream frames, each carried by its
//!   own signed [`MessagePayload`](crate::message::MessagePayload).
//!
//! Rings intentionally uses the DID/account secp256k1 public key as the
//! encryption key. The public key carried by a handshake or encrypted message
//! must resolve to the signed DID; this is the protocol's key-ownership proof.
//! The module does not derive a separate encryption subkey.
//!
//! Each [`E2eStreamFrame`] carries a fresh ephemeral secp256k1 public key and a
//! ChaCha20-Poly1305 ciphertext whose associated data binds the stream
//! identifier, sender public key, recipient public key, ephemeral public key,
//! monotonic sequence number, and end-of-stream flag. Ciphertext modification,
//! cross-stream replay, sequence swapping, or finality tampering are therefore
//! rejected by the AEAD tag even if a frame is inspected outside its outer
//! [`MessagePayload`](crate::message::MessagePayload) envelope. The stream id,
//! sequence, and final marker make truncation observable and let the decryptor
//! release reordered frames as a gapless plaintext stream.
//!
//! Delivery to the application is best effort, so a frame may be lost. Law (no silent
//! stall): the decryptor releases plaintext only as a gapless prefix of the stream, and a stream
//! that has made no progress for its stall deadline ([`REORDER_DEADLINE`] by default) fails with
//! [`Error::E2eStreamStalled`], whether the lost frame is the first, a middle one (later frames
//! wait buffered) or the final one (nothing more arrives). The decryptor is pure: the caller
//! supplies the time, and polls [`E2eStreamDecryptor::check`] while no frame arrives.

use std::collections::BTreeMap;
use std::time::Duration;

use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::KeyInit;
use chacha20poly1305::aead::Payload;
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::Key;
use chacha20poly1305::Nonce;
use hkdf::Hkdf;
use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::dht::Did;
use crate::ecc::elgamal::impls::secp256k1;
use crate::ecc::group::Point;
use crate::ecc::group::Secp256k1;
use crate::ecc::prime_order::NonIdentityPoint;
use crate::ecc::prime_order::NonZeroScalar;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::REORDER_DEADLINE;
use crate::message::REORDER_WINDOW;
use crate::utils::SystemEntropy;

/// Plaintext bytes per logical E2E block unit.
pub const E2E_PLAINTEXT_BLOCK_LEN: usize = secp256k1::PLAINTEXT_BLOCK_SIZE;

/// Default plaintext bytes per encrypted E2E stream frame.
pub const DEFAULT_E2E_PLAINTEXT_FRAME_LEN: usize = E2E_PLAINTEXT_BLOCK_LEN * 16;

const E2E_AEAD_KEY_LEN: usize = 32;
const E2E_AEAD_NONCE_LEN: usize = 12;
const E2E_AEAD_TAG_LEN: usize = 16;
const E2E_ECIES_SALT: &[u8] = b"rings-core:e2e-ecies-v1:salt";
const E2E_ECIES_INFO: &[u8] = b"rings-core:e2e-ecies-v1:chacha20poly1305";
const E2E_ECIES_AAD_TAG: &[u8] = b"rings-core:e2e-ecies-v1:aad";
const E2E_ECIES_AAD_LEN: usize = E2E_ECIES_AAD_TAG.len() + 16 + 33 + 33 + 33 + 8 + 1;

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

/// One ordered ECIES-authenticated stream frame.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct E2eStreamFrame {
    /// Stream identifier shared by all frames in this body stream.
    pub stream_id: E2eStreamId,
    /// Public key owned by the signed sender DID.
    pub sender_public_key: PublicKey<33>,
    /// Per-frame ephemeral secp256k1 public key for ECDH key agreement.
    pub ephemeral_public_key: PublicKey<33>,
    /// Monotonic frame counter checked by the decryptor and bound in AEAD AAD.
    pub sequence: u64,
    /// End-of-stream marker authenticated by AEAD AAD and the message signature.
    pub is_final: bool,
    /// ChaCha20-Poly1305 ciphertext and 16-byte authentication tag.
    pub ciphertext: Vec<u8>,
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
    /// How long the stream may go without progress before it is stalled.
    stall_deadline: Duration,
    /// When the stream last made progress: its creation, or the latest frame that released
    /// plaintext.
    progress_ms: u128,
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
    /// Verify that the carried sender key belongs to the signed sender DID and that the
    /// ephemeral public key is a valid non-identity secp256k1 point.
    pub fn verify_sender(&self, sender: Did) -> Result<()> {
        ensure_public_key_matches_did(self.sender_public_key, sender)?;
        ensure_valid_public_key(self.ephemeral_public_key)
    }

    /// Logical ciphertext block count (`>= 1` for every valid AEAD frame, including an empty
    /// final frame carrying only the 16-byte authentication tag).
    pub fn ciphertext_blocks(&self) -> usize {
        self.ciphertext
            .len()
            .saturating_sub(E2E_AEAD_TAG_LEN)
            .div_ceil(E2E_PLAINTEXT_BLOCK_LEN)
            .max(1)
    }
}

impl E2eStreamEncryptor {
    /// Create an ECIES streaming encryptor for `recipient_public_key`.
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

    /// Encrypt the final plaintext frame and close this stream.
    pub fn encrypt_final(
        &mut self,
        plaintext: &[u8],
        rng: &mut impl RngCore,
    ) -> Result<E2eStreamFrame> {
        self.encrypt_frame(plaintext, true, rng)
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

        let recipient_point = NonIdentityPoint::<Secp256k1>::try_from(self.recipient_public_key)?;
        let ephemeral_secret_key = SecretKey::random_with_rng(rng);
        let ephemeral_scalar = NonZeroScalar::<Secp256k1>::from_secret_key(&ephemeral_secret_key);
        let ephemeral_public_key =
            PublicKey::from(&NonIdentityPoint::generator_mul(&ephemeral_scalar));
        let shared_secret = recipient_point.shared_secret(&ephemeral_scalar);
        let derived = derive_e2e_key_and_nonce(shared_secret.as_slice(), true)?;
        let aad = e2e_ecies_aad(
            self.stream_id,
            self.sender_public_key,
            self.recipient_public_key,
            ephemeral_public_key,
            sequence,
            is_final,
        );
        let cipher = ChaCha20Poly1305::new(Key::from_slice(derived.key.as_slice()));
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(derived.nonce.as_slice()), Payload {
                msg: plaintext,
                aad: aad.as_slice(),
            })
            .map_err(|_| Error::MessageEncryptionFailed("E2E AEAD seal failed".to_string()))?;

        Ok(E2eStreamFrame {
            stream_id: self.stream_id,
            sender_public_key: self.sender_public_key,
            ephemeral_public_key,
            sequence,
            is_final,
            ciphertext,
        })
    }
}

impl E2eStreamDecryptor {
    /// Create an ECIES streaming decryptor for a recipient secret key, at `now_ms`.
    pub fn new(
        stream_id: E2eStreamId,
        expected_sender: Did,
        recipient_secret_key: SecretKey,
        now_ms: u128,
    ) -> Self {
        Self {
            stream_id,
            expected_sender,
            recipient_secret_key,
            next_sequence: 0,
            final_sequence: None,
            seen_final: false,
            reorder_window: REORDER_WINDOW,
            pending_frames: BTreeMap::new(),
            stall_deadline: REORDER_DEADLINE,
            progress_ms: now_ms,
        }
    }

    /// This decryptor with the future-frame reorder window `reorder_window` instead of
    /// [`REORDER_WINDOW`], beyond which a missing frame can no longer arrive.
    pub fn with_reorder_window(mut self, reorder_window: u64) -> Self {
        self.reorder_window = reorder_window;
        self
    }

    /// This decryptor with the stall deadline `deadline` instead of [`REORDER_DEADLINE`].
    pub fn with_stall_deadline(mut self, deadline: Duration) -> Self {
        self.stall_deadline = deadline;
        self
    }

    /// Decrypt one frame arriving at `now_ms` and return newly contiguous plaintext bytes.
    ///
    /// Out-of-order future frames are buffered and return an empty vector until the missing
    /// lower sequence numbers arrive. Fails with [`Error::E2eStreamStalled`] when, even after
    /// this frame, the stream has made no progress for its stall deadline.
    pub fn decrypt_next(&mut self, frame: &E2eStreamFrame, now_ms: u128) -> Result<Vec<u8>> {
        let progressed_from = (self.next_sequence, self.seen_final);
        let plaintext = self.accept(frame)?;
        if (self.next_sequence, self.seen_final) != progressed_from {
            self.progress_ms = now_ms;
        }
        self.check(now_ms)?;
        Ok(plaintext)
    }

    /// Whether the stream is still live at `now_ms`: complete, or with progress within its
    /// stall deadline. A caller polls this while no frame arrives, so a lost final frame is
    /// reported, not waited for.
    pub fn check(&self, now_ms: u128) -> Result<()> {
        let stalled_ms = now_ms.saturating_sub(self.progress_ms);
        if self.seen_final || stalled_ms <= self.stall_deadline.as_millis() {
            return Ok(());
        }
        Err(Error::E2eStreamStalled {
            next_sequence: self.next_sequence,
            buffered: self.pending_frames.len(),
            stalled_ms,
        })
    }

    /// Verify that the stream ended with an authenticated final frame.
    pub fn finish(&self) -> Result<()> {
        if self.seen_final {
            Ok(())
        } else {
            Err(Error::E2eMissingFinalFrame)
        }
    }

    /// Take one frame into the stream and return the plaintext it made contiguous.
    fn accept(&mut self, frame: &E2eStreamFrame) -> Result<Vec<u8>> {
        if self.validate_frame(frame)? == FrameAcceptance::Duplicate {
            return Ok(Vec::new());
        }

        if frame.is_final {
            self.final_sequence = Some(frame.sequence);
        }
        self.pending_frames.insert(frame.sequence, frame.clone());
        self.decrypt_ready_frames()
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
        let recipient_public_key = self.recipient_secret_key.pubkey();
        let recipient_scalar =
            NonZeroScalar::<Secp256k1>::from_secret_key(&self.recipient_secret_key);

        while let Some(frame) = self.pending_frames.get(&self.next_sequence) {
            let frame_plaintext =
                decrypt_ecies_frame(frame, recipient_public_key, &recipient_scalar)?;
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

fn decrypt_ecies_frame(
    frame: &E2eStreamFrame,
    recipient_public_key: PublicKey<33>,
    recipient_scalar: &NonZeroScalar<Secp256k1>,
) -> Result<Vec<u8>> {
    let ephemeral_point = NonIdentityPoint::<Secp256k1>::try_from(frame.ephemeral_public_key)?;
    let shared_secret = ephemeral_point.shared_secret(recipient_scalar);
    let derived = derive_e2e_key_and_nonce(shared_secret.as_slice(), false)?;
    let aad = e2e_ecies_aad(
        frame.stream_id,
        frame.sender_public_key,
        recipient_public_key,
        frame.ephemeral_public_key,
        frame.sequence,
        frame.is_final,
    );
    let cipher = ChaCha20Poly1305::new(Key::from_slice(derived.key.as_slice()));
    cipher
        .decrypt(Nonce::from_slice(derived.nonce.as_slice()), Payload {
            msg: frame.ciphertext.as_slice(),
            aad: aad.as_slice(),
        })
        .map_err(|_| Error::MessageDecryptionFailed("E2E AEAD open failed".to_string()))
}

struct E2eDerivedMaterial {
    key: Zeroizing<[u8; E2E_AEAD_KEY_LEN]>,
    nonce: Zeroizing<[u8; E2E_AEAD_NONCE_LEN]>,
}

fn derive_e2e_key_and_nonce(
    shared_secret: &[u8],
    encrypt_side: bool,
) -> Result<E2eDerivedMaterial> {
    let mut okm = Zeroizing::new(vec![u8::default(); E2E_AEAD_KEY_LEN + E2E_AEAD_NONCE_LEN]);
    Hkdf::<Sha256>::new(Some(E2E_ECIES_SALT), shared_secret)
        .expand(E2E_ECIES_INFO, okm.as_mut_slice())
        .map_err(|_| {
            if encrypt_side {
                Error::MessageEncryptionFailed("E2E key derivation failed".to_string())
            } else {
                Error::MessageDecryptionFailed("E2E key derivation failed".to_string())
            }
        })?;
    let (key_bytes, nonce_bytes) = okm.split_at(E2E_AEAD_KEY_LEN);
    let mut key = Zeroizing::<[u8; E2E_AEAD_KEY_LEN]>::default();
    key.copy_from_slice(key_bytes);
    let mut nonce = Zeroizing::<[u8; E2E_AEAD_NONCE_LEN]>::default();
    nonce.copy_from_slice(nonce_bytes);
    Ok(E2eDerivedMaterial { key, nonce })
}

fn e2e_ecies_aad(
    stream_id: E2eStreamId,
    sender_public_key: PublicKey<33>,
    recipient_public_key: PublicKey<33>,
    ephemeral_public_key: PublicKey<33>,
    sequence: u64,
    is_final: bool,
) -> [u8; E2E_ECIES_AAD_LEN] {
    let mut aad = [0u8; E2E_ECIES_AAD_LEN];
    let mut offset = 0usize;
    for part in [
        E2E_ECIES_AAD_TAG,
        stream_id.as_bytes().as_slice(),
        sender_public_key.0.as_slice(),
        recipient_public_key.0.as_slice(),
        ephemeral_public_key.0.as_slice(),
        sequence.to_be_bytes().as_slice(),
        [u8::from(is_final)].as_slice(),
    ] {
        let next = offset + part.len();
        if let Some(slot) = aad.get_mut(offset..next) {
            slot.copy_from_slice(part);
        }
        offset = next;
    }
    debug_assert_eq!(offset, E2E_ECIES_AAD_LEN);
    aad
}

fn plaintext_stream_frames(
    plaintext: &[u8],
    max_plaintext_frame_len: usize,
) -> E2ePlaintextFrames<'_> {
    E2ePlaintextFrames::new(plaintext, max_plaintext_frame_len)
}

/// Encrypt a byte slice lazily into ECIES-authenticated E2E stream frames.
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

/// Encrypt a byte slice lazily with the default system entropy source.
pub fn encrypt_stream_frames(
    plaintext: &[u8],
    stream_id: E2eStreamId,
    sender_public_key: PublicKey<33>,
    recipient_public_key: PublicKey<33>,
    max_plaintext_frame_len: usize,
) -> Result<E2eStreamFrames<'_, SystemEntropy>> {
    encrypt_stream_frames_with_rng(
        plaintext,
        stream_id,
        sender_public_key,
        recipient_public_key,
        max_plaintext_frame_len,
        SystemEntropy,
    )
}

/// Encrypt a byte slice into ECIES-authenticated E2E stream frames.
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

/// Decrypt a complete ECIES-authenticated E2E frame sequence into bytes.
pub fn decrypt_stream(
    frames: &[E2eStreamFrame],
    stream_id: E2eStreamId,
    expected_sender: Did,
    recipient_secret_key: SecretKey,
) -> Result<Vec<u8>> {
    // A complete sequence is taken at once, so it is never stalled: no time is consulted.
    let mut decryptor =
        E2eStreamDecryptor::new(stream_id, expected_sender, recipient_secret_key, 0);
    let mut plaintext = Vec::new();

    for frame in frames {
        let frame_plaintext = decryptor.accept(frame)?;
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
mod tests;
