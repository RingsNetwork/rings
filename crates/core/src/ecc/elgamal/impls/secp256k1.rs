//! secp256k1 plaintext and ciphertext adapter for ElGamal.
//!
//! The generic ElGamal implementation encrypts group elements. Existing Rings
//! callers encrypt strings and exchange `PublicKey<33>` values, so this module
//! provides the compatibility layer:
//!
//! - map UTF-8 bytes into secp256k1 points with a reversible x-coordinate
//!   encoding;
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

use std::convert::TryFrom;
use std::convert::TryInto;

use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::KeyInit;
use chacha20poly1305::aead::Payload;
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::Key;
use chacha20poly1305::Nonce;
use hkdf::Hkdf;
use libsecp256k1::curve::Affine;
use libsecp256k1::curve::Field;
use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;
use sha2::Sha256;
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

const AEAD_VERSION: u8 = 1;
const AEAD_KEY_LEN: usize = 32;
const AEAD_NONCE_LEN: usize = 12;
const AEAD_HKDF_SALT: &[u8] = b"rings-core:secp256k1-elgamal-aead:salt:v1";
const AEAD_HKDF_INFO: &[u8] = b"rings-core:secp256k1-elgamal-aead:chacha20poly1305:v1";

/// KEM/DEM ciphertext using secp256k1 ElGamal to wrap a ChaCha20-Poly1305 key.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Serialize)]
struct AeadTranscript<'a> {
    version: u8,
    encrypted_key: &'a [CiphertextBlock],
    aad: &'a [u8],
}

/// Plaintext input before it is mapped into secp256k1 group elements.
pub struct Plaintext<'a>(&'a str);

/// Binary plaintext input before it is mapped into secp256k1 group elements.
pub struct PlaintextBytes<'a>(&'a [u8]);

/// secp256k1 group elements that encode one plaintext message.
pub struct MessagePoints(Vec<Point<Secp256k1>>);

impl<'a> Plaintext<'a> {
    /// Plaintext string before group encoding.
    pub fn as_str(&self) -> &'a str {
        self.0
    }
}

impl<'a> PlaintextBytes<'a> {
    /// Plaintext bytes before group encoding.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.0
    }
}

impl MessagePoints {
    /// Group elements after plaintext encoding.
    pub fn into_vec(self) -> Vec<Point<Secp256k1>> {
        self.0
    }
}

impl<'a> From<&'a str> for Plaintext<'a> {
    fn from(message: &'a str) -> Self {
        Self(message)
    }
}

impl<'a> From<&'a [u8]> for PlaintextBytes<'a> {
    fn from(message: &'a [u8]) -> Self {
        Self(message)
    }
}

impl From<Vec<Point<Secp256k1>>> for MessagePoints {
    fn from(points: Vec<Point<Secp256k1>>) -> Self {
        Self(points)
    }
}

impl IntoIterator for MessagePoints {
    type IntoIter = std::vec::IntoIter<Point<Secp256k1>>;
    type Item = Point<Secp256k1>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> TryFrom<Plaintext<'a>> for MessagePoints {
    type Error = Error;

    fn try_from(message: Plaintext<'a>) -> Result<Self> {
        MessagePoints::try_from(PlaintextBytes::from(message.as_str().as_bytes()))
    }
}

impl<'a> TryFrom<PlaintextBytes<'a>> for MessagePoints {
    type Error = Error;

    fn try_from(message: PlaintextBytes<'a>) -> Result<Self> {
        Ok(bytes_to_affine(message.as_bytes())?
            .into_iter()
            .map(Point::<Secp256k1>::from)
            .collect::<Vec<_>>()
            .into())
    }
}

impl TryFrom<MessagePoints> for String {
    type Error = Error;

    fn try_from(points: MessagePoints) -> Result<Self> {
        String::from_utf8(Vec::<u8>::try_from(points)?).map_err(Error::Utf8Encoding)
    }
}

impl TryFrom<MessagePoints> for Vec<u8> {
    type Error = Error;

