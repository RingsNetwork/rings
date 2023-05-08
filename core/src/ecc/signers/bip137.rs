//! BIP137 Signer

use sha2::Digest;
use sha2::Sha256;

use crate::ecc::Address;
use crate::ecc::PublicKey;
use crate::err::Result;

/// recover pubkey according to signature.
pub fn recover(msg: &str, sig: impl AsRef<[u8]>) -> Result<PublicKey> {
    let sig_byte: [u8; 65] = sig.as_ref().try_into()?;
    let hash = self::magic_hash(msg);
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

fn varint_buf_num(n: u64) -> Vec<u8> {
    if n < 253 {
        vec![n as u8]
    } else if n < 0x10000 {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[253u8]);
        buf.extend_from_slice(&(n as u16).to_le_bytes());
        buf
    } else if n < 0x100000000 {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[254u8]);
        buf.extend_from_slice(&(n as u32).to_le_bytes());
        buf
    } else {
        let mut buf = vec![255u8, 0, 0, 0, 0, 0, 0, 0, 0];
        buf[1..5].copy_from_slice(&n.to_le_bytes()[..4]);
        buf[5..9].copy_from_slice(&((n >> 32) as u32).to_le_bytes()[..4]);
        buf.truncate(1 + 8);
        buf
    }
}

pub fn magic_hash(msg: &str) -> [u8; 32] {
    let magic_bytes = "Bitcoin Signed Message:\n".as_bytes();
    let msg_bytes = msg.as_bytes();
    let mut buf = Vec::new();
    buf.extend_from_slice(varint_buf_num(magic_bytes.len() as u64).as_slice());
    buf.extend_from_slice(magic_bytes);
    buf.extend_from_slice(varint_buf_num(msg_bytes.len() as u64).as_slice());
    buf.extend_from_slice(msg_bytes);
    let hash = Sha256::digest(Sha256::digest(&buf));
    hash.into()
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
        assert_eq!(pk, pubkey);
    }
}
