use anyhow::anyhow;
use anyhow::Result;
use hex;
use libsecp256k1::recover;
use rand::SeedableRng;
use rand_hc::Hc128Rng;
use std::convert::TryFrom;
use std::ops::Deref;
use web3::signing::keccak256;
use web3::types::Address;

// ref https://docs.rs/web3/0.18.0/src/web3/signing.rs.html#69

// length r: 32, length s: 32, length v(recovery_id): 1
pub type SigBytes = [u8; 65];

#[derive(Copy, Clone, Debug)]
pub struct SecretKey(libsecp256k1::SecretKey);
#[derive(Copy, Clone, Debug)]
pub struct PublicKey(libsecp256k1::PublicKey);

impl Deref for SecretKey {
    type Target = libsecp256k1::SecretKey;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Deref for PublicKey {
    type Target = libsecp256k1::PublicKey;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<libsecp256k1::SecretKey> for SecretKey {
    fn from(key: libsecp256k1::SecretKey) -> Self {
        Self(key)
    }
}

impl From<libsecp256k1::PublicKey> for PublicKey {
    fn from(key: libsecp256k1::PublicKey) -> Self {
        Self(key)
    }
}

impl TryFrom<&str> for SecretKey {
    type Error = anyhow::Error;
    fn try_from(s: &str) -> Result<Self> {
        let key = hex::decode(s)?;
        let key_arr: [u8; 32] = key.as_slice().try_into()?;
        match libsecp256k1::SecretKey::parse(&key_arr) {
            Ok(key) => Ok(key.into()),
            Err(e) => Err(anyhow!(e)),
        }
    }
}

fn public_key_address(public_key: &PublicKey) -> Address {
    let public_key = public_key.serialize();
    debug_assert_eq!(public_key[0], 0x04);
    let hash = keccak256(&public_key[1..]);
    Address::from_slice(&hash[12..])
}

fn secret_key_address(secret_key: &SecretKey) -> Address {
    let public_key = libsecp256k1::PublicKey::from_secret_key(secret_key);
    public_key_address(&public_key.into())
}

impl SecretKey {
    pub fn random() -> Self {
        let mut rng = Hc128Rng::from_entropy();
        Self(libsecp256k1::SecretKey::random(&mut rng))
    }

    pub fn address(&self) -> Address {
        secret_key_address(self)
    }
    pub fn sign(&self, msg: &[u8; 32]) -> (libsecp256k1::Signature, libsecp256k1::RecoveryId) {
        libsecp256k1::sign(&libsecp256k1::Message::parse(msg), &*self)
    }
}

impl PublicKey {
    pub fn address(&self) -> Address {
        public_key_address(self)
    }
}

pub fn sign(message: &str, key: &SecretKey) -> SigBytes
where
{
    let message_hash = keccak256(message.as_bytes());

    let (signature, recover_id) = key.sign(&message_hash);

    let mut sig_bytes: SigBytes = [0u8; 65];
    sig_bytes[0..32].copy_from_slice(&signature.r.b32());
    sig_bytes[32..64].copy_from_slice(&signature.s.b32());
    sig_bytes[64] = recover_id.serialize();

    sig_bytes
}

pub fn verify<S>(message: &str, address: &Address, signature: S) -> bool
where
    S: AsRef<[u8]>,
{
    if let Ok(sig_bytes) = signature.as_ref().try_into() {
        do_verify(message, address, sig_bytes).unwrap_or(false)
    } else {
        false
    }
}

fn do_verify(message: &str, address: &Address, sig_bytes: &SigBytes) -> Result<bool> {
    let r_s_signature: [u8; 64] = sig_bytes[..64].try_into()?;
    let recovery_id: u8 = sig_bytes[64];

    let message_hash: [u8; 32] = keccak256(message.as_bytes());

    let pubkey = recover(
        &libsecp256k1::Message::parse(&message_hash),
        &libsecp256k1::Signature::parse_standard(&r_s_signature)?,
        &libsecp256k1::RecoveryId::parse(recovery_id)?,
    )?;

    Ok(&PublicKey::from(pubkey).address() == address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn sign_then_verify() {
        let message = "Hello, world!";
        // key generate from https://www.ethereumaddressgenerator.com/
        let address = Address::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();

        // Ensure that the address belongs to the key.
        assert_eq!(address, key.address());

        let sig = sign(message, &key);

        // Verify message signature by address.
        assert!(verify(message, &address, sig));
    }
}
