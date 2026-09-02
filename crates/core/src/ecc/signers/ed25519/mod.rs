//! ed25519 sign algorithm using ed25519_dalek
use ed25519_dalek::Signer;

use crate::ecc::PublicKey;
use crate::ecc::PublicKeyAddress;
use crate::error::Result;

/// Derive an Ed25519 public key from a 32-byte seed.
pub fn public_key(seed: &[u8; 32]) -> Result<PublicKey<33>> {
    let secret = ed25519_dalek::SigningKey::from_bytes(seed);
    let public = ed25519_dalek::VerifyingKey::from(&secret);
    Ok(public.into())
}

/// Sign raw message bytes with an Ed25519 seed.
pub fn sign(seed: &[u8; 32], msg: &[u8]) -> Result<[u8; 64]> {
    let secret = ed25519_dalek::SigningKey::from_bytes(seed);
    Ok(secret.sign(msg).to_bytes())
}

/// ref <https://www.rfc-editor.org/rfc/rfc8709>
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
    let sig_data = match <[u8; 64]>::try_from(sig.as_ref()) {
        Ok(sig_data) => sig_data,
        Err(_) => return false,
    };
    if let Ok(p) = TryInto::<ed25519_dalek::VerifyingKey>::try_into(*pubkey) {
        let s = ed25519_dalek::Signature::from_bytes(&sig_data);
        match p.verify_strict(msg, &s) {
            Ok(()) => true,
            Err(_) => false,
        }
    } else {
        false
    }
}

#[cfg(test)]
mod test_ed25519;
