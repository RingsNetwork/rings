use rings_derive::wasm_export;

use super::model::pack_session;
use super::Account;
use super::Session;
use super::SessionSk;
use crate::consts::DEFAULT_SESSION_TTL_MS;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::utils;

/// Builds a [`SessionSk`] from an external account authorization.
#[wasm_export]
pub struct SessionSkBuilder {
    sk: SecretKey,
    account_entity: String,
    account_type: String,
    ttl_ms: u64,
    ts_ms: u128,
    sig: Vec<u8>,
}

#[wasm_export]
impl SessionSkBuilder {
    /// Create a new `SessionSkBuilder`.
    ///
    /// `account_type` is the lowercase account algorithm name and `account_entity` is the encoded
    /// entity accepted by that account algorithm.
    pub fn new(account_entity: String, account_type: String) -> SessionSkBuilder {
        let sk = SecretKey::random();
        Self {
            sk,
            account_entity,
            account_type,
            ttl_ms: DEFAULT_SESSION_TTL_MS,
            ts_ms: utils::get_epoch_ms(),
            sig: vec![],
        }
    }

    /// Return whether the configured account type and entity form a valid account.
    pub fn validate_account(&self) -> bool {
        Account::try_from((self.account_entity.clone(), self.account_type.clone()))
            .map_err(|error| {
                tracing::debug!(?error, "session account validation failed");
                error
            })
            .is_ok()
    }

    /// Construct the proof string that the external account must sign.
    pub fn unsigned_proof(&self) -> String {
        pack_session(self.sk.address().into(), self.ts_ms, self.ttl_ms)
    }

    /// Set the account signature authorizing this session.
    pub fn set_session_sig(mut self, sig: Vec<u8>) -> Self {
        self.sig = sig;
        self
    }

    /// Set the session lifetime.
    pub fn set_ttl(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Verify the authorization and build the session key.
    pub fn build(self) -> Result<SessionSk> {
        let account = Account::try_from((self.account_entity, self.account_type))?;
        let session = Session {
            session_id: self.sk.address().into(),
            account,
            ttl_ms: self.ttl_ms,
            ts_ms: self.ts_ms,
            sig: self.sig,
        };

        session.verify_self()?;
        Ok(SessionSk::from_parts(session, self.sk))
    }
}
