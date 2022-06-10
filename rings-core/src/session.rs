//! To avoid too frequent signing, and keep the private key safe, we implemented a session protocol for signing/encrypting and verifying message.
//! - ECDSA Session is based on secp256k1, which create a temporate secret key with one time signing auth
//! - To create a ECDSA Session, we should generate the unsign_info with our pubkey (Address)
//! - `SessionManager::gen_unsign_info(addr, ..)`, it will returns the msg needs for sign, and a temporate private key
//! - Then we can sign the auth message via some web3 provider like metamask or just with raw private key, and create the SessionManger with
//! - SessionManager::new(sig, auth_info, temp_key)

use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::{Error, Result};
use crate::utils;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use std::sync::RwLock;
use web3::types::Address;

const DEFAULT_TTL_MS: usize = 24 * 3600 * 1000;

/// we support both EIP712 and raw ECDSA singing forrmat
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum Signer {
    DEFAULT,
    EIP712,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum Ttl {
    Some(usize),
    Never,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct AuthorizedInfo {
    pub authorizer: Address,
    pub signer: Signer,
    pub addr: Address,
    pub ttl_ms: Ttl,
    pub ts_ms: u128,
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
    pub fn new(sig: &[u8], auth_info: &AuthorizedInfo) -> Self {
        Self {
            sig: sig.to_vec(),
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
        if let Ok(auth_str) = self.auth.to_string() {
            match self.auth.signer {
                Signer::DEFAULT => {
                    signers::default::verify(&auth_str, &self.auth.authorizer, &self.sig)
                }
                Signer::EIP712 => {
                    signers::eip712::verify(&auth_str, &self.auth.authorizer, &self.sig)
                }
            }
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

    pub fn authorizer_pubkey(&self) -> Result<PublicKey> {
        let auth = self.auth.to_string()?;
        match self.auth.signer {
            Signer::DEFAULT => signers::default::recover(&auth, &self.sig),
            Signer::EIP712 => signers::eip712::recover(&auth, &self.sig),
        }
    }
}

impl SessionManager {
    pub fn gen_unsign_info(
        authorizer: Address,
        ttl: Option<Ttl>,
        signer: Option<Signer>,
    ) -> Result<(AuthorizedInfo, SecretKey)> {
        let key = SecretKey::random();
        let signer = signer.unwrap_or(Signer::DEFAULT);
        let info = AuthorizedInfo {
            signer,
            authorizer,
            addr: key.address(),
            ttl_ms: ttl.unwrap_or(Ttl::Some(DEFAULT_TTL_MS)),
            ts_ms: utils::get_epoch_ms(),
        };
        Ok((info, key))
    }

    /// sig: Sigature of AuthorizedInfo
    /// auth_info: generated from `gen_unsign_info`
    /// session_key: temp key from gen_unsign_info
    pub fn new(sig: &[u8], auth_info: &AuthorizedInfo, session_key: &SecretKey) -> Self {
        let inner = SessionWithKey {
            session: Session::new(sig, auth_info),
            session_key: *session_key,
        };

        Self {
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    /// generate Session with private key
    /// only use it for unittest
    pub fn new_with_seckey(key: &SecretKey) -> Result<Self> {
        let (auth, s_key) = Self::gen_unsign_info(key.address(), None, None)?;
        let sig = key.sign(&auth.to_string()?).to_vec();
        Ok(Self::new(&sig, &auth, &s_key))
    }

    pub fn renew(&self, sig: &[u8], auth_info: &AuthorizedInfo, key: &SecretKey) -> Result<&Self> {
        let new_inner = SessionWithKey {
            session: Session::new(sig, auth_info),
            session_key: *key,
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
        let s = self.session()?;
        let key = self.session_key()?;
        match s.auth.signer {
            Signer::DEFAULT => Ok(signers::default::sign_raw(key, msg).to_vec()),
            Signer::EIP712 => Ok(signers::eip712::sign_raw(key, msg).to_vec()),
        }
    }

    pub fn authorizer(&self) -> Result<Address> {
        Ok(self.session()?.auth.authorizer)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_session_verify() {
        let key = SecretKey::random();
        let sm = SessionManager::new_with_seckey(&key).unwrap();
        let session = sm.session().unwrap();
        assert!(session.verify());
    }

    #[test]
    pub fn test_authorizer_pubkey() {
        let key = SecretKey::random();
        let sm = SessionManager::new_with_seckey(&key).unwrap();
        let session = sm.session().unwrap();
        let pubkey = session.authorizer_pubkey().unwrap();
        assert_eq!(key.pubkey(), pubkey);
    }
}
