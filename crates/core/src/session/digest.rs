use serde::Deserialize;
use serde::Serialize;

use super::Session;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;

/// Width of a [`SessionDigest`]: the address width of the overlay, so a reference to a session
/// costs what a DID costs.
const SESSION_DIGEST_BYTES: usize = 20;
/// Width of the keccak256 output a [`SessionDigest`] is cut from.
const KECCAK256_BYTES: usize = 32;
/// Where the digest starts inside the hash: the trailing bytes, as an account address is cut.
const SESSION_DIGEST_OFFSET: usize = KECCAK256_BYTES - SESSION_DIGEST_BYTES;

/// Content address of one exact [`Session`] value: the trailing `SESSION_DIGEST_BYTES` of
/// `keccak256(encode(session))`, where `encode` is the canonical Rings wire encoding.
///
/// ```text
///   digest : Session → SessionDigest          digest(s) = digest(s') ⟹ s = s'   (up to keccak)
/// ```
///
/// The address names the whole delegation `(session_id, account, ts_ms, ttl_ms, sig)`, not the
/// session key. A session key can carry more than one delegation (an account may re-delegate it
/// under a new lifetime, and any account can sign a delegation for a key it does not hold), so
/// `session_id` alone does not determine the account a message is attributed to; the content
/// address does. This is what lets a reference stand in for the inline value without changing
/// what is verified, attributed, or digested.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SessionDigest([u8; SESSION_DIGEST_BYTES]);

impl SessionDigest {
    /// The canonical digest bytes.
    pub const fn into_bytes(self) -> [u8; SESSION_DIGEST_BYTES] {
        self.0
    }
}

impl Session {
    /// The content address of this exact delegation.
    ///
    /// Law: `s = s' ⟹ s.digest() = s'.digest()`, because the wire encoding is a function of the
    /// value. The only failure is the encoder's.
    pub fn digest(&self) -> Result<SessionDigest> {
        let encoded = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        let hash: [u8; KECCAK256_BYTES] = keccak256(encoded.as_slice());
        let mut address = [0u8; SESSION_DIGEST_BYTES];
        let trailing = hash.iter().skip(SESSION_DIGEST_OFFSET).copied();
        for (slot, byte) in address.iter_mut().zip(trailing) {
            *slot = byte;
        }
        Ok(SessionDigest(address))
    }
}
