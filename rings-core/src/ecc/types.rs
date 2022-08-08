#![warn(missing_docs)]
use serde::Deserialize;
use serde::Serialize;

use crate::err::Error;
use crate::err::Result;

/// PublicKey for ECDSA and EdDSA
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub struct PublicKey(pub [u8; 33]);

struct PublicKeyVisitor;
// /// twist from https://docs.rs/libsecp256k1/latest/src/libsecp256k1/lib.rs.html#335-344
impl Serialize for PublicKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where S: serde::ser::Serializer {
        serializer.serialize_str(
            &base58_monero::encode_check(&self.0[..]).map_err(serde::ser::Error::custom)?,
        )
    }
}

impl PublicKey {
    /// trezor style b58
    pub fn try_from_b58t(value: &str) -> Result<PublicKey> {
        let value: Vec<u8> =
            base58::FromBase58::from_base58(value).map_err(|_| Error::PublicKeyBadFormat)?;
        Self::from_u8(value.as_slice())
    }

    /// monero and bitcoin style b58
    pub fn try_from_b58m(value: &str) -> Result<PublicKey> {
        let value: &[u8] =
            &base58_monero::decode_check(value).map_err(|_| Error::PublicKeyBadFormat)?;
        Self::from_u8(value)
    }

    /// monero style uncheck base56
    pub fn try_from_b58m_uncheck(value: &str) -> Result<PublicKey> {
        let value: &[u8] = &base58_monero::decode(value).map_err(|_| Error::PublicKeyBadFormat)?;
        Self::from_u8(value)
    }

    /// from raw [u8], the length can be 32, or 33
    pub fn from_u8(value: &[u8]) -> Result<PublicKey> {
        let mut s = value.to_vec();
        let data: Vec<u8> = match s.len() {
            32 => {
                s.reverse();
                s.push(0);
                s.reverse();
                Ok(s)
            }
            33 => Ok(s),
            _ => Err(Error::PublicKeyBadFormat),
        }?;
        let pub_data: [u8; 33] = data.as_slice().try_into()?;
        Ok(PublicKey(pub_data))
    }
}

impl<'de> serde::de::Visitor<'de> for PublicKeyVisitor {
    type Value = PublicKey;

    fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        formatter.write_str("a bytestring of in length 33")
    }
    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where E: serde::de::Error {
        PublicKey::try_from_b58m(value).map_err(|e| E::custom(e))
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::de::Deserializer<'de> {
        deserializer.deserialize_str(PublicKeyVisitor)
    }
}
