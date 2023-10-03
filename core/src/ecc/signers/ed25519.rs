//! ed25519 sign algorithm using ed25519_dalek
use ed25519_dalek::Verifier;

use crate::ecc::PublicKey;
use crate::ecc::H160;

/// ref <https://www.rfc-editor.org/rfc/rfc8709>
pub fn verify(msg: &str, address: &H160, sig: impl AsRef<[u8]>, pubkey: PublicKey) -> bool {
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

#[cfg(test)]
mod test {
    use super::*;

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
        assert!(self::verify(msg, &signer.address(), sig.as_slice(), signer))
    }
}
