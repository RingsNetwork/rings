//! BIP137 Signer

use arrayref::array_mut_ref;
use sha2::Digest;
use sha2::Sha256;

use crate::ecc::Address;
use crate::ecc::PublicKey;
use crate::err::Result;

/// recover pubkey according to signature.
pub fn recover(msg: &str, sig: impl AsRef<[u8]>) -> Result<PublicKey> {
    let mut sig = sig.as_ref().to_vec();
    sig.rotate_left(1);
    let sig = sig.as_mut_slice();
    let sig_byte = array_mut_ref![sig, 0, 65];
    let hash = self::magic_hash(msg);
    sig_byte[64] -= 27;
    crate::ecc::recover_hash(&hash, sig_byte)
}

/// verify message signed by Ethereum address.
pub fn verify(msg: &str, address: &Address, sig: impl AsRef<[u8]>) -> bool {
    match recover(msg, sig.as_ref()) {
        Ok(recover_pk) => {
            if recover_pk.address() == *address {
                return true;
            }
            tracing::debug!(
                "failed to recover pubkey address, got: {}, expect: {}",
                recover_pk.address(),
                address
            );
            false
        }
        Err(e) => {
            tracing::debug!(
                "failed to recover pubkey: {:?}\nmsg: {}\nsig:{:?}",
                e,
                msg,
                sig.as_ref(),
            );
            false
        }
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

    use super::*;
    use crate::session::AuthorizedInfo;

    #[test]
    fn test_verify() {
        // The sig is created by unisat.
        // public_key = "02c0eeef8d136b10b862a0ac979eac2ad036f9902d87963ddf0fa108f1e275b9c7"
        // msg = r#"{"authorizer":{"did":"0xfada88633e01d2f6704a7f2a6ebc57263aca6978","pubkey":null},"signer":"BIP137","ttl_ms":{"Some":2592000000},"ts_ms":1683616225109,"session_id":"0xc5a72f5031c99d6553a6cc0e4c1219b8cbfdc127"}"#
        // sig = "G96t3+4oboSvj8DQFAGllxKUGcutek25uhLe9xXJY8BSVlGlPAaBEYWGh+/nU7bCVO5Bxtzy1A9gUhRpwYr50SE=";
        // let sig = await window.unisat.signMessage(msg);

        let pubkey = PublicKey::from_hex_string(
            "02c0eeef8d136b10b862a0ac979eac2ad036f9902d87963ddf0fa108f1e275b9c7",
        )
        .unwrap();

        let msg = r#"{"authorizer":{"did":"0xfada88633e01d2f6704a7f2a6ebc57263aca6978","pubkey":null},"signer":"BIP137","ttl_ms":{"Some":2592000000},"ts_ms":1683616225109,"session_id":"0xc5a72f5031c99d6553a6cc0e4c1219b8cbfdc127"}"#;
        let auth: AuthorizedInfo = serde_json::from_str(msg).unwrap();
        let sig = vec![
            27, 222, 173, 223, 238, 40, 110, 132, 175, 143, 192, 208, 20, 1, 165, 151, 18, 148, 25,
            203, 173, 122, 77, 185, 186, 18, 222, 247, 21, 201, 99, 192, 82, 86, 81, 165, 60, 6,
            129, 17, 133, 134, 135, 239, 231, 83, 182, 194, 84, 238, 65, 198, 220, 242, 212, 15,
            96, 82, 20, 105, 193, 138, 249, 209, 33,
        ];
        assert_eq!(sig.len(), 65);
        // assert_eq!(sig[64], 28);
        let pk = self::recover(auth.to_string().unwrap().as_str(), sig).unwrap();
        assert_eq!(pk, pubkey);
        assert_eq!(pk.address(), pubkey.address());
    }
}
