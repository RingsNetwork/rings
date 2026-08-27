use serde::Deserialize;
use serde::Serialize;

use super::Account;
use crate::dht::Did;
use crate::ecc::keys::AccountVerifier;
use crate::ecc::keys::VerificationPublicKey;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::error::Error;
use crate::error::Result;
use crate::utils;

pub(super) fn pack_session(session_id: Did, ts_ms: u128, ttl_ms: u64) -> String {
    format!("{session_id}\n{ts_ms}\n{ttl_ms}")
}

/// A serializable session proof used to verify messages signed by a delegated session key.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct Session {
    /// DID of the session public key.
    pub(super) session_id: Did,
    /// Account that authorized the session.
    pub(super) account: Account,
    /// Session lifetime.
    pub(super) ttl_ms: u64,
    /// Timestamp when the session was created.
    pub(super) ts_ms: u128,
    /// Account signature authorizing the session.
    pub(super) sig: Vec<u8>,
}

impl Session {
    /// Pack the session into a string for verification or public key recovery.
    pub fn pack(&self) -> Vec<u8> {
        pack_session(self.session_id, self.ts_ms, self.ttl_ms)
            .as_bytes()
            .to_vec()
    }

    /// Return the DID of the session public key.
    pub fn session_did(&self) -> Did {
        self.session_id
    }

    /// Check whether the session has expired.
    pub fn is_expired(&self) -> bool {
        let now = utils::get_epoch_ms();
        now > self.ts_ms + self.ttl_ms as u128
    }

    /// Verify that the account authorized this unexpired session.
    pub fn verify_self(&self) -> Result<()> {
        if self.is_expired() {
            return Err(Error::SessionExpired);
        }

        let auth_bytes = self.pack();
        if !self
            .account
            .account_verifier()
            .verify(&auth_bytes, &self.sig)
        {
            return Err(Error::VerifySignatureFailed);
        }
        Ok(())
    }

    /// Verify a message signed by this session key.
    pub fn verify(&self, msg: &[u8], sig: impl AsRef<[u8]>) -> Result<()> {
        self.verify_self()?;
        if !signers::secp256k1::verify(msg, &self.session_id, sig) {
            return Err(Error::VerifySignatureFailed);
        }
        Ok(())
    }

    /// Get the legacy secp256k1-compatible account public key.
    ///
    /// Use [`Session::account_verification_pubkey`] for typed account verification keys.
    pub fn account_pubkey(&self) -> Result<PublicKey<33>> {
        match self.account_verification_pubkey()? {
            VerificationPublicKey::Secp256k1(pk)
            | VerificationPublicKey::Eip191(pk)
            | VerificationPublicKey::Bip137(pk) => Ok(pk),
            VerificationPublicKey::Secp256r1(_)
            | VerificationPublicKey::Ed25519(_)
            | VerificationPublicKey::Bls12381(_) => Err(Error::UnknownAccount),
        }
    }

    /// Get the typed account verification public key from the session proof.
    pub fn account_verification_pubkey(&self) -> Result<VerificationPublicKey> {
        self.account
            .account_verifier()
            .verification_key_from_signature(&self.pack(), &self.sig)
    }

    /// Get the typed account verifier.
    pub fn account_verifier(&self) -> AccountVerifier {
        self.account.account_verifier()
    }

    /// Get the authorizing account DID.
    pub fn account_did(&self) -> Did {
        self.account.account_verifier().did()
    }
}
