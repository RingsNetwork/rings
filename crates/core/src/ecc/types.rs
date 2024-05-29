#![warn(missing_docs)]

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// PublicKey for ECDSA and EdDSA.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub struct PublicKey<const SIZE: usize>(pub [u8; SIZE]);

struct PublicKeyVisitor<const SIZE: usize>;
// /// twist from https://docs.rs/libsecp256k1/latest/src/libsecp256k1/lib.rs.html#335-344
impl<const SIZE: usize> Serialize for PublicKey<SIZE> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where S: serde::ser::Serializer {
        serializer.serialize_str(
            &base58_monero::encode_check(&self.0[..]).map_err(serde::ser::Error::custom)?,
        )
    }
}

impl PublicKey<33> {
    /// trezor style b58
    pub fn try_from_b58t(value: &str) -> Result<PublicKey<33>> {
        let value: Vec<u8> =
            base58::FromBase58::from_base58(value).map_err(|_| Error::PublicKeyBadFormat)?;
        Self::from_u8(value.as_slice())
    }

    /// monero and bitcoin style b58
    pub fn try_from_b58m(value: &str) -> Result<PublicKey<33>> {
        let value: &[u8] =
            &base58_monero::decode_check(value).map_err(|_| Error::PublicKeyBadFormat)?;
        Self::from_u8(value)
    }

    /// monero style uncheck base56
    pub fn try_from_b58m_uncheck(value: &str) -> Result<PublicKey<33>> {
        let value: &[u8] = &base58_monero::decode(value).map_err(|_| Error::PublicKeyBadFormat)?;
        Self::from_u8(value)
    }

    /// from raw [u8], the length can be 32, or 33
    /// Odd flag can be "02" (odd), "03" (even) or "00" (unknown)
    /// The format is <odd_flat (1 bytes), x_cordinate (32 bytes)>
    /// For 32 bytes case, we mark the odd flas as 00 (unknown)
    /// For 64 bytes case, we compress the public key into compressed public key
    pub fn from_u8(value: &[u8]) -> Result<PublicKey<33>> {
        let mut s = value.to_vec();
        let data: Vec<u8> = match s.len() {
            32 => {
                s.reverse();
                s.push(0);
                s.reverse();
                Ok(s)
            }
            33 => Ok(s),
            64 => {
                let pk: [u8; 64] = s.try_into().unwrap();
                let mut x: Vec<u8> = pk[..32].to_vec();
                let y: [u8; 32] = pk[32..].try_into().unwrap();
                let y_odd = if y[31] & 1 == 1 { 2u8 } else { 3u8 };
                x.reverse();
                x.push(y_odd);
                x.reverse();
                Ok(x)
            }
            _ => Err(Error::PublicKeyBadFormat),
        }?;
        let pub_data: [u8; 33] = data.as_slice().try_into()?;
        Ok(PublicKey(pub_data))
    }

    /// convert pubkey to base58_string
    pub fn to_base58_string(&self) -> Result<String> {
        Ok(base58::ToBase58::to_base58(&self.0[..]))
    }

    /// convert public_key from hex string
    pub fn from_hex_string(value: &str) -> Result<PublicKey<33>> {
        let v = hex::decode(value)?;
        Self::from_u8(v.as_slice())
    }
}

impl<'de> serde::de::Visitor<'de> for PublicKeyVisitor<33> {
    type Value = PublicKey<33>;

    fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        formatter.write_str("a bytestring of in length 33")
    }
    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where E: serde::de::Error {
        PublicKey::try_from_b58m(value).map_err(|e| E::custom(e))
    }
}

impl<'de> Deserialize<'de> for PublicKey<33> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::de::Deserializer<'de> {
        deserializer.deserialize_str(PublicKeyVisitor)
    }
}
