use std::str::FromStr;

use rings_derive::wasm_export;
use serde::Deserialize;
use serde::Serialize;
use zeroize::Zeroizing;

use super::Delegation;
use super::DelegationBuilder;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::ecc::keys::AccountVerifier;
use crate::ecc::prime_order::NonIdentityPoint;
use crate::ecc::prime_order::NonZeroScalar;
use crate::ecc::prime_order::SHARED_SECRET_BYTES;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::Secp256k1;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// A verified [`Delegation`] and the delegatee's private signing key.
///
/// Clone law: cloning a `DelegateeKey` duplicates the same in-memory signing and decryption
/// authority. The clone preserves the delegator DID, delegatee identity, and delegatee public
/// key; it does not mint, rotate, or narrow the capability.
#[wasm_export]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct DelegateeKey {
    delegation: Delegation,
    delegatee_secret_key: SecretKey,
}

impl FromStr for DelegateeKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = crate::base58_check::decode(s).map_err(|_| Error::Decode)?;
        serde_json::from_slice(&s).map_err(Error::Deserialize)
    }
}

impl DelegateeKey {
    pub(super) const fn from_parts(
        delegation: Delegation,
        delegatee_secret_key: SecretKey,
    ) -> Self {
        Self {
            delegation,
            delegatee_secret_key,
        }
    }

    #[cfg(test)]
    /// Construct a deterministic delegation for cross-target wire fixtures.
    pub(crate) fn from_test_keys(
        account_key: &SecretKey,
        delegatee_key: SecretKey,
        ts_ms: u128,
        ttl_ms: u64,
    ) -> Result<Self> {
        let delegatee_did = Did::from(delegatee_key.address());
        let proof = super::model::pack_delegation(delegatee_did, ts_ms, ttl_ms);
        let delegation = Delegation {
            delegatee_did,
            delegator: super::Account::Secp256k1(account_key.address().into()),
            ttl_ms,
            ts_ms,
            delegator_signature: account_key.sign(&proof)?.to_vec(),
        };
        delegation.verify_delegator_authorization_at(ts_ms)?;
        Ok(Self::from_parts(delegation, delegatee_key))
    }

    /// Generate a self-delegated key from an existing private key. Only use this for unit tests.
    ///
    /// To protect account private keys in production, use [`DelegationBuilder`] instead.
    pub fn new_with_seckey(key: &SecretKey) -> Result<Self> {
        let account_entity = Did::from(key.address()).to_string();
        let account_type = "secp256k1".to_string();
        let builder = DelegationBuilder::new(account_entity, account_type);
        let sig = key.sign(&builder.unsigned_proof())?;
        builder.set_delegator_signature(sig.to_vec()).build()
    }

    /// Clone the public delegation proof.
    pub fn delegation(&self) -> Delegation {
        self.delegation.clone()
    }

    /// Return the secp256k1 delegatee public key used for encryption.
    pub fn delegatee_public_key(&self) -> PublicKey<33> {
        self.delegatee_secret_key.pubkey()
    }

    /// Decrypt an ElGamal-AEAD envelope with this delegatee key.
    pub fn decrypt_elgamal_aead(
        &self,
        sealed: &crate::ecc::elgamal::impls::secp256k1::AeadCiphertext,
        aad: &[u8],
    ) -> Result<Vec<u8>> {
        crate::ecc::elgamal::impls::secp256k1::decrypt_aead(sealed, aad, &self.delegatee_secret_key)
    }

    /// The Diffie–Hellman shared secret `x(P·d)` of a peer element `P` and the delegatee secret
    /// `d`, zeroized on drop.
    ///
    /// Law: for every `x ∈ Z_n^*`, `diffie_hellman(x·G) = (d·G)·x` in its x-coordinate, so a
    /// sender holding `x` and [`Self::delegatee_public_key`] derives the same secret; this is the
    /// key transport of a Sphinx header (#834 D6″). `P ≠ O` by its type, and the secret scalar is
    /// only ever held in a zeroizing `Z_n^*` value.
    pub fn diffie_hellman(
        &self,
        peer: &NonIdentityPoint<Secp256k1>,
    ) -> Zeroizing<[u8; SHARED_SECRET_BYTES]> {
        peer.shared_secret(&NonZeroScalar::from_secret_key(&self.delegatee_secret_key))
    }

    /// Sign a message with this delegatee key.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        let h = keccak256(msg);
        Ok(signers::secp256k1::sign(&self.delegatee_secret_key, &h)?.to_vec())
    }

    /// Get the DID of the delegator.
    pub fn delegator_did(&self) -> Did {
        self.delegation.delegator_did()
    }

    /// Get the delegator account verifier from this delegation.
    pub fn delegator_verifier(&self) -> AccountVerifier {
        self.delegation.delegator_verifier()
    }

    /// Encode this delegatee key for storage in a configuration file.
    ///
    /// Restore it with [`DelegateeKey::from_str`].
    pub fn dump(&self) -> Result<String> {
        let s = serde_json::to_string(self).map_err(|_| Error::SerializeError)?;
        base58_monero::encode_check(s.as_bytes()).map_err(|_| Error::Encode)
    }
}
