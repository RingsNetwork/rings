//! A mail as DHT elements: a manifest and the chunks of one ciphertext (pure).
//!
//! ```text
//!   c          = AeadCiphertext(m) to ek, aad = "rings-node:mail:" ‖ d      (encoded)
//!   chunk_i    = slice_i(c)                             each element ≤ ENTRY_PAYLOAD_MAX_BYTES
//!   manifest   = (mail_id = H(c), [H(chunk_0) … H(chunk_{n−1})])
//!   seal(m)    = [manifest, chunk_0, …, chunk_{n−1}]   n ≤ N_max, else TooLarge
//! ```
//!
//! `H` is SHA-256 over bytes as they are carried: `H(c)` over the encoded ciphertext and
//! `H(chunk_i)` over the chunk's element, so a reader checks a chunk before decoding it. The
//! manifest's ordered digests fix which chunks make the mail and in which order, and `mail_id`
//! names their concatenation, so neither needs a signature: any writer can append a manifest,
//! but one whose chunks do not concatenate to `H⁻¹(mail_id)` is ignored (`open`). The AEAD tag
//! covers the whole of `c`, and its associated data binds the recipient, so a mail opens only
//! whole and only for the DID it was sealed for.
//!
//! Sizes (`N_max = 32`, about 1 MiB a mail; a topic keeps `ENTRY_DATA_MAX_LEN = 1024`
//! elements, so 32 mails of the largest size fit one lifetime):
//!
//! ```text
//!   |chunk element| = MAIL_CHUNK_ELEMENT_OVERHEAD + |slice| ≤ ENTRY_PAYLOAD_MAX_BYTES
//!   |c|             = MAIL_SEAL_OVERHEAD + |m|           for MAIL_MESSAGE_MAX's neighbourhood
//!   |m| ≤ MAIL_MESSAGE_MAX  ⇒  |c| ≤ MAIL_CIPHERTEXT_MAX = N_max · MAIL_CHUNK_SLICE_MAX
//! ```

use bytes::Bytes;
use rand::RngCore;
use rings_core::consts::ENTRY_PAYLOAD_MAX_BYTES;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;

use super::descriptor::MailRecipient;
use super::write_lowercase_hex;

/// The most chunks one mail is cut into, `N_max`.
pub const MAIL_CHUNKS_MAX: usize = 32;
/// Bytes a chunk element adds to its slice: the element's variant tag (1) and the varint
/// length of a slice of `2¹⁴` to `2²¹` bytes (3), `1 + 3 = 4`; witnessed with equality by
/// `test_the_widest_chunk_element_is_one_entry_payload`.
pub const MAIL_CHUNK_ELEMENT_OVERHEAD: usize = 4;
/// The largest slice of a ciphertext one chunk carries: its element is then exactly
/// `ENTRY_PAYLOAD_MAX_BYTES`.
pub const MAIL_CHUNK_SLICE_MAX: usize = ENTRY_PAYLOAD_MAX_BYTES - MAIL_CHUNK_ELEMENT_OVERHEAD;
/// The largest ciphertext one mail carries: [`MAIL_CHUNKS_MAX`] full slices.
pub const MAIL_CIPHERTEXT_MAX: usize = MAIL_CHUNKS_MAX * MAIL_CHUNK_SLICE_MAX;
/// Bytes an encoded ciphertext adds to its message, for a message of `2¹⁴ − 16` to `2²¹ − 17`
/// bytes: its version (1), the length of its wrapped key (1) and the key's two blocks of two
/// points, each a length-prefixed 51-character base58 string (`2 · 2 · 52`), the nonce (12), the
/// varint length of the sealed message (3) and the AEAD tag (16),
/// `1 + 1 + 208 + 12 + 3 + 16 = 241`; witnessed with equality by
/// `test_the_largest_message_seals_into_the_most_chunks`.
pub const MAIL_SEAL_OVERHEAD: usize = 241;
/// The largest message one mail carries: its ciphertext is then exactly
/// [`MAIL_CIPHERTEXT_MAX`].
pub const MAIL_MESSAGE_MAX: usize = MAIL_CIPHERTEXT_MAX - MAIL_SEAL_OVERHEAD;
/// The prefix of the AEAD associated data, which binds the recipient's DID.
const MAIL_AAD_PREFIX: &str = "rings-node:mail:";

/// A SHA-256 digest, `H(x)`: the id of a ciphertext (`mail_id = H(c)`) or the name of a chunk
/// element (`H(chunk_i)`).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MailDigest([u8; 32]);

