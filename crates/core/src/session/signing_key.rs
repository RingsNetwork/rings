use std::str::FromStr;

use rings_derive::wasm_export;
use serde::Deserialize;
use serde::Serialize;

use super::Session;
use super::SessionSkBuilder;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::ecc::keys::AccountVerifier;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// A verified [`Session`] and its delegated private signing key.
///
/// Clone law: cloning a `SessionSk` duplicates the same in-memory signing and decryption authority.
/// The clone preserves the account DID, session identity, and session public key; it does not mint,
/// rotate, or narrow the capability.
#[wasm_export]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct SessionSk {
    session: Session,
    sk: SecretKey,
}

impl FromStr for SessionSk {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = base58_monero::decode_check(s).map_err(|_| Error::Decode)?;
        serde_json::from_slice(&s).map_err(Error::Deserialize)
    }
}

impl SessionSk {
    pub(super) const fn from_parts(session: Session, sk: SecretKey) -> Self {
        Self { session, sk }
    }

    /// Generate a session with a private key. Only use this for unit tests.
    ///
    /// To protect account private keys in production, use [`SessionSkBuilder`] instead.
    pub fn new_with_seckey(key: &SecretKey) -> Result<Self> {
        let account_entity = Did::from(key.address()).to_string();
        let account_type = "secp256k1".to_string();
        let builder = SessionSkBuilder::new(account_entity, account_type);
        let sig = key.sign(&builder.unsigned_proof());
        builder.set_session_sig(sig.to_vec()).build()
    }

    /// Clone the public session proof.
    pub fn session(&self) -> Session {
        self.session.clone()
    }

    /// Return the secp256k1 session public key used for encryption.
    pub fn session_public_key(&self) -> PublicKey<33> {
        self.sk.pubkey()
    }

    /// Decrypt an ElGamal-AEAD envelope with this session key.
    pub fn decrypt_elgamal_aead(
        &self,
        sealed: &crate::ecc::elgamal::impls::secp256k1::AeadCiphertext,
        aad: &[u8],
    ) -> Result<Vec<u8>> {
        crate::ecc::elgamal::impls::secp256k1::decrypt_aead(sealed, aad, self.sk)
    }

    /// Sign a message with the delegated session key.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        let h = keccak256(msg);
        Ok(signers::secp256k1::sign(self.sk, &h).to_vec())
    }

    /// Get the authorizing account DID.
    pub fn account_did(&self) -> Did {
        self.session.account_did()
    }

    /// Get the typed account verifier from the session.
    pub fn account_verifier(&self) -> AccountVerifier {
        self.session.account_verifier()
    }

    /// Encode this session key for storage in a configuration file.
    ///
    /// Restore it with [`SessionSk::from_str`].
    pub fn dump(&self) -> Result<String> {
        let s = serde_json::to_string(self).map_err(|_| Error::SerializeError)?;
        base58_monero::encode_check(s.as_bytes()).map_err(|_| Error::Encode)
    }
}