    fn try_from(points: MessagePoints) -> Result<Self> {
        let affines = points
            .into_iter()
            .map(Affine::from)
            .collect::<Vec<Affine>>();
        Ok(affine_to_bytes(&affines))
    }
}

/// Convert a string into field elements using the adapter encoding.
///
/// Each plaintext chunk is at most 29 bytes so the field candidate can carry
/// `0xFF || 0x52 || len || chunk || zero padding`. The first byte is only a
/// search bias for `lift_x`; the marker and length bytes make the mapping
/// reversible and preserve leading or embedded NUL bytes.
pub fn str_to_field(s: &str) -> Vec<Field> {
    bytes_to_field(s.as_bytes())
}

/// Convert arbitrary bytes into field elements using the adapter encoding.
pub fn bytes_to_field(bytes: &[u8]) -> Vec<Field> {
    bytes
        .chunks(PLAINTEXT_BLOCK_SIZE)
        .map(|x| {
            let data = encode_field_candidate(x);
            let mut field = Field::default();
            // `FIELD_ENCODING_MARKER` is below the secp256k1 modulus prefix, so
            // the encoded 32-byte candidate always fits in the field.
            let _ = field.set_b32(&data);
            field
        })
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

/// Decode field elements produced by [`str_to_field`].
pub fn field_to_str(f: &[Field]) -> Result<String> {
    String::from_utf8(field_to_bytes(f)).map_err(Error::Utf8Encoding)
}

/// Decode field elements produced by [`bytes_to_field`].
pub fn field_to_bytes(f: &[Field]) -> Vec<u8> {
    f.iter().fold(vec![], |mut acc, x| {
        let mut field = *x;
        field.normalize();
        acc.extend(decode_field_bytes(field.b32()));
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
    let mut ec = Affine::default();
    let mut x = *x;
    x.normalize();

    if ec.set_xo_var(&x, x.is_odd()) {
        return Ok(ec);
    }

    for bias in (1..=254).rev() {
        let mut bytes = x.b32();
        let Some(first) = bytes.first_mut() else {
            return Err(Error::Secp256k1PointLiftFailed);
        };
        *first = bias;

        let mut candidate = Field::default();
        if !candidate.set_b32(&bytes) {
            continue;
        }
        candidate.normalize();

        if ec.set_xo_var(&candidate, candidate.is_odd()) {
            ec.x.normalize();
            ec.y.normalize();
            return Ok(ec);
        }
    }

    // Typed safeguard for future encoding changes; normal adapter chunks should
    // find a valid lift among the high-byte candidates above.
    Err(Error::Secp256k1PointLiftFailed)
}

/// Convert a string into secp256k1 points using the adapter encoding.
pub fn str_to_affine(s: &str) -> Result<Vec<Affine>> {
    bytes_to_affine(s.as_bytes())
}

/// Convert arbitrary bytes into secp256k1 points using the adapter encoding.
pub fn bytes_to_affine(bytes: &[u8]) -> Result<Vec<Affine>> {
    bytes_to_field(bytes)
        .into_iter()
        .map(|a| lift_x(&a))
        .collect::<Result<Vec<Affine>>>()
}

/// Decode secp256k1 points produced by `str_to_affine`.
pub fn affine_to_str(a: &[Affine]) -> Result<String> {
    String::from_utf8(affine_to_bytes(a)).map_err(Error::Utf8Encoding)
}

/// Decode secp256k1 points produced by [`bytes_to_affine`].
pub fn affine_to_bytes(a: &[Affine]) -> Vec<u8> {
    field_to_bytes(a.iter().map(|x| x.x).collect::<Vec<Field>>().as_slice())
}

/// Encrypt a string with the current secp256k1 compatibility adapter.
pub fn encrypt(s: &str, k: PublicKey<33>) -> Result<Vec<CiphertextBlock>> {
    let public_key = ElGamalPublicKey::<Point<Secp256k1>>::from_element(k.try_into()?);
    let points = MessagePoints::try_from(Plaintext::from(s))?;
    ElGamal::<Point<Secp256k1>>::encrypt(points, &public_key)
        .into_iter()
        .map(|(c1, c2)| Ok((c1.try_into()?, c2.try_into()?)))
        .collect()
}

/// Encrypt a string with caller-supplied randomness for ElGamal ephemerals.
pub fn encrypt_with_rng(
    s: &str,
    k: PublicKey<33>,
    rng: &mut impl RngCore,
) -> Result<Vec<CiphertextBlock>> {
    encrypt_bytes_with_rng(s.as_bytes(), k, rng)
}

/// Encrypt arbitrary bytes with caller-supplied randomness for ElGamal ephemerals.
pub fn encrypt_bytes_with_rng(
    bytes: &[u8],
    k: PublicKey<33>,
    rng: &mut impl RngCore,
) -> Result<Vec<CiphertextBlock>> {
    let public_key = ElGamalPublicKey::<Point<Secp256k1>>::from_element(k.try_into()?);
    let points = MessagePoints::try_from(PlaintextBytes::from(bytes))?;
    ElGamal::<Point<Secp256k1>>::encrypt_with_rng(points, &public_key, rng)
        .into_iter()
        .map(|(c1, c2)| Ok((c1.try_into()?, c2.try_into()?)))
        .collect()
}

/// Decrypt ciphertext produced by the current secp256k1 compatibility adapter.
pub fn decrypt(m: &[CiphertextBlock], k: SecretKey) -> Result<String> {
    String::from_utf8(decrypt_bytes(m, k)?).map_err(Error::Utf8Encoding)
}

/// Decrypt arbitrary bytes produced by [`encrypt_bytes_with_rng`].
pub fn decrypt_bytes(m: &[CiphertextBlock], k: SecretKey) -> Result<Vec<u8>> {
    let secret_key =
        ElGamalSecretKey::<Point<Secp256k1>>::from_scalar(GroupScalar::<Secp256k1>::from(k));
    let ciphertext = m
        .iter()
        .map(|(c1, c2)| Ok(((*c1).try_into()?, (*c2).try_into()?)))
        .collect::<Result<Vec<(Point<Secp256k1>, Point<Secp256k1>)>>>()?;
    let points = ElGamal::<Point<Secp256k1>>::decrypt(&ciphertext, &secret_key);
    Vec::<u8>::try_from(MessagePoints::from(points))
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
    recipient_secret: SecretKey,
) -> Result<Vec<u8>> {
    if sealed.version != AEAD_VERSION {
        return Err(Error::MessageDecryptionFailed(format!(
            "unsupported ElGamal AEAD version {}",
            sealed.version
        )));
    }

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
    bincode::serialize(&AeadTranscript {
        version: AEAD_VERSION,
        encrypted_key,
        aad,
    })
    .map_err(Error::BincodeSerialize)
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;
    use std::time::Instant;

    use libsecp256k1::curve::ECMultContext;
    use libsecp256k1::curve::ECMultGenContext;
    use libsecp256k1::curve::Jacobian;
    use libsecp256k1::curve::Scalar;
    use rand::distributions::Alphanumeric;
    use rand::Rng;
    use rand::SeedableRng;
    use rand_hc::Hc128Rng;

    use super::*;

    fn random(len: usize) -> String {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(len)
            .map(char::from)
            .collect()
    }

    #[test]
    fn test_string_to_field() {
        let t: String = random(1024);
        assert_eq!(field_to_str(&str_to_field(&t)).unwrap(), t);

        let t: String = random(127);
        assert_eq!(field_to_str(&str_to_field(&t)).unwrap(), t);
    }

    #[test]
    fn test_string_to_field_keeps_nul_bytes() {
        let leading_nul = "\0hello";
        assert_eq!(
            field_to_str(&str_to_field(leading_nul)).unwrap(),
            leading_nul
        );

        let chunk_boundary_nul = format!("{}\0tail", "a".repeat(FIELD_CHUNK_SIZE));
        assert_eq!(
            field_to_str(&str_to_field(&chunk_boundary_nul)).unwrap(),
            chunk_boundary_nul
        );
    }

    #[test]
    fn test_bytes_to_field_keeps_binary_payload() {
        let mut payload = vec![0, 255, 1, 2, 3];
        payload.extend((0..FIELD_CHUNK_SIZE * 2).map(|i| (i % 251) as u8));

        assert_eq!(field_to_bytes(&bytes_to_field(&payload)), payload);
    }

    #[test]
    fn test_string_to_affine() {
        let t: String = random(1024);
        assert_eq!(affine_to_str(&str_to_affine(&t).unwrap()).unwrap(), t);

        let t: String = random(127);
        assert_eq!(affine_to_str(&str_to_affine(&t).unwrap()).unwrap(), t);
    }

    #[test]
    fn test_bytes_to_affine() {
        let payload = [0, 1, 2, 3, 255, 128, 0, 42, 77, 0, 99];

        assert_eq!(
            affine_to_bytes(&bytes_to_affine(&payload).unwrap()),
            payload
        );
    }

    #[test]
    fn test_algorithm() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let sec_key: libsecp256k1::SecretKey = key.into();
        let pubkey: libsecp256k1::PublicKey = key.pubkey().try_into().unwrap();
        let mut pub_point: Affine = pubkey.into();
        pub_point.x.normalize();
        pub_point.y.normalize();
        let pub_x = [
            226, 15, 49, 60, 133, 119, 254, 51, 180, 4, 209, 133, 17, 253, 134, 129, 149, 245, 53,
            173, 45, 62, 36, 113, 168, 153, 24, 91, 137, 141, 81, 47,
        ];
        let pub_y = [
            108, 113, 105, 68, 84, 69, 224, 17, 240, 33, 13, 214, 109, 90, 19, 142, 61, 78, 77,
            105, 96, 121, 193, 87, 117, 185, 180, 47, 202, 81, 181, 204,
        ];
        assert_eq!(pub_point.x.b32(), pub_x);
        assert_eq!(pub_point.y.b32(), pub_y);
        let test = "test";
        let points = str_to_affine(test).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(affine_to_str(&str_to_affine(test).unwrap()).unwrap(), test);
        let m_point = points[0];
        let r: libsecp256k1::SecretKey =
            SecretKey::try_from("1f9275dbafdfba81942eb3330b07f38cbee4ebb86bdc2174af9648d5f5509a54")
                .unwrap()
                .into();
        let r_v = [
            31, 146, 117, 219, 175, 223, 186, 129, 148, 46, 179, 51, 11, 7, 243, 140, 190, 228,
            235, 184, 107, 220, 33, 116, 175, 150, 72, 213, 245, 80, 154, 84,
        ];
        let r_sca: Scalar = r.into();
        assert_eq!(r_sca.b32(), r_v);
        let cxt = ECMultGenContext::new_boxed();
        let mut c1 = Jacobian::default();
        cxt.ecmult_gen(&mut c1, &r_sca);
        let mut a_c1 = Affine::from_gej(&c1);

        a_c1.x.normalize();
        a_c1.y.normalize();
        let c1_x = [
            252, 168, 85, 233, 220, 119, 76, 217, 52, 108, 167, 27, 234, 188, 197, 95, 72, 213,
            148, 212, 111, 255, 6, 59, 9, 134, 111, 121, 175, 9, 189, 105,
        ];
        let c1_y = [
            20, 45, 13, 61, 245, 50, 136, 183, 182, 210, 169, 120, 84, 204, 77, 138, 12, 116, 50,
            9, 115, 98, 138, 245, 24, 61, 223, 144, 55, 180, 231, 59,
        ];
        assert_eq!(a_c1.x.b32(), c1_x);
        assert_eq!(a_c1.y.b32(), c1_y);

        let mut mask_point = Jacobian::default();
        let cxt2 = ECMultContext::new_boxed();
        cxt2.ecmult_const(&mut mask_point, &pub_point, &r_sca);
        let mut a_mask = Affine::from_gej(&mask_point);
        a_mask.x.normalize();
        a_mask.y.normalize();

        let mask_x = [
            218, 19, 55, 137, 15, 46, 160, 160, 208, 222, 206, 77, 46, 79, 32, 80, 64, 243, 93, 23,
            223, 130, 148, 226, 131, 17, 254, 95, 43, 95, 35, 34,
        ];

        let mask_y = [
            106, 127, 47, 58, 214, 6, 110, 28, 171, 176, 73, 11, 34, 28, 125, 10, 82, 154, 84, 154,
            11, 80, 191, 68, 111, 197, 98, 224, 84, 116, 208, 115,
        ];
        assert_eq!(a_mask.x.b32(), mask_x);
        assert_eq!(a_mask.y.b32(), mask_y);
        let c2 = mask_point.add_ge(&m_point);
        let c2_y = [
            225, 196, 104, 44, 46, 208, 86, 14, 40, 40, 133, 81, 125, 222, 217, 21, 242, 64, 68,
            206, 194, 27, 61, 193, 20, 18, 110, 198, 39, 60, 214, 200,
        ];
        let c2_x = [
            156, 159, 250, 245, 112, 81, 128, 176, 19, 145, 119, 199, 12, 181, 147, 13, 138, 34,
            205, 124, 119, 235, 28, 243, 77, 11, 100, 13, 159, 164, 188, 247,
        ];
        let mut a_c2 = Affine::from_gej(&c2);
        a_c2.x.normalize();
        a_c2.y.normalize();
        assert_eq!(a_c2.x.b32(), c2_x);
        assert_eq!(a_c2.y.b32(), c2_y);

        let mut t = Jacobian::default();
        cxt2.ecmult_const(&mut t, &a_c1, &sec_key.into());
        let mut a_t = Affine::from_gej(&t);
        let t_x = [
            218, 19, 55, 137, 15, 46, 160, 160, 208, 222, 206, 77, 46, 79, 32, 80, 64, 243, 93, 23,
            223, 130, 148, 226, 131, 17, 254, 95, 43, 95, 35, 34,
        ];
        let t_y = [
            106, 127, 47, 58, 214, 6, 110, 28, 171, 176, 73, 11, 34, 28, 125, 10, 82, 154, 84, 154,
            11, 80, 191, 68, 111, 197, 98, 224, 84, 116, 208, 115,
        ];
        a_t.x.normalize();
        a_t.y.normalize();
        assert_eq!(a_t.x.b32(), t_x);
        assert_eq!(a_t.y.b32(), t_y);

        let ret = c2.add_ge(&a_t.neg());
        let mut a_ret = Affine::from_gej(&ret);
        a_ret.x.normalize();
        a_ret.y.normalize();
        assert_eq!(a_ret.x, m_point.x);
    }

    #[test]
    fn test_encrypt_decrypt() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let t: String = random(1024);
        assert_eq!(decrypt(&encrypt(&t, pubkey).unwrap(), key).unwrap(), t)
    }

    #[test]
    fn test_encrypt_decrypt_keeps_nul_bytes() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let message = format!("\0{}{}", "a".repeat(FIELD_CHUNK_SIZE - 1), "\0tail");
        assert_eq!(
            decrypt(&encrypt(&message, pubkey).unwrap(), key).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_binary_bytes() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let mut rng = Hc128Rng::seed_from_u64(608);
        let mut payload = vec![0, 255, 1, 2, 3];
        payload.extend((0..FIELD_CHUNK_SIZE * 3).map(|i| (i % 251) as u8));

        let ciphertext = encrypt_bytes_with_rng(&payload, pubkey, &mut rng).unwrap();

        assert_eq!(decrypt_bytes(&ciphertext, key).unwrap(), payload);
    }

    #[test]
    fn test_encrypt_with_rng_is_reproducible_for_same_seed() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let message = format!("prefix\0{}tail", "a".repeat(FIELD_CHUNK_SIZE));
        let mut rng_a = Hc128Rng::seed_from_u64(42);
        let mut rng_b = Hc128Rng::seed_from_u64(42);

        let ciphertext_a = encrypt_with_rng(&message, pubkey, &mut rng_a).unwrap();
        let ciphertext_b = encrypt_with_rng(&message, pubkey, &mut rng_b).unwrap();

        assert_eq!(ciphertext_a, ciphertext_b);
        assert_eq!(decrypt(&ciphertext_a, key).unwrap(), message);
    }

    #[test]
    fn test_encrypt_uses_fresh_ephemeral_point_per_block() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let message = random(FIELD_CHUNK_SIZE * 4);
        let ciphertext = encrypt(&message, pubkey).unwrap();

        assert!(ciphertext.len() > 1);
        let unique_c1 = ciphertext
            .iter()
            .map(|(c1, _)| c1.0)
            .collect::<HashSet<_>>();
        assert_eq!(unique_c1.len(), ciphertext.len());
    }

    #[test]
    fn test_decrypt_malformed_ciphertext_returns_error() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let malformed = PublicKey([0u8; 33]);
        let result = std::panic::catch_unwind(|| decrypt(&[(malformed, malformed)], key));

        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn test_aead_encrypt_decrypt_binary_bytes() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let mut rng = Hc128Rng::seed_from_u64(908);
        let mut payload = vec![0, 255, 42, 0, 17];
        payload.extend((0..1024).map(|i| (i % 251) as u8));
        let aad = b"rings-core test associated data";

        let sealed = encrypt_aead_with_rng(&payload, aad, pubkey, &mut rng).unwrap();

        assert_eq!(decrypt_aead(&sealed, aad, key).unwrap(), payload);
    }

    #[test]
    fn test_aead_rejects_ciphertext_tampering() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let mut rng = Hc128Rng::seed_from_u64(909);
        let mut sealed = encrypt_aead_with_rng(b"authenticated", b"aad", pubkey, &mut rng).unwrap();
        let first = sealed.ciphertext.first_mut().unwrap();
        *first ^= 1;

        assert!(matches!(
            decrypt_aead(&sealed, b"aad", key),
            Err(Error::MessageDecryptionFailed(_))
        ));
    }

    #[test]
    fn test_aead_rejects_associated_data_tampering() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let mut rng = Hc128Rng::seed_from_u64(910);
        let sealed = encrypt_aead_with_rng(b"authenticated", b"aad", pubkey, &mut rng).unwrap();

        assert!(matches!(
            decrypt_aead(&sealed, b"other-aad", key),
            Err(Error::MessageDecryptionFailed(_))
        ));
    }

    #[test]
    fn test_aead_rejects_empty_wrapped_key() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let sealed = AeadCiphertext {
            version: AEAD_VERSION,
            encrypted_key: Vec::new(),
            nonce: [0u8; AEAD_NONCE_LEN],
            ciphertext: Vec::new(),
        };

        assert!(matches!(
            decrypt_aead(&sealed, b"aad", key),
            Err(Error::MessageDecryptionFailed(_))
        ));
    }

    #[test]
    #[ignore = "performance probe; run with --ignored --nocapture"]
    fn bench_encrypt_decrypt_4kb() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let pubkey = key.pubkey();
        let message = random(4 * 1024);
        let rounds = 20;
        let start = Instant::now();

        for _ in 0..rounds {
            let ciphertext = encrypt(std::hint::black_box(&message), pubkey).unwrap();
            let plaintext = decrypt(std::hint::black_box(&ciphertext), key).unwrap();
            assert_eq!(plaintext, message);
        }

        let elapsed = start.elapsed();
        println!(
            "secp256k1 ElGamal adapter encrypt+decrypt 4KiB: {:?} total, {:?} per round",
            elapsed,
            elapsed / rounds
        );
    }
}
