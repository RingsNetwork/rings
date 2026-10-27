//! A mail as DHT elements: a signed manifest and the chunks of one ciphertext (pure).
//!
//! ```text
//!   c          = AeadCiphertext(m) to ek, aad = "rings-node:mail:" ‖ d      (encoded)
//!   chunk_i    = (mail_id, i, n, slice_i(c))           each element ≤ ENTRY_PAYLOAD_MAX_BYTES
//!   manifest   = (d, mail_id = H(c), [H(chunk_0) … H(chunk_{n−1})], sig_gateway)
//!   seal(m)    = [manifest, chunk_0, …, chunk_{n−1}]   n ≤ N_max, else TooLarge
//! ```
//!
//! `H` is SHA-256 over bytes as they are carried: `H(c)` over the encoded ciphertext and
//! `H(chunk_i)` over the chunk's element, so a reader checks a chunk before decoding it. The
//! AEAD tag covers the whole of `c`, and its associated data binds the recipient, so a mail
//! opens only whole and only for the DID it was sealed for.
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
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use rings_core::message::SigningDomain;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;

use super::descriptor::MailRecipient;

/// The most chunks one mail is cut into, `N_max`.
pub const MAIL_CHUNKS_MAX: usize = 32;
/// Bytes a chunk element adds to its slice: the element's variant tag (1), the mail id (32),
/// the index and the count (1 each), and the varint length of a slice of `2¹⁴` to `2²¹` bytes
/// (3), `1 + 32 + 1 + 1 + 3 = 38`; witnessed with equality by
/// `test_the_widest_chunk_element_is_one_entry_payload`.
pub const MAIL_CHUNK_ELEMENT_OVERHEAD: usize = 38;
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
/// Message family of the manifest signature.
const MAIL_MANIFEST_DOMAIN_TAG: DomainTag = domain_tag!("rings-node:mail-manifest");
/// The prefix of the AEAD associated data, which binds the recipient's DID.
const MAIL_AAD_PREFIX: &str = "rings-node:mail:";

/// The ciphertext `MAIL_CHUNKS_MAX` indexes fit in a chunk's one-byte index and count.
const _: () = assert!(MAIL_CHUNKS_MAX <= u8::MAX as usize);

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
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// One slice of a mail's ciphertext.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct MailChunk {
    /// `H(c)` of the ciphertext this chunk is a slice of.
    pub mail_id: MailDigest,
    /// The slice's position, `i`.
    pub index: u8,
    /// The ciphertext's number of chunks, `n`.
    pub count: u8,
    /// `slice_i(c)`.
    pub slice: Bytes,
}

/// The fields of a manifest its gateway signs.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct ManifestBody {
    /// The DID the mail is sealed for.
    pub recipient: Did,
    /// `H(c)`.
    pub mail_id: MailDigest,
    /// `H(chunk_i)` of every chunk, in order; `n` is its length.
    pub chunks: Vec<MailDigest>,
}

impl ManifestBody {
    /// The bytes the signature covers.
    fn signing_data(&self) -> Result<Vec<u8>, rings_codec::Error> {
        rings_codec::serialize(self)
    }
}

/// A manifest: which chunks make one mail, signed by the gateway that wrote it.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct MailManifest {
    /// The signed fields.
    pub body: ManifestBody,
    /// The gateway's signature over `body`.
    pub signature: MessageVerification,
}

impl MailManifest {
    /// Sign `body` as the gateway `signer` at `now_ms`.
    pub fn new_signed(
        body: ManifestBody,
        signer: MessageSigner<&DelegateeKey>,
        now_ms: u128,
    ) -> Result<Self, SealError> {
        let signature = signer.sign_at(MAIL_MANIFEST_DOMAIN_TAG, &body.signing_data()?, now_ms)?;
        Ok(Self { body, signature })
    }

    /// Whether a reader in overlay `network_id` at `now_ms` takes this manifest for
    /// `recipient`: it names `recipient`, at least one and at most [`MAIL_CHUNKS_MAX`] chunks,
    /// and its gateway's signature verifies.
    pub fn verifies_for(&self, recipient: Did, network_id: u32, now_ms: u128) -> bool {
        let names = (1..=MAIL_CHUNKS_MAX).contains(&self.body.chunks.len());
        self.body.recipient == recipient
            && names
            && self.body.signing_data().is_ok_and(|data| {
                let domain = SigningDomain::new(MAIL_MANIFEST_DOMAIN_TAG, network_id);
                self.signature.verify_at(domain, &data, now_ms)
            })
    }

    /// The gateway that signed this manifest: its account DID.
    pub fn gateway(&self) -> Did {
        self.signature.delegation.delegator_did()
    }
}

/// One element of a recipient's mail topic.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum MailElement {
    /// A mail's manifest.
    Manifest(MailManifest),
    /// A slice of a mail's ciphertext.
    Chunk(MailChunk),
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
    /// Encryption or the manifest signature failed.
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

/// Seal `message` for `recipient`, signed by the gateway `signer` at `now_ms`, with ephemerals
/// from `rng`.
///
/// Post: `Ok(s)` has `1 + n` elements with `1 ≤ n ≤ MAIL_CHUNKS_MAX`, each at most
/// `ENTRY_PAYLOAD_MAX_BYTES`, and opens to `message` for the holder of
/// `recipient.encryption_key`'s secret (the round-trip law of `open`). `TooLarge` exactly when
/// the ciphertext needs more than `MAIL_CHUNKS_MAX` chunks, which every `message` of at most
/// [`MAIL_MESSAGE_MAX`] bytes does not.
pub fn seal(
    message: &[u8],
    recipient: &MailRecipient,
    signer: MessageSigner<&DelegateeKey>,
    now_ms: u128,
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
    let count = u8::try_from(chunks)
        .ok()
        .filter(|_| chunks <= MAIL_CHUNKS_MAX)
        .ok_or(SealError::TooLarge { chunks })?;
    let mail_id = MailDigest::of(&ciphertext);
    let chunk_elements = slices
        .zip(0u8..)
        .map(|(slice, index)| {
            MailElement::Chunk(MailChunk {
                mail_id,
                index,
                count,
                slice: Bytes::copy_from_slice(slice),
            })
            .encode()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let body = ManifestBody {
        recipient: recipient.did,
        mail_id,
        chunks: chunk_elements
            .iter()
            .map(|element| MailDigest::of(element))
            .collect(),
    };
    let manifest =
        MailElement::Manifest(MailManifest::new_signed(body, signer, now_ms)?).encode()?;
    Ok(SealedMail {
        mail_id,
        elements: std::iter::once(manifest).chain(chunk_elements).collect(),
    })
}
