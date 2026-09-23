use serde::Deserialize;
use serde::Serialize;

use super::Delegation;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;

/// Width of a [`DelegationDigest`]: the address width of the overlay, so a delegation reference
/// costs what a DID costs.
const DELEGATION_DIGEST_BYTES: usize = 20;
/// Width of the keccak256 output a [`DelegationDigest`] is cut from.
const KECCAK256_BYTES: usize = 32;

/// Content address of one exact [`Delegation`] value: the trailing `DELEGATION_DIGEST_BYTES` of
/// `keccak256(encode(delegation))`, where `encode` is the canonical Rings wire encoding.
///
/// ```text
///   digest : Delegation → DelegationDigest          digest(s) = digest(s') ⟹ s = s'   (up to keccak)
/// ```
///
/// The address names the whole delegation
/// `(delegatee_did, delegator, ts_ms, ttl_ms, delegator_signature)`, not the delegatee key. A
/// delegatee key can carry more than one delegation (a delegator may authorize it for a new
/// lifetime, and a delegator can sign a delegation for a key it does not hold), so
/// `delegatee_did` alone does not determine which delegator a message is attributed to; the content
/// address does. This is what lets a reference stand in for the inline value without changing
/// what is verified, attributed, or digested.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DelegationDigest([u8; DELEGATION_DIGEST_BYTES]);

impl DelegationDigest {
    /// The canonical digest bytes.
    pub const fn into_bytes(self) -> [u8; DELEGATION_DIGEST_BYTES] {
        self.0
    }
}

impl Delegation {
    /// The content address of this exact delegation.
    ///
    /// Law: `s = s' ⟹ s.digest() = s'.digest()`, because the wire encoding is a function of the
    /// value. The only failure is the encoder's.
    pub fn digest(&self) -> Result<DelegationDigest> {
        let encoded = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        let hash: [u8; KECCAK256_BYTES] = keccak256(encoded.as_slice());
        let [_, _, _, _, _, _, _, _, _, _, _, _, address @ ..] = hash;
        Ok(DelegationDigest(address))
    }
}