impl MailDigest {
    /// `H(bytes)`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl std::fmt::Display for MailDigest {
    /// The digest in lowercase hex.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_lowercase_hex(f, &self.0)
    }
}

/// A manifest: which chunks make one mail, in order.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct MailManifest {
    /// `H(c)`.
    pub mail_id: MailDigest,
    /// `H(chunk_i)` of every chunk, in order; `n` is its length.
    pub chunks: Vec<MailDigest>,
}

impl MailManifest {
    /// Whether a reader takes this manifest: it lists at least one and at most
    /// [`MAIL_CHUNKS_MAX`] chunks.
    pub fn is_well_formed(&self) -> bool {
        (1..=MAIL_CHUNKS_MAX).contains(&self.chunks.len())
    }
}

/// One element of a recipient's mail topic.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum MailElement {
    /// A mail's manifest.
    Manifest(MailManifest),
    /// A slice of a mail's ciphertext, `slice_i(c)`.
    Chunk(Bytes),
}

impl MailElement {
    /// This element as DHT element bytes.
    pub fn encode(&self) -> Result<Bytes, rings_codec::Error> {
        rings_codec::serialize(self).map(Bytes::from)
    }

    /// The element `bytes` encode.
    pub fn decode(bytes: &[u8]) -> Result<Self, rings_codec::Error> {
        rings_codec::deserialize(bytes)
    }
}

/// The AEAD associated data of mail for `recipient`: `"rings-node:mail:" ‖ d`.
pub fn mail_aad(recipient: Did) -> Vec<u8> {
    format!("{MAIL_AAD_PREFIX}{recipient}").into_bytes()
}

/// Why a message could not be sealed.
#[derive(Debug, Error)]
pub enum SealError {
    /// The ciphertext needs more than [`MAIL_CHUNKS_MAX`] chunks.
    #[error("the mail needs {chunks} chunks, more than {MAIL_CHUNKS_MAX}")]
    TooLarge {
        /// The chunks it needs.
        chunks: usize,
    },
    /// Encryption failed.
    #[error("cannot seal the mail: {0}")]
    Crypto(#[from] rings_core::error::Error),
    /// An element could not be encoded.
    #[error("cannot encode the mail: {0}")]
    Encode(#[from] rings_codec::Error),
}

/// A sealed mail: its id and the elements to write, manifest first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedMail {
    /// `H(c)`.
    pub mail_id: MailDigest,
    /// `[manifest, chunk_0, …, chunk_{n−1}]`.
    pub elements: Vec<Bytes>,
}

/// Seal `message` for `recipient`, with ephemerals from `rng`.
///
/// Post: `Ok(s)` has `1 + n` elements with `1 ≤ n ≤ MAIL_CHUNKS_MAX`, each at most
/// `ENTRY_PAYLOAD_MAX_BYTES`, and opens to `message` for the holder of
/// `recipient.encryption_key`'s secret (the round-trip law of `open`). `TooLarge` exactly when
/// the ciphertext needs more than `MAIL_CHUNKS_MAX` chunks, which every `message` of at most
/// [`MAIL_MESSAGE_MAX`] bytes does not.
pub fn seal(
    message: &[u8],
    recipient: &MailRecipient,
    rng: &mut impl RngCore,
) -> Result<SealedMail, SealError> {
    let sealed = encrypt_aead_with_rng(
        message,
        &mail_aad(recipient.did),
        recipient.encryption_key,
        rng,
    )?;
    let ciphertext = rings_codec::serialize(&sealed)?;
    let slices = ciphertext.chunks(MAIL_CHUNK_SLICE_MAX);
    let chunks = slices.len();
    if chunks > MAIL_CHUNKS_MAX {
        return Err(SealError::TooLarge { chunks });
    }
    let chunk_elements = slices
        .map(|slice| MailElement::Chunk(Bytes::copy_from_slice(slice)).encode())
        .collect::<Result<Vec<_>, _>>()?;
    let mail_id = MailDigest::of(&ciphertext);
    let manifest = MailElement::Manifest(MailManifest {
        mail_id,
        chunks: chunk_elements
            .iter()
            .map(|element| MailDigest::of(element))
            .collect(),
    })
    .encode()?;
    Ok(SealedMail {
        mail_id,
        elements: std::iter::once(manifest).chain(chunk_elements).collect(),
    })
}
