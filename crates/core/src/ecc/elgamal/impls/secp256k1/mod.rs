//! secp256k1 plaintext and ciphertext adapter for ElGamal.
//!
//! The generic ElGamal implementation encrypts group elements. Rings callers
//! encrypt bytes and exchange `PublicKey<33>` values, so this module provides
//! the adapter:
//!
//! - map bytes into secp256k1 points with a reversible x-coordinate encoding;
//! - call [`crate::ecc::elgamal::ElGamal`] over `Point<Secp256k1>`;
//! - serialize ciphertext points back into `CurveEle<33>` pairs.
//!
//! The point encoding is intentionally local to this adapter. Other curves can
//! choose different message encodings without changing the ElGamal algorithm or
//! the finite-group abstraction.
//!
//! Plaintext chunks are encoded into 32-byte secp256k1 field candidates as:
//!
//! - byte `0`: initial lift-search bias, starting at `0xFF`;
//! - byte `1`: adapter marker `0x52`;
//! - byte `2`: plaintext length for this chunk;
//! - bytes `3..3+len`: the raw plaintext bytes;
//! - remaining bytes: zero padding.
//!
//! `lift_x` may overwrite byte `0` while searching for an x-coordinate that
//! lies on secp256k1. Decoding therefore ignores byte `0` and validates the
//! marker, length, and zero padding before returning bytes `3..3+len`. This is
//! why embedded NUL bytes are preserved instead of being trimmed.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::KeyInit;
use chacha20poly1305::aead::Payload;
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::Key;
use chacha20poly1305::Nonce;
use elliptic_curve::point::AffineCoordinates;
use elliptic_curve::point::DecompressPoint;
use hkdf::Hkdf;
use k256::AffinePoint as Affine;
use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;
use sha2::Sha256;
use subtle::Choice;
use zeroize::Zeroizing;

use crate::ecc::elgamal::ElGamal;
use crate::ecc::elgamal::ElGamalPublicKey;
use crate::ecc::elgamal::ElGamalSecretKey;
use crate::ecc::group::Point;
use crate::ecc::group::Scalar as GroupScalar;
use crate::ecc::group::Secp256k1;
use crate::ecc::CurveEle;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

const FIELD_ENCODING_MARKER: u8 = 0x52;
const FIELD_ENCODING_OVERHEAD: usize = 3;
const FIELD_CHUNK_SIZE: usize = 32 - FIELD_ENCODING_OVERHEAD;
const FIELD_CHUNK_SIZE_U8: u8 = 29;

/// Plaintext bytes carried by one secp256k1 point in this adapter.
pub const PLAINTEXT_BLOCK_SIZE: usize = FIELD_CHUNK_SIZE;

/// One serialized ElGamal ciphertext block over secp256k1.
pub type CiphertextBlock = (CurveEle<33>, CurveEle<33>);

/// Secp256k1 field candidate used by this adapter's reversible plaintext encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Field([u8; 32]);

impl Field {
    fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the big-endian 32-byte field representation.
    pub fn b32(&self) -> [u8; 32] {
        self.0
    }

    fn is_odd(&self) -> bool {
        self.0.last().copied().unwrap_or_default() & 1 == 1
    }
}

const AEAD_VERSION: u8 = 1;
const AEAD_KEY_LEN: usize = 32;
const AEAD_WRAPPED_KEY_BLOCKS: usize = AEAD_KEY_LEN.div_ceil(PLAINTEXT_BLOCK_SIZE);
const AEAD_NONCE_LEN: usize = 12;
const AEAD_HKDF_SALT: &[u8] = b"rings-core:secp256k1-elgamal-aead:salt";
const AEAD_HKDF_INFO: &[u8] = b"rings-core:secp256k1-elgamal-aead:chacha20poly1305";

