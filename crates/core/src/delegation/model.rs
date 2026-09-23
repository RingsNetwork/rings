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

pub(super) fn pack_delegation(delegatee_did: Did, ts_ms: u128, ttl_ms: u64) -> String {
    format!("{delegatee_did}\n{ts_ms}\n{ttl_ms}")
}

/// A serializable authorization proof signed by a delegator for a delegatee key.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct Delegation {
    /// DID derived from the delegatee public signing key.
    pub(super) delegatee_did: Did,
    /// Account identity that authorized this delegation.
    pub(super) delegator: Account,
    /// Delegation lifetime.
    pub(super) ttl_ms: u64,
    /// Timestamp when the delegation was created.
    pub(super) ts_ms: u128,
    /// Delegator signature authorizing the delegatee key.
    pub(super) delegator_signature: Vec<u8>,
}

impl Delegation {
    /// Pack the delegatee DID and validity period for signature verification.
    pub fn pack(&self) -> Vec<u8> {
        pack_delegation(self.delegatee_did, self.ts_ms, self.ttl_ms)
            .as_bytes()
            .to_vec()
    }

    /// Return the DID derived from the delegatee public signing key.
    pub fn delegatee_did(&self) -> Did {
        self.delegatee_did
    }

    /// Check whether this delegation has expired.
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(utils::get_epoch_ms())
    }

    /// Check whether this delegation had expired at the instant `at_ms`.
    ///
    /// Total over every stamp: a lifetime that overflows the clock saturates, so a delegation
    /// that arrives unsigned on a link (an announcement) cannot make this end panic.
    pub fn is_expired_at(&self, at_ms: u128) -> bool {
        at_ms > self.ts_ms.saturating_add(u128::from(self.ttl_ms))
    }

    /// Verify that the delegator authorized this unexpired delegation.
    pub fn verify_delegator_authorization(&self) -> Result<()> {
        self.verify_delegator_authorization_at(utils::get_epoch_ms())
    }

    /// Verify that the delegator authorized this delegation and that it was live at `at_ms`.
    pub fn verify_delegator_authorization_at(&self, at_ms: u128) -> Result<()> {
        if self.is_expired_at(at_ms) {
            return Err(Error::DelegationExpired);
        }

        let auth_bytes = self.pack();
        if !self
            .delegator
            .account_verifier()
            .verify(&auth_bytes, &self.delegator_signature)
        {
            return Err(Error::VerifySignatureFailed);
        }
        Ok(())
    }

    /// Verify a message signed by the delegatee key under this delegation.
    pub fn verify(&self, msg: &[u8], sig: impl AsRef<[u8]>) -> Result<()> {
        self.verify_at(msg, sig, utils::get_epoch_ms())
    }

    /// Verify a message signed by the delegatee key at `at_ms`: the delegation must
    /// have been live then, whatever it is now.
    pub fn verify_at(&self, msg: &[u8], sig: impl AsRef<[u8]>, at_ms: u128) -> Result<()> {
        self.verify_delegator_authorization_at(at_ms)?;
        if !signers::secp256k1::verify(msg, &self.delegatee_did, sig) {
            return Err(Error::VerifySignatureFailed);
        }
        Ok(())
    }

    /// Get the legacy secp256k1-compatible delegator public key.
    ///
    /// Use [`Delegation::delegator_verification_pubkey`] for typed delegator verification keys.
    pub fn delegator_pubkey(&self) -> Result<PublicKey<33>> {
        match self.delegator_verification_pubkey()? {
            VerificationPublicKey::Secp256k1(pk)
            | VerificationPublicKey::Eip191(pk)
            | VerificationPublicKey::Bip137(pk) => Ok(pk),
            VerificationPublicKey::Secp256r1(_)
            | VerificationPublicKey::Ed25519(_)
            | VerificationPublicKey::Bls12381(_) => Err(Error::UnknownAccount),
        }
    }

    /// Get the typed delegator verification public key from this delegation proof.
    pub fn delegator_verification_pubkey(&self) -> Result<VerificationPublicKey> {
        self.delegator
            .account_verifier()
            .verification_key_from_signature(&self.pack(), &self.delegator_signature)
    }

    /// Get the typed account verifier.
    pub fn delegator_verifier(&self) -> AccountVerifier {
        self.delegator.account_verifier()
    }

    /// Get the DID of the delegator that authorized this delegation.
    pub fn delegator_did(&self) -> Did {
        self.delegator.account_verifier().did()
    }
}
