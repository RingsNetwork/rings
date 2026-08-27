//! Sign and verify message with curve secp256r1 and ECDSA
//! This module support WebCrypto API
//! ref: <https://developer.mozilla.org/en-US/docs/Web/API/Web_Crypto_API>
//! To use this signature, message should be wrapped with prefix
//!
//! ```js
//! function messageWithPrefixToBytes(msg) {
//!     const prefix = "\x19Rings Signed Message:\n" + msg.length;
//!     const encoder = new TextEncoder();
//!
//!     const prefixBytes = encoder.encode(prefix);
//!     const msgBytes = encoder.encode(msg);
//!
//!     const combined = new Uint8Array(prefixBytes.length + msgBytes.length);
//!     combined.set(prefixBytes);
//!     combined.set(msgBytes, prefixBytes.length);
//!
//!     return combined;
//! }
//! ```
//!
//! And you can sign message with API of webcrypto like:
//!
//! ```js
//! let keyPair = await window.crypto.subtle.generateKey(
//! {
//!   name: "ECDSA",
//!   namedCurve: "P-256",
//! },
//!   true,
//!   ["sign", "verify"],
//! );
//!
//! let signature = await window.crypto.subtle.sign(
//!   {
//!    name: "ECDSA",
//!    hash: { name: "SHA-256" },
//!    namedCurve: "P-256"
//!   },
//!   keyPair.privateKey,
//!   encoded,
//! );
//! ```
//!
//! And verify your signature in rings network.

use ecdsa::signature::hazmat::PrehashVerifier;
use elliptic_curve::FieldBytes;
use p256;
use sha2::Digest;
use sha2::Sha256;
use subtle::CtOption;

use crate::ecc::PublicKey;
use crate::ecc::PublicKeyAddress;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// sign function with `hash` data. Recover is no needed.
pub fn sign(sec: SecretKey, hash: &[u8; 32]) -> Result<[u8; 64]> {
    let sk_bytes: FieldBytes<p256::NistP256> = sec.into();
    let sk = ecdsa::SigningKey::<p256::NistP256>::from_bytes(&sk_bytes)?;
    let (sig, _rid) = sk.sign_prehash_recoverable(hash)?;
    let sig_bytes: [u8; 64] = sig.to_bytes().as_slice().try_into()?;
    Ok(sig_bytes)
}

/// Prefix a message with the Rings secp256r1 signing domain string.
pub fn magic_prefix(msg: &[u8]) -> Vec<u8> {
    let mut prefix_msg = format!("\x19Rings Signed Message:\n{}", msg.len()).into_bytes();
    prefix_msg.extend_from_slice(msg);
    prefix_msg.to_vec()
}

/// Hash a Rings-prefixed message with SHA-256.
pub fn hash(msg: &[u8]) -> [u8; 32] {
    let prefix_msg = magic_prefix(msg);
    let hash = Sha256::digest(prefix_msg);
    hash.into()
}

/// Verify message signed by secp256r1
pub fn verify(
    msg: &[u8],
    address: &PublicKeyAddress,
    sig: impl AsRef<[u8]>,
    pubkey: &PublicKey<33>,
) -> bool {
    if pubkey.address() != *address {
        return false;
    }
    if sig.as_ref().len() != 64 {
        return false;
    }
    let ct_pk: CtOption<Result<ecdsa::VerifyingKey<p256::NistP256>>> =
        (*pubkey).ct_try_into_secp256r1_pubkey();
    if !bool::from(ct_pk.is_some()) {
        return false;
    }
    let msg_hash = hash(msg);
    let res: Result<()> = ct_pk.unwrap().and_then(|pk| {
        pk.verify_prehash(
            &msg_hash,
            &ecdsa::Signature::<p256::NistP256>::from_slice(sig.as_ref())
                .map_err(Error::ECDSAError)?,
        )
        .map_err(Error::ECDSAError)
    });
    res.is_ok()
}

#[cfg(test)]
mod test_secp256r1;
