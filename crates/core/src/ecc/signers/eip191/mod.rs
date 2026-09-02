//! eip191.
//! ref <https://eips.ethereum.org/EIPS/eip-191>

use crate::ecc::keccak256;
use crate::ecc::PublicKey;
use crate::ecc::PublicKeyAddress;
use crate::ecc::SecretKey;
use crate::error::Result;

/// sign function passing raw message parameter.
pub fn sign_raw(sec: &SecretKey, msg: &[u8]) -> Result<[u8; 65]> {
    sign(sec, &hash(msg))
}

/// sign function with `hash` data.
pub fn sign(sec: &SecretKey, hash: &[u8; 32]) -> Result<[u8; 65]> {
    let mut sig = sec.sign_hash(hash)?;
    sig[64] += 27;
    Ok(sig)
}

/// \x19Ethereum Signed Message\n is used for PersonalSign, which can encode by send `personalSign` rpc call.
pub fn hash(msg: &[u8]) -> [u8; 32] {
    let mut prefix_msg = format!("\x19Ethereum Signed Message:\n{}", msg.len()).into_bytes();
    prefix_msg.extend_from_slice(msg);
    keccak256(&prefix_msg)
}

/// recover pubkey according to signature.
pub fn recover(msg: &[u8], sig: impl AsRef<[u8]>) -> Result<PublicKey<33>> {
    let sig_byte: [u8; 65] = sig.as_ref().try_into()?;
    let hash = hash(msg);
    let mut sig712 = sig_byte;
    sig712[64] = super::recovery_id_from_v(sig712[64], 27)?;
    crate::ecc::recover_hash(&hash, &sig712)
}

/// verify message signed by Ethereum address.
pub fn verify(msg: &[u8], address: &PublicKeyAddress, sig: impl AsRef<[u8]>) -> bool {
    if let Ok(p) = recover(msg, sig) {
        p.address() == *address
    } else {
        false
    }
}

#[cfg(test)]
mod test_eip191;
