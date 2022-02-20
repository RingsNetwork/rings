use anyhow::anyhow;
use anyhow::Result;
use base58_monero as b58m;

pub fn encode(s: String) -> Result<String> {
    // Encode a byte vector into a base58-check string, adds 4 bytes checksuM
    b58m::encode_check(s.as_bytes()).map_err(|e| anyhow!(e))
}

pub fn decode(s: String) -> Result<String> {
    // Decode base58-encoded with 4 bytes checksum string into a byte vector

    match b58m::decode_check(&s) {
        Ok(b) => Ok(String::from_utf8(b)?),
        Err(e) => Err(anyhow!(e)),
    }
}
