//! ECDSA, EdDSA, and ElGamal
use std::convert::TryFrom;
use std::fmt::Write;
use std::ops::Deref;
use std::str::FromStr;

use ethereum_types::H160;
use hex;
use rand::SeedableRng;
use rand_hc::Hc128Rng;
use serde::Deserialize;
use serde::Serialize;
use sha1::Digest;
use sha1::Sha1;
use subtle::CtOption;

use crate::error::Error;
use crate::error::Result;
pub mod elgamal;
pub mod signers;
mod types;
use elliptic_curve::generic_array::typenum::U32;
use elliptic_curve::generic_array::GenericArray;
use elliptic_curve::point::AffineCoordinates;
use elliptic_curve::point::DecompressPoint;
use elliptic_curve::FieldBytes;
use p256::NistP256;
use subtle::Choice;
pub use types::PublicKey;

/// ref <https://docs.rs/web3/0.18.0/src/web3/signing.rs.html#69>
///
/// length r: 32, length s: 32, length v(recovery_id): 1
pub type SigBytes = [u8; 65];
/// Alias PublicKey.
pub type CurveEle = PublicKey;
/// PublicKeyAddress is H160.
pub type PublicKeyAddress = H160;

/// Wrap libsecp256k1::SecretKey.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub struct SecretKey(libsecp256k1::SecretKey);

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
    pub fn new<T: Into<String>>(s: T) -> Self {
        HashStr(s.into())
    }

    pub fn inner(&self) -> String {
        self.0.clone()
    }
}

impl Deref for SecretKey {
    type Target = libsecp256k1::SecretKey;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<SecretKey> for libsecp256k1::SecretKey {
    fn from(key: SecretKey) -> Self {
        *key.deref()
    }
}

impl TryFrom<PublicKey> for libsecp256k1::PublicKey {
    type Error = Error;
    fn try_from(key: PublicKey) -> Result<Self> {
        Self::parse_compressed(&key.0).map_err(|_| Error::ECDSAPublicKeyBadFormat)
    }
}

impl TryFrom<PublicKey> for ed25519_dalek::PublicKey {
    type Error = Error;
    fn try_from(key: PublicKey) -> Result<Self> {
        // pubkey[0] == 0
        Self::from_bytes(&key.0[1..]).map_err(|_| Error::EdDSAPublicKeyBadFormat)
    }
}

impl AffineCoordinates for PublicKey {
    type FieldRepr = GenericArray<u8, U32>;

    fn x(&self) -> Self::FieldRepr {
        let x: [u8; 32] = self.0[1..].try_into().expect("Expecting length is 32");
        GenericArray::<u8, U32>::from(x)
    }

    fn y_is_odd(&self) -> subtle::Choice {
        Choice::from(self.0[0])
    }
}

impl PublicKey {
    /// Map a PublicKey into secp256r1 affine point,
    /// This function is an constant-time cryptographic implementations
    pub fn ct_into_secp256r1_affine(self) -> CtOption<primeorder::AffinePoint<NistP256>> {
        primeorder::AffinePoint::<NistP256>::decompress(&self.x(), self.y_is_odd())
    }

    /// Map a PublicKey into secp256r1 public key,
    /// This function is an constant-time cryptographic implementations
    pub fn ct_try_into_secp256_pubkey(self) -> CtOption<Result<ecdsa::VerifyingKey<NistP256>>> {
        let opt_affine: CtOption<primeorder::AffinePoint<NistP256>> = self.ct_into_secp256r1_affine();
        opt_affine.and_then(|affine| {
            let ret = ecdsa::VerifyingKey::<NistP256>::from_affine(affine)
                .map_err(|e| Error::ECDSASecp256r1PublicKeyBadFormat(e));
            CtOption::new(ret, Choice::from(1))
        })
    }
}

impl Into<FieldBytes<NistP256>> for SecretKey {
    fn into(self) -> FieldBytes<NistP256> {
	GenericArray::<u8, U32>::from(self.ser())
    }
}

impl From<ed25519_dalek::PublicKey> for PublicKey {
    fn from(key: ed25519_dalek::PublicKey) -> Self {
        // [u8;32] here
        // ref: https://docs.rs/ed25519-dalek/latest/ed25519_dalek/struct.PublicKey.html
        let mut s = key.to_bytes().as_slice().to_vec();
        // [u8;32] + [0]
        s.reverse();
        s.push(0);
        s.reverse();
        Self(s.as_slice().try_into().unwrap())
    }
}

impl TryFrom<PublicKey> for libsecp256k1::curve::Affine {
    type Error = Error;
    fn try_from(key: PublicKey) -> Result<Self> {
        Ok(TryInto::<libsecp256k1::PublicKey>::try_into(key)?.into())
    }
}

impl TryFrom<libsecp256k1::curve::Affine> for PublicKey {
    type Error = Error;
    fn try_from(a: libsecp256k1::curve::Affine) -> Result<Self> {
        let pubkey: libsecp256k1::PublicKey = a.try_into().map_err(|_| Error::InvalidPublicKey)?;
        Ok(pubkey.into())
    }
}

impl From<SecretKey> for libsecp256k1::curve::Scalar {
    fn from(key: SecretKey) -> libsecp256k1::curve::Scalar {
        key.0.into()
    }
}

impl From<libsecp256k1::SecretKey> for SecretKey {
    fn from(key: libsecp256k1::SecretKey) -> Self {
        Self(key)
    }
}

impl From<libsecp256k1::PublicKey> for PublicKey {
    fn from(key: libsecp256k1::PublicKey) -> Self {
        Self(key.serialize_compressed())
    }
}

impl From<SecretKey> for PublicKey {
    fn from(secret_key: SecretKey) -> Self {
        libsecp256k1::PublicKey::from_secret_key(&secret_key.0).into()
    }
}

impl<T> From<T> for HashStr
where T: Into<String>
{
    fn from(s: T) -> Self {
        let inputs = s.into();
        let mut hasher = Sha1::new();
        hasher.update(inputs.as_bytes());
        let bytes = hasher.finalize();
        let mut ret = String::with_capacity(bytes.len() * 2);
        for &b in &bytes {
            write!(ret, "{:02x}", b).unwrap();
        }
        HashStr(ret)
    }
}

impl TryFrom<&str> for SecretKey {
    type Error = Error;
    fn try_from(s: &str) -> Result<Self> {
        let key = hex::decode(s)?;
        let key_arr: [u8; 32] = key.as_slice().try_into()?;
        match libsecp256k1::SecretKey::parse(&key_arr) {
            Ok(key) => Ok(key.into()),
            Err(e) => Err(Error::Libsecp256k1SecretKeyParse(format!("{:?}", e))),
        }
    }
}

impl std::str::FromStr for SecretKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::try_from(s)
    }
}

