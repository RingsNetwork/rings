//! ECDSA, EdDSA, and ElGamal
use std::convert::TryFrom;
use std::str::FromStr;

use ethereum_types::H160;
use hex;
use k256::ecdsa::RecoveryId;
use k256::ecdsa::Signature as K256Signature;
use k256::ecdsa::SigningKey as K256SigningKey;
use k256::ecdsa::VerifyingKey as K256VerifyingKey;
use k256::AffinePoint as K256AffinePoint;
use k256::PublicKey as K256PublicKey;
use k256::Scalar as K256Scalar;
use k256::SecretKey as K256SecretKey;
use rand::SeedableRng;
use rand_hc::Hc128Rng;
use serde::Deserialize;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;
use subtle::CtOption;
use zeroize::Zeroize;

use crate::error::Error;
use crate::error::Result;
pub mod elgamal;
pub mod group;
pub mod keys;
/// Signature schemes used by DID identity and provider login.
pub mod signers;
mod types;
use elliptic_curve::generic_array::typenum::U32;
use elliptic_curve::generic_array::GenericArray;
use elliptic_curve::point::AffineCoordinates;
use elliptic_curve::point::DecompressPoint;
use elliptic_curve::sec1::ToEncodedPoint;
use elliptic_curve::FieldBytes;
pub use group::*;
pub use keys::*;
use p256::NistP256;
use subtle::Choice;
pub use types::PublicKey;

/// ref <https://docs.rs/web3/0.18.0/src/web3/signing.rs.html#69>
///
/// length r: 32, length s: 32, length v(recovery_id): 1
pub type SigBytes = [u8; 65];
/// Alias PublicKey.
pub type CurveEle<const SIZE: usize> = PublicKey<SIZE>;
/// PublicKeyAddress is H160.
pub type PublicKeyAddress = H160;

/// Secp256k1 secret key bytes.
///
/// The bytes are validated at construction time and stay in the canonical
/// external format used by existing configs and DIDs.
#[derive(PartialEq, Eq, Clone)]
pub struct SecretKey([u8; 32]);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SecretKey").field(&"<redacted>").finish()
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Wrap String into HashStr.
#[derive(Deserialize, Serialize, Debug, Clone, Eq, PartialEq)]
pub struct HashStr(String);

/// Compute the Keccak-256 hash of input bytes.
pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    use tiny_keccak::Hasher;
    use tiny_keccak::Keccak;
    let mut output = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(bytes);
    hasher.finalize(&mut output);
    output
}

impl HashStr {
    /// Create a hash string wrapper from an existing string value.
    pub fn new<T: Into<String>>(s: T) -> Self {
        HashStr(s.into())
    }

    /// Compute the SHA-1 digest of raw bytes and encode it as lowercase hex.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        HashStr(hex::encode(hasher.finalize()))
    }

    /// Return the wrapped hash string.
    pub fn inner(&self) -> String {
        self.0.clone()
    }
}

impl TryFrom<PublicKey<33>> for K256PublicKey {
    type Error = Error;
    fn try_from(key: PublicKey<33>) -> Result<Self> {
        Self::from_sec1_bytes(&key.0).map_err(|_| Error::ECDSAPublicKeyBadFormat)
    }
}

impl TryFrom<PublicKey<33>> for ed25519_dalek::VerifyingKey {
    type Error = Error;
    fn try_from(key: PublicKey<33>) -> Result<Self> {
        // pubkey[0] == 0
        let [_, bytes @ ..] = key.0;
        Self::from_bytes(&bytes).map_err(|_| Error::EdDSAPublicKeyBadFormat)
    }
}

impl AffineCoordinates for PublicKey<33> {
    type FieldRepr = GenericArray<u8, U32>;

    fn x(&self) -> Self::FieldRepr {
        let [_, x @ ..] = self.0;
        GenericArray::<u8, U32>::from(x)
    }

    fn y_is_odd(&self) -> subtle::Choice {
        let [prefix, ..] = self.0;
        match prefix {
            2u8 => Choice::from(1),
            3u8 => Choice::from(0),
            _ => Choice::from(0),
        }
    }
}

impl PublicKey<33> {
    /// Map a PublicKey into secp256r1 affine point,
    /// This function is an constant-time cryptographic implementations
    pub fn ct_into_secp256r1_affine(self) -> CtOption<primeorder::AffinePoint<NistP256>> {
        primeorder::AffinePoint::<NistP256>::decompress(&self.x(), self.y_is_odd())
    }

