//! Default method signing using libsecp256k1::SecretKey.
use web3::signing::keccak256;

use crate::ecc::Address;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::Result;

/// sign function passing raw message parameter.
pub fn sign_raw(sec: SecretKey, msg: &str) -> [u8; 65] {
    sign(sec, &hash(msg))
}

/// sign function with `hash` data.
pub fn sign(sec: SecretKey, hash: &[u8; 32]) -> [u8; 65] {
    sec.sign_hash(hash)
}

/// generate hash data from message.
pub fn hash(msg: &str) -> [u8; 32] {
    keccak256(msg.as_bytes())
}

/// recover public key from message and signature.
pub fn recover(msg: &str, sig: impl AsRef<[u8]>) -> Result<PublicKey> {
    let sig_byte: [u8; 65] = sig.as_ref().try_into()?;
    crate::ecc::recover(msg, sig_byte)
}

/// verify signature with message and address.
pub fn verify(msg: &str, address: &Address, sig: impl AsRef<[u8]>) -> bool {
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
        let h = self::hash(msg);
        let sig = self::sign(key, &h);
        assert_eq!(sig, key.sign(msg));
    }
}
