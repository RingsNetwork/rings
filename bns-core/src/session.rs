use crate::ecc::verify;
use crate::ecc::SecretKey;
use crate::err::{Error, Result};
use crate::utils;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use std::sync::RwLock;
use web3::types::Address;

const DEFAULT_TTL_MS: usize = 24 * 3600 * 1000;

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum Ttl {
    Some(usize),
    Never,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct AuthorizedInfo {
    authorizer: Address,
    addr: Address,
    ttl_ms: Ttl,
    ts_ms: u128,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct Session {
    pub sig: Vec<u8>,
    pub auth: AuthorizedInfo,
}

#[derive(Debug, Clone)]
pub struct SessionWithKey {
    pub session: Session,
    pub session_key: SecretKey,
}

#[derive(Debug)]
pub struct SessionManager {
    inner: Arc<RwLock<SessionWithKey>>,
}

impl Clone for SessionManager {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl AuthorizedInfo {
    pub fn to_string(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|_| Error::SerializeToString)
    }
}

impl Session {
    pub fn new(sig: &Vec<u8>, auth_info: &AuthorizedInfo) -> Self {
        Self {
            sig: sig.clone(),
            auth: auth_info.clone(),
        }
    }

    pub fn is_expired(&self) -> bool {
        if let Ttl::Some(ttl_ms) = self.auth.ttl_ms {
            let now = utils::get_epoch_ms();
            now > self.auth.ts_ms + ttl_ms as u128
        } else {
            false
        }
    }

    pub fn verify(&self) -> bool {
        if self.is_expired() {
            return false;
        }
        if let Ok(auth_str) =
            serde_json::to_string(&self.auth).map_err(|_| Error::SerializeToString)
        {
            verify(&auth_str, &self.auth.authorizer, self.sig.clone())
        } else {
            false
        }
    }

    pub fn address(&self) -> Result<Address> {
        if !self.verify() {
            Err(Error::VerifySignatureFailed)
        } else {
            Ok(self.auth.addr)
        }
    }
}

impl SessionManager {
    pub fn gen_unsign_info(
        authorizer: Address,
        ttl: Option<Ttl>,
    ) -> Result<(AuthorizedInfo, SecretKey)> {
        let key = SecretKey::random();
        let info = AuthorizedInfo {
            authorizer,
            addr: key.address(),
            ttl_ms: ttl.unwrap_or(Ttl::Some(DEFAULT_TTL_MS)),
            ts_ms: utils::get_epoch_ms(),
        };
        Ok((info, key))
    }

    pub fn new(sig: &Vec<u8>, auth_info: &AuthorizedInfo, key: &SecretKey) -> Self {
        let inner = SessionWithKey {
            session: Session::new(&sig, &auth_info),
            session_key: key.clone(),
        };

        Self {
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    pub fn renew(
        &self,
        sig: &Vec<u8>,
        auth_info: &AuthorizedInfo,
        key: &SecretKey,
    ) -> Result<&Self> {
        let new_inner = SessionWithKey {
            session: Session::new(&sig, &auth_info),
            session_key: key.clone(),
        };
        let mut inner = self
            .inner
            .try_write()
            .map_err(|_| Error::SessionTryLockFailed)?;
        *inner = new_inner;
        Ok(self)
    }

    pub fn session_key(&self) -> Result<SecretKey> {
        let inner = self
            .inner
            .try_read()
            .map_err(|_| Error::SessionTryLockFailed)?;
        Ok(inner.session_key)
    }

    pub fn session(&self) -> Result<Session> {
        let inner = self
            .inner
            .try_read()
            .map_err(|_| Error::SessionTryLockFailed)?;
        Ok(inner.session.clone())
    }

    pub fn sign(&self, msg: &str) -> Result<Vec<u8>> {
        Ok(self.session_key()?.sign(&msg).to_vec())
    }

    pub fn authorizer(&self) -> Result<Address> {
        Ok(self.session()?.auth.authorizer)
    }
}