    /// Map a PublicKey into secp256r1 public key,
    /// This function is an constant-time cryptographic implementations
    pub fn ct_try_into_secp256r1_pubkey(self) -> CtOption<Result<ecdsa::VerifyingKey<NistP256>>> {
        let opt_affine: CtOption<primeorder::AffinePoint<NistP256>> =
            self.ct_into_secp256r1_affine();
        opt_affine.and_then(|affine| {
            let ret =
                ecdsa::VerifyingKey::<NistP256>::from_affine(affine).map_err(Error::ECDSAError);
            match ret {
                Ok(_r) => CtOption::new(ret, Choice::from(1)),
                Err(_) => CtOption::new(ret, Choice::from(0)),
            }
        })
    }
}

impl From<SecretKey> for FieldBytes<NistP256> {
    fn from(val: SecretKey) -> Self {
        Self::from(&val)
    }
}

impl From<&SecretKey> for FieldBytes<NistP256> {
    fn from(val: &SecretKey) -> Self {
        GenericArray::<u8, U32>::from(val.ser())
    }
}

impl From<ed25519_dalek::VerifyingKey> for PublicKey<33> {
    fn from(key: ed25519_dalek::VerifyingKey) -> Self {
        // [u8;32] here
        // ref: https://docs.rs/ed25519-dalek/latest/ed25519_dalek/struct.VerifyingKey.html
        let mut data = [0u8; 33];
        let key_bytes = key.to_bytes();
        if let Some(suffix) = data.get_mut(1..) {
            suffix.copy_from_slice(&key_bytes);
        }
        Self(data)
    }
}

impl TryFrom<PublicKey<33>> for K256AffinePoint {
    type Error = Error;
    fn try_from(key: PublicKey<33>) -> Result<Self> {
        Ok(TryInto::<K256PublicKey>::try_into(key)?
            .to_projective()
            .to_affine())
    }
}

impl TryFrom<K256AffinePoint> for PublicKey<33> {
    type Error = Error;
    fn try_from(a: K256AffinePoint) -> Result<Self> {
        let encoded = a.to_encoded_point(true);
        let data: [u8; 33] = encoded
            .as_bytes()
            .try_into()
            .map_err(|_| Error::InvalidPublicKey)?;
        Ok(Self(data))
    }
}

impl From<K256PublicKey> for PublicKey<33> {
    fn from(key: K256PublicKey) -> Self {
        let encoded = key.to_encoded_point(true);
        let mut data = [0u8; 33];
        if encoded.as_bytes().len() == data.len() {
            data.copy_from_slice(encoded.as_bytes());
        }
        Self(data)
    }
}

impl From<K256VerifyingKey> for PublicKey<33> {
    fn from(key: K256VerifyingKey) -> Self {
        let encoded = key.to_encoded_point(true);
        let mut data = [0u8; 33];
        if encoded.as_bytes().len() == data.len() {
            data.copy_from_slice(encoded.as_bytes());
        }
        Self(data)
    }
}

impl From<SecretKey> for PublicKey<33> {
    fn from(secret_key: SecretKey) -> Self {
        secret_key.pubkey()
    }
}

impl From<&SecretKey> for PublicKey<33> {
    fn from(secret_key: &SecretKey) -> Self {
        secret_key.pubkey()
    }
}

impl<T> From<T> for HashStr
where T: Into<String>
{
    fn from(s: T) -> Self {
        let inputs = s.into();
        HashStr::from_bytes(inputs.as_bytes())
    }
}

impl TryFrom<&str> for SecretKey {
    type Error = Error;
    fn try_from(s: &str) -> Result<Self> {
        let key = hex::decode(s)?;
        let key_arr: [u8; 32] = key.as_slice().try_into()?;
        Self::from_bytes(key_arr)
    }
}

impl std::str::FromStr for SecretKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::try_from(s)
    }
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for SecretKey {
    fn to_string(&self) -> String {
        hex::encode(self.0)
    }
}

struct SecretKeyVisitor;

impl<'de> serde::de::Visitor<'de> for SecretKeyVisitor {
    type Value = SecretKey;

    fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        formatter.write_str("SecretKey deserializer")
    }
    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where E: serde::de::Error {
        SecretKey::from_str(value).map_err(|e| serde::de::Error::custom(e))
    }
}

impl<'de> Deserialize<'de> for SecretKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        deserializer.deserialize_str(SecretKeyVisitor)
    }
}

impl Serialize for SecretKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where S: serde::Serializer {
        serializer.serialize_str(self.to_string().as_str())
    }
}

