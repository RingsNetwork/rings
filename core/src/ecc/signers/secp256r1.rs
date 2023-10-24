//! Sign and verify message with curve secp256r1 and ECDSA
//! This module support WebCrypto API
//! ref: https://developer.mozilla.org/en-US/docs/Web/API/Web_Crypto_API
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

// Prefix with magic string
pub fn magic_prefix(msg: &[u8]) -> Vec<u8> {
    let mut prefix_msg = format!("\x19Rings Signed Message:\n{}", msg.len()).into_bytes();
    prefix_msg.extend_from_slice(msg);
    prefix_msg.to_vec()
}

// Hash function with `SHA-256`
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::ecc::SecretKey;

    /// The step to get pk and sk from js console:
    /// help functions like byteArrayToHex, concatUint8Arrays and BaseUrlToUint8Array are note included.:
    ///
    /// ```js
    /// let keyPair = await window.crypto.subtle.generateKey(
    /// {
    ///   name: "ECDSA",
    ///   namedCurve: "P-256",
    /// },
    ///   true,
    ///   ["sign", "verify"],
    /// );
    ///
    /// jwk = await window.crypto.subtle.exportKey("jwk", keyPair.privateKey);
    /// let sk = byteArrayToHex(base64UrlToUint8Array(jwk.d))
    /// >>> "2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038"
    ///
    /// let pk =  byteArrayToHex(concatUint8Arrays(base64UrlToUint8Array(jwk.x), base64UrlToUint8Array(jwk.y)))
    /// >>> "17a6afd392fcbe4ac9270a599a9c5732c4f838ce35ea2234d389d8f0c367f3f5dcab906352e27289002c7f2c96039ddce7c1b5aad8b87ba94984d4c8b4f95702"
    ///
    /// function messageWithPrefixToBytes(msg) {
    ///     const prefix = "\x19Rings Signed Message:\n" + msg.length;
    ///     const encoder = new TextEncoder();
    ///
    ///     const prefixBytes = encoder.encode(prefix);
    ///     const msgBytes = encoder.encode(msg);
    ///
    ///     const combined = new Uint8Array(prefixBytes.length + msgBytes.length);
    ///     combined.set(prefixBytes);
    ///     combined.set(msgBytes, prefixBytes.length);
    ///
    ///     return combined;
    /// }
    ///
    /// let encoded = messageToBytes("hello world")
    ///
    /// let signature = await window.crypto.subtle.sign(
    ///   {
    ///    name: "ECDSA",
    ///    hash: { name: "SHA-256" },
    ///    namedCurve: "P-256"
    ///   },
    ///   keyPair.privateKey,
    ///   encoded,
    /// );
    /// byteArrayToHex(new Uint8Array(signature))
    /// >>> 43e9f1ce3f4fc0761805cb13b3ec188ccd3d509b7e563f3794e5daf84eaf43bf4fe1343f0b08a810768475fa87fd061a586e943ca9665ee167a3f63c70c72fd9
    /// ```
    #[test]
    fn test_secp256r1_sign_and_verify() {
        let pk: PublicKey = PublicKey::from_hex_string(
	    "17a6afd392fcbe4ac9270a599a9c5732c4f838ce35ea2234d389d8f0c367f3f5dcab906352e27289002c7f2c96039ddce7c1b5aad8b87ba94984d4c8b4f95702"
	).unwrap();
        let sk =
            SecretKey::try_from("2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038")
                .unwrap();
        // Check msg encode
        let msg = "hello world";
        let prefix_msg = magic_prefix(msg.as_bytes());
        let js_msg =
            hex::decode("1952696e6773205369676e6564204d6573736167653a0a313168656c6c6f20776f726c64")
                .unwrap()
                .to_vec();
        assert_eq!(prefix_msg, js_msg, "encoded msg not equal");
        let sig: [u8; 64] = hex::decode("43e9f1ce3f4fc0761805cb13b3ec188ccd3d509b7e563f3794e5daf84eaf43bf4fe1343f0b08a810768475fa87fd061a586e943ca9665ee167a3f63c70c72fd9").unwrap().try_into().unwrap();

        // Check our sign and verify work right
        let our_sig = sign(sk, &hash(msg.as_bytes()));
        assert!(verify(msg.as_bytes(), &pk.address(), our_sig, pk));

        let hash_msg: [u8; 32] =
            hex::decode("5e230abb2ae1cb0717986854d6e16b998da03b827b736c9ac32f6ec9e47e3670")
                .unwrap()
                .try_into()
                .unwrap();
        let hashed = hash(msg.as_bytes());
        assert_eq!(hashed, hash_msg, "hash ret not equal");

        assert!(verify(msg.as_bytes(), &pk.address(), sig, pk));
    }
}
