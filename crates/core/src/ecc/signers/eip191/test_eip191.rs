use std::str::FromStr;

use super::*;
use crate::ecc::SecretKey;

#[test]
fn test_eip191() {
    use hex::FromHex;
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let address = PublicKeyAddress::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

    // window.ethereum.request({method: "personal_sign", params: ["test", "0x11E807fcc88dD319270493fB2e822e388Fe36ab0"]})
    let metamask_sig = Vec::from_hex("724fc31d9272b34d8406e2e3a12a182e72510b008de6cc44684577e31e20d9626fb760d6a0badd79a6cf4cd56b2fc0fbd60c438b809aa7d29bfb598c13e7b50e1b").unwrap();
    let msg = "test";
    let h = self::hash(msg.as_bytes());
    let sig = self::sign(&key, &h).unwrap();
    assert_eq!(metamask_sig.as_slice(), sig);
    let pubkey = self::recover(msg.as_bytes(), sig).unwrap();
    assert_eq!(pubkey.address(), address);
    assert!(self::verify(msg.as_bytes(), &address, sig));
}

#[test]
fn recover_rejects_out_of_range_recovery_id_without_panicking() {
    for recovery_id in [0, 26, 31, u8::MAX] {
        let mut signature = [0u8; 65];
        signature[64] = recovery_id;
        assert!(matches!(
            recover(b"message", signature),
            Err(crate::error::Error::InvalidRecoverId(actual)) if actual == recovery_id
        ));
    }
}