fn public_key_address(pubkey: &PublicKey<33>) -> PublicKeyAddress {
    let hash = match TryInto::<K256PublicKey>::try_into(*pubkey) {
        // if pubkey is ecdsa key
        Ok(pk) => {
            let data = pk.to_encoded_point(false);
            let data = data.as_bytes();
            debug_assert_eq!(data.first(), Some(&0x04));
            keccak256(data.get(1..).unwrap_or_default())
        }
        // if pubkey is eddsa key
        Err(_) => keccak256(pubkey.0.get(1..).unwrap_or_default()),
    };
    PublicKeyAddress::from_slice(&hash[12..])
}

fn secret_key_address(secret_key: &SecretKey) -> PublicKeyAddress {
    secret_key.pubkey().address()
}

impl SecretKey {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        K256SecretKey::from_slice(&bytes).map_err(|_| Error::PrivateKeyBadFormat)?;
        Ok(Self(bytes))
    }

    pub(crate) fn secp256k1_scalar(&self) -> Result<K256Scalar> {
        let secret = K256SecretKey::from_slice(&self.0).map_err(|_| Error::PrivateKeyBadFormat)?;
        Ok(*secret.to_nonzero_scalar().as_ref())
    }

    /// Generate a random secp256k1 secret key.
    pub fn random() -> Self {
        let mut rng = Hc128Rng::from_entropy();
        let bytes = K256SecretKey::random(&mut rng).to_bytes();
        Self(bytes.into())
    }

    /// Derive the Ethereum-style address for this secret key.
    pub fn address(&self) -> PublicKeyAddress {
        secret_key_address(self)
    }

    /// Sign a UTF-8 message after hashing it with Keccak-256.
    ///
    /// Returns an error when the stored key is invalid or signing fails.
    pub fn sign(&self, message: &str) -> Result<SigBytes> {
        self.sign_raw(message.as_bytes())
    }

    /// Sign raw message bytes after hashing them with Keccak-256.
    ///
    /// Returns an error when the stored key is invalid or signing fails.
    pub fn sign_raw(&self, message: &[u8]) -> Result<SigBytes> {
        let message_hash = keccak256(message);
        self.sign_hash(&message_hash)
    }

    /// Sign an already computed 32-byte message hash.
    ///
    /// Returns an error when the stored key is invalid or signing fails.
    pub fn sign_hash(&self, message_hash: &[u8; 32]) -> Result<SigBytes> {
        let signing_key =
            K256SigningKey::from_slice(&self.0).map_err(|_| Error::PrivateKeyBadFormat)?;
        let (signature, recover_id) = signing_key.sign_prehash_recoverable(message_hash)?;
        let mut sig_bytes: SigBytes = [0u8; 65];
        sig_bytes[0..64].copy_from_slice(signature.to_bytes().as_slice());
        sig_bytes[64] = recover_id.to_byte();
        Ok(sig_bytes)
    }

    /// Derive the compressed public key for this secret key.
    pub fn pubkey(&self) -> PublicKey<33> {
        match K256SecretKey::from_slice(&self.0) {
            Ok(secret_key) => secret_key.public_key().into(),
            Err(_) => PublicKey([0u8; 33]),
        }
    }

    /// Serialize this secret key into its 32-byte representation.
    pub fn ser(&self) -> [u8; 32] {
        self.0
    }
}

impl PublicKey<33> {
    /// Derive the Ethereum-style address for this public key.
    pub fn address(&self) -> PublicKeyAddress {
        public_key_address(self)
    }
}

/// Recover PublicKey from RawMessage using signature.
pub fn recover<S>(message: &[u8], signature: S) -> Result<PublicKey<33>>
where S: AsRef<[u8]> {
    let sig_bytes: SigBytes = signature.as_ref().try_into()?;
    let message_hash: [u8; 32] = keccak256(message);
    recover_hash(&message_hash, &sig_bytes)
}

