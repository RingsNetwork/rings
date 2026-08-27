use std::ops::Deref;

use base58_monero as b58m;
use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// Encodes values into the base58-check wire representation.
pub trait Encoder {
    /// Encode this value into an [`Encoded`] wrapper.
    fn encode(&self) -> Result<Encoded>;
}

/// Decodes values from the base58-check wire representation.
pub trait Decoder: Sized {
    /// Decode `Self` from an [`Encoded`] wrapper.
    fn from_encoded(encoded: &Encoded) -> Result<Self>;
}

/// Base58-check encoded message data.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Encoded(String);

impl Encoded {
    /// Borrow the encoded string value.
    pub fn value(&self) -> &String {
        &self.0
    }
}

impl Deref for Encoded {
    type Target = String;
    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

impl Encoder for String {
    fn encode(&self) -> Result<Encoded> {
        Ok(Encoded(
            b58m::encode_check(self.as_bytes()).map_err(|_| Error::Encode)?,
        ))
    }
}

impl Decoder for String {
    fn from_encoded(encoded: &Encoded) -> Result<String> {
        let d = Vec::from_encoded(encoded)?;
        String::from_utf8(d).map_err(|_| Error::Decode)
    }
}

impl Encoder for &str {
    fn encode(&self) -> Result<Encoded> {
        self.as_bytes().encode()
    }
}

impl Encoder for &[u8] {
    fn encode(&self) -> Result<Encoded> {
        Ok(Encoded(
            b58m::encode_check(self).map_err(|_| Error::Encode)?,
        ))
    }
}

impl Encoder for Vec<u8> {
    fn encode(&self) -> Result<Encoded> {
        Ok(Encoded(
            b58m::encode_check(self).map_err(|_| Error::Encode)?,
        ))
    }
}

impl Encoder for Bytes {
    fn encode(&self) -> Result<Encoded> {
        self.as_ref().encode()
    }
}

impl Decoder for Vec<u8> {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        b58m::decode_check(encoded.deref()).map_err(|_| Error::Decode)
    }
}

impl Decoder for Bytes {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let d = Vec::from_encoded(encoded)?;
        Ok(Bytes::from(d))
    }
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for Encoded {
    fn to_string(&self) -> String {
        self.deref().to_owned()
    }
}

impl From<String> for Encoded {
    fn from(v: String) -> Self {
        Self(v)
    }
}

impl From<&str> for Encoded {
    fn from(v: &str) -> Self {
        Self(v.to_owned())
    }
}

impl From<Encoded> for Vec<u8> {
    fn from(a: Encoded) -> Self {
        a.to_string().as_bytes().to_vec()
    }
}

impl TryFrom<Vec<u8>> for Encoded {
    type Error = Error;
    fn try_from(a: Vec<u8>) -> Result<Self> {
        let s: String = String::from_utf8(a)?;
        Ok(s.into())
    }
}

impl Encoded {
    /// Create an [`Encoded`] value from a string that is already encoded.
    pub fn from_encoded_str(str: &str) -> Self {
        Self(str.to_owned())
    }

    /// Decode this value into a target type implementing [`Decoder`].
    pub fn decode<T>(&self) -> Result<T>
    where T: Decoder {
        T::from_encoded(self)
    }
}

#[cfg(test)]
mod test_encoder;
