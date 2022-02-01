use anyhow::Result;
use base64;
use hex;

pub fn encode(s: String) -> String {
    hex::encode(base64::encode(s))
}

pub fn decode(s: String) -> Result<String> {
    Ok(String::from_utf8(base64::decode(&hex::decode(s)?)?)?)
}
