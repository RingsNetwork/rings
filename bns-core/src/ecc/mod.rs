use crate::err::{Error, Result};
use hex;
use libsecp256k1::curve::Affine;
use rand::SeedableRng;
use rand_hc::Hc128Rng;
use serde::Deserialize;
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::convert::TryFrom;
use std::fmt::Write;
use std::ops::Deref;
use web3::signing::keccak256;
use web3::types::Address;
pub mod elgamal;
pub mod signers;

// ref https://docs.rs/web3/0.18.0/src/web3/signing.rs.html#69

// length r: 32, length s: 32, length v(recovery_id): 1
pub type SigBytes = [u8; 65];
pub type CurveEle = PublicKey;

#[derive(PartialEq, Debug, Clone, Copy)]
pub struct SecretKey(libsecp256k1::SecretKey);
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone, Copy)]
pub struct PublicKey(libsecp256k1::PublicKey);

#[derive(Deserialize, Serialize, Debug, Clone, Eq, PartialEq)]
pub struct HashStr(String);

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

impl Deref for PublicKey {
    type Target = libsecp256k1::PublicKey;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<SecretKey> for libsecp256k1::SecretKey {
    fn from(key: SecretKey) -> Self {
        *key.deref()
    }
}

impl From<PublicKey> for libsecp256k1::PublicKey {
    fn from(key: PublicKey) -> Self {
        *key.deref()
    }
}

impl From<PublicKey> for libsecp256k1::curve::Affine {
    fn from(key: PublicKey) -> Self {
        (*key.deref()).into()
    }
}

impl TryFrom<libsecp256k1::curve::Affine> for PublicKey {
    type Error = Error;
    fn try_from(a: libsecp256k1::curve::Affine) -> Result<Self> {
        let pubkey: libsecp256k1::PublicKey = a.try_into().map_err(|_| Error::InvalidPublicKey)?;
        Ok(pubkey.into())
    }
}

impl Into<libsecp256k1::curve::Scalar> for SecretKey {
    fn into(self) -> libsecp256k1::curve::Scalar {
        self.0.into()
    }
}

impl From<libsecp256k1::SecretKey> for SecretKey {
    fn from(key: libsecp256k1::SecretKey) -> Self {
        Self(key)
    }
}

impl From<libsecp256k1::PublicKey> for PublicKey {
    fn from(key: libsecp256k1::PublicKey) -> Self {
        Self(key)
    }
}

impl From<SecretKey> for PublicKey {
    fn from(secret_key: SecretKey) -> Self {
        libsecp256k1::PublicKey::from_secret_key(&secret_key.0).into()
    }
}

impl<T> From<T> for HashStr
where
    T: Into<String>,
{
    fn from(s: T) -> Self {
        let inputs = s.into();
        let mut hasher = Sha1::new();
        hasher.update(&inputs.as_bytes());
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
        hex::encode(self.serialize())
    }
}

fn public_key_address(public_key: &PublicKey) -> Address {
    let pub_key: libsecp256k1::PublicKey = *public_key.deref();
    let pub_key = pub_key.serialize();
    debug_assert_eq!(pub_key[0], 0x04);
    let hash = keccak256(&pub_key[1..]);
    Address::from_slice(&hash[12..])
}

fn secret_key_address(secret_key: &SecretKey) -> Address {
    let public_key = libsecp256k1::PublicKey::from_secret_key(secret_key);
    public_key_address(&public_key.into())
}

impl SecretKey {
    pub fn random() -> Self {
        let mut rng = Hc128Rng::from_entropy();
        Self(libsecp256k1::SecretKey::random(&mut rng))
    }

    pub fn address(&self) -> Address {
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
            libsecp256k1::sign(&libsecp256k1::Message::parse(message_hash), &*self);
        let mut sig_bytes: SigBytes = [0u8; 65];
        sig_bytes[0..32].copy_from_slice(&signature.r.b32());
        sig_bytes[32..64].copy_from_slice(&signature.s.b32());
        sig_bytes[64] = recover_id.serialize();
        sig_bytes
    }

    pub fn pubkey(&self) -> PublicKey {
        libsecp256k1::PublicKey::from_secret_key(&(*self).into()).into()
    }
}

impl PublicKey {
    pub fn address(&self) -> Address {
        public_key_address(self)
    }
}

pub fn recover<S>(message: &str, signature: S) -> Result<PublicKey>
where
    S: AsRef<[u8]>,
{
    let sig_bytes: SigBytes = signature.as_ref().try_into()?;
    let message_hash: [u8; 32] = keccak256(message.as_bytes());
    recover_hash(&message_hash, &sig_bytes)
}

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
mod tests {
    use super::*;
    use hex::FromHex;

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
}