/// KEM/DEM ciphertext using secp256k1 ElGamal to wrap a ChaCha20-Poly1305 key.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AeadCiphertext {
    /// Version of the AEAD envelope format.
    pub version: u8,
    /// ElGamal-encrypted DEM key material.
    pub encrypted_key: Vec<CiphertextBlock>,
    /// ChaCha20-Poly1305 nonce.
    pub nonce: [u8; AEAD_NONCE_LEN],
    /// ChaCha20-Poly1305 ciphertext, including the authentication tag.
    pub ciphertext: Vec<u8>,
}

impl AeadCiphertext {
    /// Prove that the envelope carries exactly the blocks needed for one fixed-size AEAD key.
    ///
    /// Deserialization calls this before producing an envelope, and [`decrypt_aead`] repeats the
    /// check as defense in depth for values constructed in memory.
    pub fn validate_wrapped_key(&self) -> Result<()> {
        let actual = self.encrypted_key.len();
        if actual != AEAD_WRAPPED_KEY_BLOCKS {
            return Err(Error::AeadWrappedKeyBlockCount {
                expected: AEAD_WRAPPED_KEY_BLOCKS,
                actual,
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for AeadCiphertext {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        #[derive(Deserialize)]
        struct WireEnvelope {
            version: u8,
            encrypted_key: Vec<CiphertextBlock>,
            nonce: [u8; AEAD_NONCE_LEN],
            ciphertext: Vec<u8>,
        }

        let envelope = WireEnvelope::deserialize(deserializer)?;
        let sealed = Self {
            version: envelope.version,
            encrypted_key: envelope.encrypted_key,
            nonce: envelope.nonce,
            ciphertext: envelope.ciphertext,
        };
        sealed
            .validate_wrapped_key()
            .map_err(serde::de::Error::custom)?;
        Ok(sealed)
    }
}

#[derive(Serialize)]
struct AeadTranscript<'a> {
    version: u8,
    encrypted_key: &'a [CiphertextBlock],
    aad: &'a [u8],
}

/// Convert bytes into field elements using the adapter encoding.
///
/// Each plaintext chunk is at most 29 bytes so the field candidate can carry
/// `0xFF || 0x52 || len || chunk || zero padding`. The first byte is only a
/// search bias for `lift_x`; the marker and length bytes make the mapping
/// reversible and preserve leading or embedded NUL bytes.
pub fn bytes_to_field(bytes: &[u8]) -> Vec<Field> {
    bytes
        .chunks(PLAINTEXT_BLOCK_SIZE)
        .map(|x| Field::new(encode_field_candidate(x)))
        .collect()
}

fn encode_field_candidate(chunk: &[u8]) -> [u8; 32] {
    let mut data = [0u8; 32];
    let payload_len = chunk.len().min(FIELD_CHUNK_SIZE);
    let chunk_len = u8::try_from(payload_len).unwrap_or(FIELD_CHUNK_SIZE_U8);
    if let Some(header) = data.get_mut(..FIELD_ENCODING_OVERHEAD) {
        header.copy_from_slice(&[255, FIELD_ENCODING_MARKER, chunk_len]);
    }

    let payload_end = FIELD_ENCODING_OVERHEAD + payload_len;
    if let (Some(payload), Some(source)) = (
        data.get_mut(FIELD_ENCODING_OVERHEAD..payload_end),
        chunk.get(..payload_len),
    ) {
        payload.copy_from_slice(source);
    }
    data
}

/// Decode field elements produced by [`bytes_to_field`].
pub fn field_to_bytes(f: &[Field]) -> Vec<u8> {
    f.iter().fold(vec![], |mut acc, x| {
        acc.extend(decode_field_bytes(x.b32()));
        acc
    })
}

fn decode_field_bytes(mut bytes: [u8; 32]) -> Vec<u8> {
    let [_, marker, len_byte, payload @ ..] = bytes;
    let len = len_byte as usize;
    if marker == FIELD_ENCODING_MARKER
        && len <= FIELD_CHUNK_SIZE
        && payload.iter().skip(len).all(|byte| *byte == 0)
    {
        return payload.iter().take(len).copied().collect();
    }

    if let Some(first) = bytes.first_mut() {
        *first = 0u8;
    }
    bytes.into_iter().skip_while(|n| *n == 0u8).collect()
}

/// Lift a field candidate into a secp256k1 affine point.
///
/// The initial candidate uses its own parity. If it is not on the curve, the
/// search decrements byte `0` from `254` to `1` and retries. This keeps bytes
/// `1..` intact, so decoding can recover the original marker, chunk length, and
/// plaintext bytes. If no high-byte candidate lifts, the adapter returns a
/// typed error.
fn lift_x(x: &Field) -> Result<Affine> {
    let x_bytes = x.b32();
    if let Some(point) = decompress_x(x_bytes, x.is_odd()) {
        return Ok(point);
    }

    for bias in (1..=254).rev() {
        let mut bytes = x_bytes;
        let Some(first) = bytes.first_mut() else {
            return Err(Error::Secp256k1PointLiftFailed);
        };
        *first = bias;

        let candidate = Field::new(bytes);
        if let Some(point) = decompress_x(candidate.b32(), candidate.is_odd()) {
            return Ok(point);
        }
    }

    // Typed safeguard for future encoding changes; normal adapter chunks should
    // find a valid lift among the high-byte candidates above.
    Err(Error::Secp256k1PointLiftFailed)
}

fn decompress_x(x: [u8; 32], y_is_odd: bool) -> Option<Affine> {
    Affine::decompress(&k256::FieldBytes::from(x), Choice::from(y_is_odd as u8)).into()
}

/// Convert bytes into secp256k1 points using the adapter encoding.
pub fn bytes_to_affine(bytes: &[u8]) -> Result<Vec<Affine>> {
    bytes_to_field(bytes)
        .into_iter()
        .map(|a| lift_x(&a))
        .collect::<Result<Vec<Affine>>>()
}

/// Decode secp256k1 points produced by [`bytes_to_affine`].
pub fn affine_to_bytes(a: &[Affine]) -> Vec<u8> {
    field_to_bytes(
        a.iter()
            .map(|point| {
                let mut x = [0u8; 32];
                x.copy_from_slice(point.x().as_slice());
                Field::new(x)
            })
            .collect::<Vec<Field>>()
            .as_slice(),
    )
}

/// Encrypt bytes with caller-supplied randomness for ElGamal ephemerals: one
/// ciphertext block per [`PLAINTEXT_BLOCK_SIZE`] bytes of input.
pub fn encrypt_bytes_with_rng(
    bytes: &[u8],
    k: PublicKey<33>,
    rng: &mut impl RngCore,
) -> Result<Vec<CiphertextBlock>> {
    let public_key = ElGamalPublicKey::<Point<Secp256k1>>::from_element(k.try_into()?);
    let points = bytes_to_affine(bytes)?
        .into_iter()
        .map(Point::<Secp256k1>::from);
    ElGamal::<Point<Secp256k1>>::encrypt_with_rng(points, &public_key, rng)
        .into_iter()
        .map(|(c1, c2)| Ok((c1.try_into()?, c2.try_into()?)))
        .collect()
}

/// Decrypt bytes produced by [`encrypt_bytes_with_rng`].
pub fn decrypt_bytes(m: &[CiphertextBlock], k: &SecretKey) -> Result<Vec<u8>> {
    let secret_key =
        ElGamalSecretKey::<Point<Secp256k1>>::from_scalar(GroupScalar::<Secp256k1>::try_from(k)?);
    let ciphertext = m
        .iter()
        .map(|(c1, c2)| Ok(((*c1).try_into()?, (*c2).try_into()?)))
        .collect::<Result<Vec<(Point<Secp256k1>, Point<Secp256k1>)>>>()?;
    let points = ElGamal::<Point<Secp256k1>>::decrypt(&ciphertext, &secret_key)
        .into_iter()
        .map(Affine::from)
        .collect::<Vec<Affine>>();
    Ok(affine_to_bytes(&points))
}

/// Encrypt bytes with an ElGamal-wrapped ChaCha20-Poly1305 content key.
///
/// The ElGamal carrier is secp256k1. The DEM operation is ChaCha20-Poly1305.
/// The external `aad` and ElGamal key-wrapping ciphertext are both bound into
/// the AEAD transcript.
pub fn encrypt_aead_with_rng(
    plaintext: &[u8],
    aad: &[u8],
    recipient: PublicKey<33>,
    rng: &mut impl RngCore,
) -> Result<AeadCiphertext> {
    let mut key_material = Zeroizing::new([0u8; AEAD_KEY_LEN]);
    rng.fill_bytes(key_material.as_mut_slice());
    let encrypted_key = encrypt_bytes_with_rng(key_material.as_slice(), recipient, rng)?;
    let key = derive_aead_key(key_material.as_slice(), AeadErrorSide::Encrypt)?;
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let associated_data = aead_associated_data(&encrypted_key, aad)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_slice()));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload {
            msg: plaintext,
            aad: associated_data.as_slice(),
        })
        .map_err(|_| Error::MessageEncryptionFailed("AEAD seal failed".to_string()))?;

