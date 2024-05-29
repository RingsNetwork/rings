//! Default method signing using libsecp256k1::SecretKey.

use crate::ecc::keccak256;
use crate::ecc::PublicKey;
use crate::ecc::PublicKeyAddress;
use crate::ecc::SecretKey;
use crate::error::Result;

/// sign function passing raw message parameter.
pub fn sign_raw(sec: SecretKey, msg: &[u8]) -> [u8; 65] {
    sign(sec, &hash(msg))
}

/// sign function with `hash` data.
pub fn sign(sec: SecretKey, hash: &[u8; 32]) -> [u8; 65] {
    sec.sign_hash(hash)
}

/// generate hash data from message.
pub fn hash(msg: &[u8]) -> [u8; 32] {
    keccak256(msg)
}

/// recover public key from message and signature.
pub fn recover(msg: &[u8], sig: impl AsRef<[u8]>) -> Result<PublicKey<33>> {
    let sig_byte: [u8; 65] = sig.as_ref().try_into()?;
    crate::ecc::recover(msg, sig_byte)
}

/// verify signature with message and address.
pub fn verify(msg: &[u8], address: &PublicKeyAddress, sig: impl AsRef<[u8]>) -> bool {
    if let Ok(p) = recover(msg, sig) {
        p.address() == *address
    } else {
        false
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ecc::SecretKey;

    #[test]
    fn test_default_sign() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();

        let msg = "hello";
        let h = self::hash(msg.as_bytes());
        let sig = self::sign(key, &h);
        assert_eq!(sig, key.sign(msg));
    }
}
