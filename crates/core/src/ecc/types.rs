#![deny(missing_docs)]

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// PublicKey for ECDSA and EdDSA.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub struct PublicKey<const SIZE: usize>(pub [u8; SIZE]);

struct PublicKeyVisitor<const SIZE: usize>;
// /// twist handling reference for compressed elliptic-curve public keys.
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
            &crate::base58_check::decode(value).map_err(|_| Error::PublicKeyBadFormat)?;
        Self::from_u8(value)
    }

    /// Build a 33-byte key from its 32-, 33- or 64-byte external form.
    ///
    /// ```text
    /// 32 bytes  k        ↦ 0x00 ‖ k              an Ed25519 verifying key
    /// 33 bytes  t ‖ x    ↦ t ‖ x                 already in the 33-byte form
    /// 64 bytes  x ‖ y    ↦ (0x02 | y mod 2) ‖ x  SEC1 §2.3.3 point compression
    /// ```
    ///
    /// The prefix `0x00` is the Ed25519 embedding (`From<ed25519_dalek::VerifyingKey>`), not a
    /// curve point with an unknown y-parity: no SEC1 decoder accepts it. For `x ‖ y` on
    /// secp256k1 or P-256 the result equals the library's `to_sec1_point(true)`, so it decodes to
    /// `(x, y)` in any SEC1 consumer.
    pub fn from_u8(value: &[u8]) -> Result<PublicKey<33>> {
        let (tag, x): (u8, &[u8]) = match value.len() {
            32 => (ED25519_TAG, value),
            33 => value
                .split_first()
                .map(|(tag, x)| (*tag, x))
                .ok_or(Error::PublicKeyBadFormat)?,
            64 => {
                let (x, y) = value
                    .split_at_checked(32)
                    .ok_or(Error::PublicKeyBadFormat)?;
                let y_last = y.last().copied().ok_or(Error::PublicKeyBadFormat)?;
                (sec1_compressed_tag(y_last), x)
            }
            _ => return Err(Error::PublicKeyBadFormat),
        };
        let x: [u8; 32] = x.try_into().map_err(|_| Error::PublicKeyBadFormat)?;
        let mut data = [0u8; 33];
        data.iter_mut()
            .zip(std::iter::once(tag).chain(x))
            .for_each(|(slot, byte)| *slot = byte);
        Ok(PublicKey(data))
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

/// The prefix of the Ed25519 embedding `0x00 ‖ k`: no 33-byte SEC1 encoding starts with it, as
/// the SEC1 identity `0x00` is a single byte.
const ED25519_TAG: u8 = 0x00;

/// The SEC1 §2.3.3 tag of a compressed point whose y-coordinate is even.
const SEC1_TAG_EVEN_Y: u8 = 0x02;

/// The SEC1 §2.3.3 tag of a compressed point whose y-coordinate ends in the byte `y_last`:
/// `0x02` for an even `y`, `0x03` for an odd one (pure).
const fn sec1_compressed_tag(y_last: u8) -> u8 {
    SEC1_TAG_EVEN_Y | (y_last & 1)
}

impl<const SIZE: usize> PublicKey<SIZE> {
    /// Build a public key from exactly `SIZE` bytes.
    pub fn from_exact_u8(value: &[u8]) -> Result<PublicKey<SIZE>> {
        let data: [u8; SIZE] = value.try_into().map_err(|_| Error::PublicKeyBadFormat)?;
        Ok(PublicKey(data))
    }
}

impl<'de, const SIZE: usize> serde::de::Visitor<'de> for PublicKeyVisitor<SIZE> {
    type Value = PublicKey<SIZE>;

    fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(formatter, "a base58-check public key with length {SIZE}")
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where E: serde::de::Error {
        let value =
            crate::base58_check::decode(value).map_err(|_| E::custom(Error::PublicKeyBadFormat))?;
        if SIZE == 33 {
            let key = PublicKey::<33>::from_u8(&value).map_err(E::custom)?;
            return PublicKey::from_exact_u8(&key.0).map_err(E::custom);
        }
        PublicKey::from_exact_u8(&value).map_err(E::custom)
    }
}

impl<'de, const SIZE: usize> Deserialize<'de> for PublicKey<SIZE> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::de::Deserializer<'de> {
        deserializer.deserialize_str(PublicKeyVisitor)
    }
}