/// Recover PublicKey from HashMessage using signature.
pub fn recover_hash(message_hash: &[u8; 32], sig: &[u8; 65]) -> Result<PublicKey<33>> {
    let r_s_signature: [u8; 64] = sig[..64].try_into()?;
    let recovery_id: u8 = sig[64];
    let signature = K256Signature::try_from(r_s_signature.as_slice()).map_err(Error::ECDSAError)?;
    if signature.normalize_s().is_some() {
        return Err(Error::NonCanonicalSignature);
    }
    let recovery_id =
        RecoveryId::from_byte(recovery_id).ok_or(Error::InvalidRecoverId(recovery_id))?;
    Ok(
        K256VerifyingKey::recover_from_prehash(message_hash, &signature, recovery_id)
            .map_err(Error::ECDSAError)?
            .into(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use hex::FromHex;

    use super::*;

    #[test]
    fn test_parse_to_string_with_sha10x00() {
        let s = "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0";
        let t: HashStr = s.into();
        assert_eq!(t.0.len(), 40);
    }

    #[test]
    fn test_parse_to_string_with_sha10x01() {
        let s = "hello";
        let t: HashStr = s.into();
        assert_eq!(t.0.len(), 40);
    }

    #[test]
    fn test_metamask_sign_for_debug() {
        let key = &SecretKey::try_from(
            "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0",
        )
        .unwrap();
        let sig_hash =
            Vec::from_hex("4a5c5d454721bbbb25540c3317521e71c373ae36458f960d2ad46ef088110e95")
                .unwrap();
        let msg = "test";
        // https://docs.rs/web3/latest/src/web3/signing.rs.html#221
        let prefix_msg_ret = "\x19Ethereum Signed Message:\n4test"
            .to_string()
            .into_bytes();
        let mut prefix_msg = format!("\x19Ethereum Signed Message:\n{}", msg.len()).into_bytes();
        prefix_msg.extend_from_slice(msg.as_bytes());
        assert_eq!(
            prefix_msg,
            prefix_msg_ret,
            "{}",
            String::from_utf8(prefix_msg.clone()).unwrap()
        );
        //        let hash = hash_message(msg.as_bytes()).0;
        assert_eq!(keccak256(prefix_msg_ret.as_slice()), sig_hash.as_slice());
        // window.ethereum.request({method: "personal_sign", params: ["test", "0x11E807fcc88dD319270493fB2e822e388Fe36ab0"]})
        let metamask_sig = Vec::from_hex("724fc31d9272b34d8406e2e3a12a182e72510b008de6cc44684577e31e20d9626fb760d6a0badd79a6cf4cd56b2fc0fbd60c438b809aa7d29bfb598c13e7b50e1b").unwrap();
        assert_eq!(metamask_sig.len(), 65);
        let h: [u8; 32] = sig_hash.as_slice().try_into().unwrap();
        let recover_id = key.sign_hash(&h).unwrap()[64];
        assert_eq!(recover_id, 0);
        let mut sig = key.sign_raw(&prefix_msg).unwrap();
        sig[64] = 27;
        assert_eq!(sig, metamask_sig.as_slice());
    }

    #[test]
    fn test_recover() {
        let key = SecretKey::random();
        let pubkey1 = key.pubkey();
        let pubkey2 = recover("hello".as_bytes(), key.sign("hello").unwrap()).unwrap();
        assert_eq!(pubkey1, pubkey2);
    }

    #[test]
    fn secret_key_redacts_debug_and_zeroizes_on_drop() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();

        assert_eq!(format!("{key:?}"), "SecretKey(\"<redacted>\")");
        assert!(std::mem::needs_drop::<SecretKey>());
    }

    #[test]
    fn invalid_secret_key_state_returns_explicit_errors() {
        let invalid = SecretKey([0u8; 32]);

        assert!(matches!(
            invalid.sign_hash(&[0u8; 32]),
            Err(Error::PrivateKeyBadFormat)
        ));
        assert!(matches!(
            invalid.secp256k1_scalar(),
            Err(Error::PrivateKeyBadFormat)
        ));
    }

    #[test]
    fn recover_hash_rejects_high_s_signature() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let hash = keccak256(b"canonical signature");
        let mut signature_bytes = key.sign_hash(&hash).unwrap();
        let signature = K256Signature::try_from(&signature_bytes[..64]).unwrap();
        let (r, s) = signature.split_scalars();
        let high_s = K256Signature::from_scalars(r.to_bytes(), (-s).to_bytes()).unwrap();

        signature_bytes[..64].copy_from_slice(&high_s.to_bytes());
        signature_bytes[64] ^= 1;

        assert!(matches!(
            recover_hash(&hash, &signature_bytes),
            Err(Error::NonCanonicalSignature)
        ));
    }

    pub(crate) fn gen_ordered_keys(n: usize) -> Vec<SecretKey> {
        let mut keys = Vec::from_iter(std::iter::repeat_with(SecretKey::random).take(n));
        keys.sort_by(|a, b| {
            if a.address() < b.address() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        keys
    }
}