    Ok(AeadCiphertext {
        version: AEAD_VERSION,
        encrypted_key,
        nonce,
        ciphertext,
    })
}

/// Decrypt bytes produced by [`encrypt_aead_with_rng`].
pub fn decrypt_aead(
    sealed: &AeadCiphertext,
    aad: &[u8],
    recipient_secret: &SecretKey,
) -> Result<Vec<u8>> {
    if sealed.version != AEAD_VERSION {
        return Err(Error::MessageDecryptionFailed(format!(
            "unsupported ElGamal AEAD version {}",
            sealed.version
        )));
    }

    sealed.validate_wrapped_key()?;
    let key_material = Zeroizing::new(decrypt_bytes(&sealed.encrypted_key, recipient_secret)?);
    let key = derive_aead_key(key_material.as_slice(), AeadErrorSide::Decrypt)?;
    let associated_data = aead_associated_data(&sealed.encrypted_key, aad)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_slice()));
    cipher
        .decrypt(Nonce::from_slice(&sealed.nonce), Payload {
            msg: sealed.ciphertext.as_slice(),
            aad: associated_data.as_slice(),
        })
        .map_err(|_| Error::MessageDecryptionFailed("AEAD open failed".to_string()))
}

#[derive(Clone, Copy)]
enum AeadErrorSide {
    Encrypt,
    Decrypt,
}

