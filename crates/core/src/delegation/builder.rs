use rings_derive::wasm_export;

use super::model::pack_delegation;
use super::Account;
use super::DelegateeKey;
use super::Delegation;
use crate::consts::DEFAULT_DELEGATION_TTL_MS;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::utils;

/// Builds a [`DelegateeKey`] from an external account authorization.
#[wasm_export]
pub struct DelegationBuilder {
    delegatee_secret_key: SecretKey,
    delegator_entity: String,
    delegator_type: String,
    ttl_ms: u64,
    ts_ms: u128,
    delegator_signature: Vec<u8>,
}

#[wasm_export]
impl DelegationBuilder {
    /// Create a new `DelegationBuilder`.
    ///
    /// `delegator_type` is the lowercase account algorithm name and `delegator_entity` is the encoded
    /// entity accepted by that account algorithm.
    pub fn new(delegator_entity: String, delegator_type: String) -> DelegationBuilder {
        let delegatee_secret_key = SecretKey::random();
        Self {
            delegatee_secret_key,
            delegator_entity,
            delegator_type,
            ttl_ms: DEFAULT_DELEGATION_TTL_MS,
            ts_ms: utils::get_epoch_ms(),
            delegator_signature: vec![],
        }
    }

    /// Construct the proof string that the external account must sign.
    pub fn unsigned_proof(&self) -> String {
        pack_delegation(
            self.delegatee_secret_key.address().into(),
            self.ts_ms,
            self.ttl_ms,
        )
    }

    /// Set the delegator signature authorizing this delegation.
    pub fn set_delegator_signature(mut self, delegator_signature: Vec<u8>) -> Self {
        self.delegator_signature = delegator_signature;
        self
    }

    /// Set the delegation lifetime.
    pub fn set_ttl(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Verify the authorization and build the delegatee key.
    pub fn build(self) -> Result<DelegateeKey> {
        let delegator = Account::try_from((self.delegator_entity, self.delegator_type))?;
        let delegation = Delegation {
            delegatee_did: self.delegatee_secret_key.address().into(),
            delegator,
            ttl_ms: self.ttl_ms,
            ts_ms: self.ts_ms,
            delegator_signature: self.delegator_signature,
        };

        delegation.verify_delegator_authorization()?;
        Ok(DelegateeKey::from_parts(
            delegation,
            self.delegatee_secret_key,
        ))
    }
}
