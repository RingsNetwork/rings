//! Signer for default ECDSA and EIP191.
use web3::signing::keccak256;

use crate::ecc::Address;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::Result;

/// Default method signing using libsecp256k1::SecretKey.
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

/// eip191.
/// ref <https://eips.ethereum.org/EIPS/eip-191>
pub mod eip191 {
    use super::*;

    /// sign function passing raw message parameter.
    pub fn sign_raw(sec: SecretKey, msg: &str) -> [u8; 65] {
        sign(sec, &hash(msg))
    }

    /// sign function with `hash` data.
    pub fn sign(sec: SecretKey, hash: &[u8; 32]) -> [u8; 65] {
        let mut sig = sec.sign_hash(hash);
        sig[64] += 27;
        sig
    }

    /// \x19Ethereum Signed Message\n use for PersionSign, which can encode by send `personalSign` rpc call.
    pub fn hash(msg: &str) -> [u8; 32] {
        let mut prefix_msg = format!("\x19Ethereum Signed Message:\n{}", msg.len()).into_bytes();
        prefix_msg.extend_from_slice(msg.as_bytes());
        keccak256(&prefix_msg)
    }

    /// recover pubkey according to signature.
    pub fn recover(msg: &str, sig: impl AsRef<[u8]>) -> Result<PublicKey> {
        let sig_byte: [u8; 65] = sig.as_ref().try_into()?;
        let hash = hash(msg);
        let mut sig712 = sig_byte;
        sig712[64] -= 27;
        crate::ecc::recover_hash(&hash, &sig712)
    }

    /// verify message signed by Ethereum address.
    pub fn verify(msg: &str, address: &Address, sig: impl AsRef<[u8]>) -> bool {
        if let Ok(p) = recover(msg, sig) {
            p.address() == *address
        } else {
            false
        }
    }
}

/// ed25519 sign algorithm using ed25519_dalek
pub mod ed25519 {
    use ed25519_dalek::Verifier;

    use super::*;

    /// ref <https://www.rfc-editor.org/rfc/rfc8709>
    pub fn verify(msg: &str, address: &Address, sig: impl AsRef<[u8]>, pubkey: PublicKey) -> bool {
        if pubkey.address() != *address {
            return false;
        }
        if sig.as_ref().len() != 64 {
            return false;
        }
        let sig_data: [u8; 64] = sig.as_ref().try_into().unwrap();
        if let (Ok(p), Ok(s)) = (
            TryInto::<ed25519_dalek::PublicKey>::try_into(pubkey),
            ed25519_dalek::Signature::from_bytes(&sig_data),
        ) {
            match p.verify(msg.as_bytes(), &s) {
                Ok(()) => true,
                Err(_) => false,
            }
        } else {
            false
        }
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
    fn test_eip191() {
        use hex::FromHex;
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let address = Address::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

        // window.ethereum.request({method: "personal_sign", params: ["test", "0x11E807fcc88dD319270493fB2e822e388Fe36ab0"]})
        let metamask_sig = Vec::from_hex("724fc31d9272b34d8406e2e3a12a182e72510b008de6cc44684577e31e20d9626fb760d6a0badd79a6cf4cd56b2fc0fbd60c438b809aa7d29bfb598c13e7b50e1b").unwrap();
        let msg = "test";
        let h = eip191::hash(msg);
        let sig = eip191::sign(key, &h);
        assert_eq!(metamask_sig.as_slice(), sig);
        let pubkey = eip191::recover(msg, sig).unwrap();
        assert_eq!(pubkey.address(), address);
        assert!(eip191::verify(msg, &address, sig));
    }

    #[test]
    fn test_verify_ed25519() {
        // test via phantom
        // const msg = "helloworld";
        // const encoded = new TextEncoder().encode(msg);
        // const signedMessage = await solana.request({
        //     method: "signMessage",
        //     params: {
        //     message: encoded,
        //     },
        // });
        // publicKey: "9z1ZTaGocNSAu3DSqGKR6Dqt214X4dXucVd6C53EgqBK"
        // signature: "2V1AR5byk4a4CkVmFRWU1TVs3ns2CGkuq6xgGju1huGQGq5hGkiHUDjEaJJaL2txfqCSGnQW55jUJpcjKFkZEKq"

        let msg = "helloworld";
        let signer =
            PublicKey::try_from_b58t("9z1ZTaGocNSAu3DSqGKR6Dqt214X4dXucVd6C53EgqBK").unwrap();
        let sig_b58 = "2V1AR5byk4a4CkVmFRWU1TVs3ns2CGkuq6xgGju1huGQGq5hGkiHUDjEaJJaL2txfqCSGnQW55jUJpcjKFkZEKq";
        let sig: Vec<u8> = base58::FromBase58::from_base58(sig_b58).unwrap();
        assert!(ed25519::verify(
            msg,
            &signer.address(),
            sig.as_slice(),
            signer
        ))
    }
}
