use anyhow::anyhow;
use anyhow::Result;
use base58_monero as b58m;
use serde::Deserialize;
use serde::Serialize;
use std::convert::TryFrom;
use std::ops::Deref;

fn encode(s: String) -> Result<String> {
    // Encode a byte vector into a base58-check string, adds 4 bytes checksuM
    b58m::encode_check(s.as_bytes()).map_err(|e| anyhow!(e))
}

fn decode(s: String) -> Result<String> {
    // Decode base58-encoded with 4 bytes checksum string into a byte vector

    match b58m::decode_check(&s) {
        Ok(b) => Ok(String::from_utf8(b)?),
        Err(e) => Err(anyhow!(e)),
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Encoded(String);

impl Deref for Encoded {
    type Target = String;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<String> for Encoded {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        Ok(Self(encode(s)?))
    }
}

impl TryFrom<&str> for Encoded {
    type Error = anyhow::Error;
    fn try_from(s: &str) -> Result<Self> {
        Ok(Self(encode(s.to_string())?))
    }
}

impl TryFrom<Encoded> for String {
    type Error = anyhow::Error;
    fn try_from(s: Encoded) -> Result<Self> {
        decode(s.deref().to_owned())
    }
}