fn derive_aead_key(
    key_material: &[u8],
    side: AeadErrorSide,
) -> Result<Zeroizing<[u8; AEAD_KEY_LEN]>> {
    if key_material.len() != AEAD_KEY_LEN {
        return Err(match side {
            AeadErrorSide::Encrypt => Error::MessageEncryptionFailed(format!(
                "invalid AEAD key material length {}",
                key_material.len()
            )),
            AeadErrorSide::Decrypt => Error::MessageDecryptionFailed(format!(
                "invalid AEAD key material length {}",
                key_material.len()
            )),
        });
    }

    let mut key = Zeroizing::new([0u8; AEAD_KEY_LEN]);
    Hkdf::<Sha256>::new(Some(AEAD_HKDF_SALT), key_material)
        .expand(AEAD_HKDF_INFO, key.as_mut_slice())
        .map_err(|_| match side {
            AeadErrorSide::Encrypt => {
                Error::MessageEncryptionFailed("AEAD key derivation failed".to_string())
            }
            AeadErrorSide::Decrypt => {
                Error::MessageDecryptionFailed("AEAD key derivation failed".to_string())
            }
        })?;
    Ok(key)
}

fn aead_associated_data(encrypted_key: &[CiphertextBlock], aad: &[u8]) -> Result<Vec<u8>> {
    rings_codec::serialize(&AeadTranscript {
        version: AEAD_VERSION,
        encrypted_key,
        aad,
    })
    .map_err(Error::CodecSerialize)
}

#[cfg(test)]
mod test_secp256k1;
