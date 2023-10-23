//! Sign and verify message with curve secp256r1 and ECDSA
//! This module support WebCrypto API
//! ref: https://developer.mozilla.org/en-US/docs/Web/API/Web_Crypto_API
//!
//! ```js
//! let keyPair = await window.crypto.subtle.generateKey(
//!   {
//!    name: "ECDSA",
//!    namedCurve: "P-256",
//!   },
//!   true,
//!   ["sign", "verify"],
//! );
//!
//!
//! function getMessageEncoding() {
//!   let message = "hello world";
//!   let enc = new TextEncoder();
//!   return enc.encode(message);
//! }
//!
//! let encoded = getMessageEncoding();
//! let signature = await window.crypto.subtle.sign(
//!   {
//!    name: "ECDSA",
//!    hash: { name: "SHA-256" },
//!   },
//!   keyPair.privateKey,
//!   encoded,
//! );
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
pub fn sign(sec: SecretKey, hash: &[u8; 32]) -> [u8; 64] {
    match (|| -> Result<[u8; 64]> {
        let sk_bytes: FieldBytes<p256::NistP256> = sec.into();
        let sk = ecdsa::SigningKey::<p256::NistP256>::from_bytes(&sk_bytes)?;
        let (sig, _rid) = sk
            .sign_prehash_recoverable(hash)
            .expect("Failed on signing message hash");
        let sig_bytes: [u8; 64] = sig.to_bytes().as_slice().try_into()?;
        Ok(sig_bytes)
    })() {
        Ok(s) => s,
        Err(e) => {
            panic!("Got error on signing msg with secp256r1 {:?}", e)
        }
    }
}

// Hash function with `SHA-256`
pub fn hash(msg: &[u8]) -> [u8; 32] {
    let mut prefix_msg = format!("\x19Rings Signed Message:\n{}", msg.len()).into_bytes();
    prefix_msg.extend_from_slice(msg);
    let hash = Sha256::digest(Sha256::digest(&prefix_msg));
    hash.into()
}

/// Verify message signed by secp256r1
pub fn verify(
    msg: &[u8],
    address: &PublicKeyAddress,
    sig: impl AsRef<[u8]>,
    pubkey: PublicKey,
) -> bool {
    if pubkey.address() != *address {
        return false;
    }
    if sig.as_ref().len() != 64 {
        return false;
    }
    let ct_pk: CtOption<Result<ecdsa::VerifyingKey<p256::NistP256>>> =
        pubkey.ct_try_into_secp256_pubkey();
    let msg_hash = hash(msg);
    if ct_pk.is_some().into() {
        let res: Result<()> = ct_pk.unwrap().and_then(|pk| {
            pk.verify_prehash(
                &msg_hash,
                &ecdsa::Signature::<p256::NistP256>::from_slice(sig.as_ref())
                    .map_err(Error::ECDSAError)?,
            )
            .map_err(Error::ECDSAError)
        });
        if res.is_err() {
            return false;
        }
    }
    true
}
