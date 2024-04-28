//! BIP137 Signer

use arrayref::array_mut_ref;
use sha2::Digest;
use sha2::Sha256;

use crate::ecc::PublicKey;
use crate::ecc::PublicKeyAddress;
use crate::error::Result;
use crate::error::Error;

/// recover pubkey according to signature.
/// | y-parity | x-order       | compression | recovery id | v  |
/// |----------|---------------|-------------|-------------|----|
/// | even     | less than n   | false       | 0           | 27 |
/// | odd      | less than n   | false       | 1           | 28 |
/// | even     | more than n   | false       | 2           | 29 |
/// | odd      | more than n   | false       | 3           | 30 |
/// | even     | less than n   | true        | 0           | 31 |
/// | odd      | less than n   | true        | 1           | 32 |
/// | even     | more than n   | true        | 2           | 33 |
/// | odd      | more than n   | true        | 3           | 34 |
pub fn recover(msg: &[u8], sig: impl AsRef<[u8]>) -> Result<PublicKey> {
    let mut sig = sig.as_ref().to_vec();
    sig.rotate_left(1);
    let sig = sig.as_mut_slice();
    let sig_byte = array_mut_ref![sig, 0, 65];
    let hash = self::magic_hash(msg);
    if sig_byte[64] >= 27 && sig_byte[64] <= 31 {
	sig_byte[64] -= 27;
    } else if sig_byte[64] >=32 && sig_byte[64] <= 34 {
	sig_byte[64] -= 31;
    } else {
	return Err(Error::InvalidRecoverId(sig_byte[64]))
    }
    crate::ecc::recover_hash(&hash, sig_byte)
}

/// verify message signed by Ethereum address.
pub fn verify(msg: &[u8], address: &PublicKeyAddress, sig: impl AsRef<[u8]>) -> bool {
    match recover(msg, sig.as_ref()) {
        Ok(recover_pk) => {
            if recover_pk.address() == *address {
                return true;
            }
            tracing::debug!(
                "failed to recover pubkey address, got: {}, expect: {}",
                address,
                recover_pk.address()
            );
            false
        }
        Err(e) => {
            tracing::debug!(
                "failed to recover pubkey: {:?}\nmsg: {:?}\nsig:{:?}",
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

pub fn magic_hash(msg: &[u8]) -> [u8; 32] {
    let magic_bytes = "Bitcoin Signed Message:\n".as_bytes();
    let mut buf = Vec::new();
    buf.extend_from_slice(varint_buf_num(magic_bytes.len() as u64).as_slice());
    buf.extend_from_slice(magic_bytes);
    buf.extend_from_slice(varint_buf_num(msg.len() as u64).as_slice());
    buf.extend_from_slice(msg);
    let hash = Sha256::digest(Sha256::digest(&buf));
    hash.into()
}

#[cfg(test)]
mod test {

    use super::*;

    #[test]
    fn test_verify() {
        let pubkey = PublicKey::from_hex_string(
            "026a626503429a973dc4fcde64fa7932158a20c69b79c9eab1245577dd43674dc5",
        )
        .unwrap();

        let msg = "Hello World 42";
        let sig = vec![
            27, 204, 122, 109, 87, 84, 60, 195, 135, 84, 231, 22, 77, 88, 215, 161, 77, 74, 181,
            192, 19, 219, 188, 251, 142, 104, 2, 233, 132, 82, 171, 102, 125, 114, 45, 23, 202, 59,
            86, 236, 76, 169, 164, 164, 179, 221, 206, 54, 32, 106, 81, 115, 217, 42, 93, 114, 131,
            115, 128, 227, 45, 231, 30, 111, 34,
        ];
        assert_eq!(sig.len(), 65);

        let pk = self::recover(msg.as_bytes(), sig).unwrap();
        assert_eq!(pk, pubkey);
        assert_eq!(pk.address(), pubkey.address());
    }
}
