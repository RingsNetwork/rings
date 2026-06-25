//! ed25519 sign algorithm using ed25519_dalek
use ed25519_dalek::Signer;
use ed25519_dalek::Verifier;

use crate::ecc::PublicKey;
use crate::ecc::PublicKeyAddress;
use crate::error::Result;

/// Derive an Ed25519 public key from a 32-byte seed.
pub fn public_key(seed: &[u8; 32]) -> Result<PublicKey<33>> {
    let secret = ed25519_dalek::SigningKey::from_bytes(seed);
    let public = ed25519_dalek::VerifyingKey::from(&secret);
    Ok(public.into())
}

/// Sign raw message bytes with an Ed25519 seed.
pub fn sign(seed: &[u8; 32], msg: &[u8]) -> Result<[u8; 64]> {
    let secret = ed25519_dalek::SigningKey::from_bytes(seed);
    Ok(secret.sign(msg).to_bytes())
}

/// ref <https://www.rfc-editor.org/rfc/rfc8709>
pub fn verify(
    msg: &[u8],
    address: &PublicKeyAddress,
    sig: impl AsRef<[u8]>,
    pubkey: &PublicKey<33>,
) -> bool {
    if pubkey.address() != *address {
        return false;
    }
    if sig.as_ref().len() != 64 {
        return false;
    }
    let sig_data: [u8; 64] = sig.as_ref().try_into().unwrap();
    if let Ok(p) = TryInto::<ed25519_dalek::VerifyingKey>::try_into(*pubkey) {
        let s = ed25519_dalek::Signature::from_bytes(&sig_data);
        match p.verify(msg, &s) {
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
        assert!(self::verify(
            msg.as_bytes(),
            &signer.address(),
            sig.as_slice(),
            &signer
        ))
    }
}
