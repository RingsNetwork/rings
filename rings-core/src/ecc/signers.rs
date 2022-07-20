//! Signer for default ECDSA and EIP712
use web3::signing::keccak256;

use crate::ecc::Address;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::Result;

pub mod default {

    use super::*;
    pub fn sign_raw(sec: SecretKey, msg: &str) -> [u8; 65] {
        sign(sec, &hash(msg))
    }

    pub fn sign(sec: SecretKey, hash: &[u8; 32]) -> [u8; 65] {
        sec.sign_hash(hash)
    }
    pub fn hash(msg: &str) -> [u8; 32] {
        keccak256(msg.as_bytes())
    }
    pub fn recover(msg: &str, sig: impl AsRef<[u8]>) -> Result<PublicKey> {
        let sig_byte: [u8; 65] = sig.as_ref().try_into()?;
        crate::ecc::recover(msg, sig_byte)
    }

    pub fn verify(msg: &str, address: &Address, sig: impl AsRef<[u8]>) -> bool {
        if let Ok(p) = recover(msg, sig) {
            p.address() == *address
        } else {
            false
        }
    }
}

pub mod eip712 {
    use super::*;

    pub fn sign_raw(sec: SecretKey, msg: &str) -> [u8; 65] {
        sign(sec, &hash(msg))
    }

    pub fn sign(sec: SecretKey, hash: &[u8; 32]) -> [u8; 65] {
        let mut sig = sec.sign_hash(hash);
        sig[64] += 27;
        sig
    }
    pub fn hash(msg: &str) -> [u8; 32] {
        let mut prefix_msg = format!("\x19Ethereum Signed Message:\n{}", msg.len()).into_bytes();
        prefix_msg.extend_from_slice(msg.as_bytes());
        keccak256(&prefix_msg)
    }

    pub fn recover(msg: &str, sig: impl AsRef<[u8]>) -> Result<PublicKey> {
        let sig_byte: [u8; 65] = sig.as_ref().try_into()?;
        let hash = hash(msg);
        let mut sig712 = sig_byte;
        sig712[64] -= 27;
        crate::ecc::recover_hash(&hash, &sig712)
    }

    pub fn verify(msg: &str, address: &Address, sig: impl AsRef<[u8]>) -> bool {
        if let Ok(p) = recover(msg, sig) {
            p.address() == *address
        } else {
            false
        }
    }
}

pub mod ed25519 {
    use super::*;

    /// will use 512-bit hash here
    pub fn sign_raw(sec: SecretKey, msg: &str) -> [u8; 65] {
        let key: ed25519_dalek::SecretKey = sec.into();
        let pubkey: ed25519_dalek::PublicKey = (&key).into();
        let ext_seckey: ed25519_dalek::ExpandedSecretKey = (&key).into();
        // always [u8;64] here
        let mut sig = ext_seckey.sign(msg.as_bytes(), &pubkey).to_bytes().to_vec();
        // push zero to tail
        sig.push(0);
        sig.as_slice().try_into().unwrap()
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use super::*;
    use crate::ecc::Address;
    use crate::ecc::SecretKey;

    #[test]
    fn test_default_sign() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();

        let msg = "hello";
        let h = default::hash(msg);
        let sig = default::sign(key, &h);
        assert_eq!(sig, key.sign(msg));
    }

    #[test]
    fn test_eip712_sign() {
        use hex::FromHex;
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let address = Address::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

        // window.ethereum.request({method: "personal_sign", params: ["test", "0x11E807fcc88dD319270493fB2e822e388Fe36ab0"]})
        let metamask_sig = Vec::from_hex("724fc31d9272b34d8406e2e3a12a182e72510b008de6cc44684577e31e20d9626fb760d6a0badd79a6cf4cd56b2fc0fbd60c438b809aa7d29bfb598c13e7b50e1b").unwrap();
        let msg = "test";
        let h = eip712::hash(msg);
        let sig = eip712::sign(key, &h);
        assert_eq!(metamask_sig.as_slice(), sig);
        let pubkey = eip712::recover(msg, &sig).unwrap();
        assert_eq!(pubkey.address(), address);
        assert!(eip712::verify(msg, &address, &sig));
    }
}
