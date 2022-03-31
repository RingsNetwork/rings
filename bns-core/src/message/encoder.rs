use base58_monero as b58m;
use serde::Deserialize;
use serde::Serialize;
use std::convert::TryFrom;
use std::ops::Deref;

use crate::err::Error;
use crate::err::Result;

fn encode(s: String) -> Result<String> {
    // Encode a byte vector into a base58-check string, adds 4 bytes checksuM
    b58m::encode_check(s.as_bytes()).map_err(|_| Error::Encode)
}

fn decode(s: String) -> Result<String> {
    // Decode base58-encoded with 4 bytes checksum string into a byte vector

    match b58m::decode_check(&s) {
        Ok(b) => Ok(String::from_utf8(b)?),
        Err(_) => Err(Error::Decode),
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Encoded(String);

impl Deref for Encoded {
    type Target = String;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<String> for Encoded {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        Ok(Self(encode(s)?))
    }
}

impl TryFrom<&str> for Encoded {
    type Error = Error;
    fn try_from(s: &str) -> Result<Self> {
        Ok(Self(encode(s.to_string())?))
    }
}

impl TryFrom<Encoded> for String {
    type Error = Error;
    fn try_from(s: Encoded) -> Result<Self> {
        decode(s.deref().to_owned())
    }
}

impl ToString for Encoded {
    fn to_string(&self) -> String {
        self.deref().to_owned()
    }
}

impl Encoded {
    pub fn from_encoded_str(str: &str) -> Self {
        Self(str.to_owned())
    }
}
