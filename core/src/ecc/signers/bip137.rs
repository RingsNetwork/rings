//! BIP137 Signer

use crate::ecc::Address;
use crate::ecc::PublicKey;
use crate::err::Result;

/// recover pubkey according to signature.
pub fn recover(msg: &str, sig: impl AsRef<[u8]>) -> Result<PublicKey> {
    super::eip191::recover(msg, sig)
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
    use base64::engine::general_purpose;
    use base64::Engine;

    use super::*;

    #[test]
    fn test_verify() {
        // The sig is created by unisat.
        // let sig = await window.unisat.signMessage("hello world");
        // sig = "G33Hk70ylJehC5kFVQGMr9NX0iHr7VF5j28iSeb2fRDSbdwGCwQWk2yHSlfnyEMNttHv7wR1pHU3N+LGN5wJ9k0="
        // signer = "bc1p43s0vyuq6wvsfqmw5vd2thhv5pg755ekasw2y8jwh754ezdayfkqvsrama"
        // public_key = "03ba4a79b52bff401b9043ae4112f397bc96ae73540ca302801837f818d8a8d922"
        //
        let pubkey = PublicKey::from_hex_string(
            "03accfab2be4d4d97d4a5943900bbf66ab602386da3353f12db942cac0705d4206",
        )
        .unwrap();

        let msg = "hello world";
        let sig_b64 = "G4j29m8WutQfZJaonWuXLoXhlhlPfJzbN/Vmz2hdiAYwFMpvTPZslHaOBaotsFfkN26KpaCF3Az0ooUr6vMbNBg=";
        let mut sig: Vec<u8> = general_purpose::STANDARD.decode(sig_b64).unwrap();
        assert_eq!(sig.len(), 65);
        sig.rotate_left(1);
        assert_eq!(sig[64], 27);
        let pk = self::recover(msg, sig).unwrap();
        assert_eq!(pk.address(), pubkey.address());
    }
}