impl ToString for SecretKey {
    fn to_string(&self) -> String {
        hex::encode(self.0.serialize())
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

fn public_key_address(pubkey: &PublicKey) -> PublicKeyAddress {
    let hash = match TryInto::<libsecp256k1::PublicKey>::try_into(*pubkey) {
        // if pubkey is ecdsa key
        Ok(pk) => {
            let data = pk.serialize();
            debug_assert_eq!(data[0], 0x04);
            keccak256(&data[1..])
        }
        // if pubkey is eddsa key
        Err(_) => keccak256(&pubkey.0[1..]),
    };
    PublicKeyAddress::from_slice(&hash[12..])
}

fn secret_key_address(secret_key: &SecretKey) -> PublicKeyAddress {
    let public_key = libsecp256k1::PublicKey::from_secret_key(secret_key);
    public_key_address(&public_key.into())
}

impl SecretKey {
    pub fn random() -> Self {
        let mut rng = Hc128Rng::from_entropy();
        Self(libsecp256k1::SecretKey::random(&mut rng))
    }

    pub fn address(&self) -> PublicKeyAddress {
        secret_key_address(self)
    }

    pub fn sign(&self, message: &str) -> SigBytes {
        self.sign_raw(message.as_bytes())
    }

    pub fn sign_raw(&self, message: &[u8]) -> SigBytes {
        let message_hash = keccak256(message);
        self.sign_hash(&message_hash)
    }

    pub fn sign_hash(&self, message_hash: &[u8; 32]) -> SigBytes {
        let (signature, recover_id) =
            libsecp256k1::sign(&libsecp256k1::Message::parse(message_hash), self);
        let mut sig_bytes: SigBytes = [0u8; 65];
        sig_bytes[0..32].copy_from_slice(&signature.r.b32());
        sig_bytes[32..64].copy_from_slice(&signature.s.b32());
        sig_bytes[64] = recover_id.serialize();
        sig_bytes
    }

    pub fn pubkey(&self) -> PublicKey {
        libsecp256k1::PublicKey::from_secret_key(&(*self).into()).into()
    }

    pub fn ser(&self) -> [u8; 32] {
        self.0.serialize()
    }
}

impl PublicKey {
    pub fn address(&self) -> PublicKeyAddress {
        public_key_address(self)
    }
}

/// Recover PublicKey from RawMessage using signature.
pub fn recover<S>(message: &[u8], signature: S) -> Result<PublicKey>
where S: AsRef<[u8]> {
    let sig_bytes: SigBytes = signature.as_ref().try_into()?;
    let message_hash: [u8; 32] = keccak256(message);
    recover_hash(&message_hash, &sig_bytes)
}

/// Recover PublicKey from HashMessage using signature.
pub fn recover_hash(message_hash: &[u8; 32], sig: &[u8; 65]) -> Result<PublicKey> {
    let r_s_signature: [u8; 64] = sig[..64].try_into()?;
    let recovery_id: u8 = sig[64];
    Ok(libsecp256k1::recover(
        &libsecp256k1::Message::parse(message_hash),
        &libsecp256k1::Signature::parse_standard(&r_s_signature)
            .map_err(|e| Error::Libsecp256k1SignatureParseStandard(e.to_string()))?,
        &libsecp256k1::RecoveryId::parse(recovery_id)
            .map_err(|e| Error::Libsecp256k1RecoverIdParse(e.to_string()))?,
    )
    .map_err(|_| Error::Libsecp256k1Recover)?
    .into())
}

#[cfg(test)]
pub mod tests {
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
        let (_, recover_id) = libsecp256k1::sign(&libsecp256k1::Message::parse(&h), key);
        assert_eq!(recover_id.serialize(), 0);
        let mut sig = key.sign_raw(&prefix_msg);
        sig[64] = 27;
        assert_eq!(sig, metamask_sig.as_slice());
    }

    #[test]
    fn test_recover() {
        let key = SecretKey::random();
        let pubkey1 = key.pubkey();
        let pubkey2 = recover("hello".as_bytes(), key.sign("hello")).unwrap();
        assert_eq!(pubkey1, pubkey2);
    }

    pub fn gen_ordered_keys(n: usize) -> Vec<SecretKey> {
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
