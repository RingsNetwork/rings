//! eip191.
//! ref <https://eips.ethereum.org/EIPS/eip-191>

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
    let mut sig = sec.sign_hash(hash);
    sig[64] += 27;
    sig
}

/// \x19Ethereum Signed Message\n is used for PersonalSign, which can encode by send `personalSign` rpc call.
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

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use super::*;
    use crate::ecc::Address;
    use crate::ecc::SecretKey;

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
        let h = self::hash(msg);
        let sig = self::sign(key, &h);
        assert_eq!(metamask_sig.as_slice(), sig);
        let pubkey = self::recover(msg, sig).unwrap();
        assert_eq!(pubkey.address(), address);
        assert!(self::verify(msg, &address, sig));
    }
}
