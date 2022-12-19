#![warn(missing_docs)]

//! Signing/encrypting and verifying message.
//!
//! To avoid too frequent signing, and keep the private key safe
//! - ECDSA Session is based on secp256k1, which create a temporate secret key with one time signing auth
//! - To create a ECDSA Session, we should generate the unsign_info with our pubkey (Address)
//! - `SessionManager::gen_unsign_info(addr, ..)`, it will returns the msg needs for sign, and a temporate private key
//! - Then we can sign the auth message via some web3 provider like metamask or just with raw private key, and create the SessionManger with
//! - SessionManager::new(sig, auth_info, temp_key)

use std::sync::Arc;
use std::sync::RwLock;

use serde::Deserialize;
use serde::Serialize;

use crate::consts::DEFAULT_SESSION_TTL_MS;
use crate::dht::Did;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::Error;
use crate::err::Result;
use crate::utils;

/// we support both EIP191 and raw ECDSA singing format.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum Signer {
    /// ecdsa
    DEFAULT,
    /// ref: https://eips.ethereum.org/EIPS/eip-191
    EIP191,
    /// ed25519
    EdDSA,
}

/// TTl with specific time, or not set.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum Ttl {
    /// Session's lifetime, (ms)
    Some(usize),
    /// Session will never expired
    Never,
}

/// Authorizor with unique did and pubkey.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct Authorizer {
    /// Sha3 of a public key
    pub did: Did,
    // for ecdsa, it's hash of pubkey, and it can be revealed via `ecdsa.recover`
    /// for ed25519' it's pubkey
    pub pubkey: Option<PublicKey>,
}

/// AuthorizedInfo need authorizer and signer, and set ttl,
/// use to verify in session.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct AuthorizedInfo {
    /// Authorizer of session.
    authorizer: Authorizer,
    /// Signing method of the session.
    signer: Signer,
    /// Session's lifetime
    ttl_ms: Ttl,
    /// Timestamp when session created
    ts_ms: u128,
    /// Did of session.
    session_id: Did,
}

/// Session contain signature which sign with `Signer`, so need AuthorizedInfo as well.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct Session {
    /// Signature
    pub sig: Vec<u8>,
    /// Information for verify Session signature
    pub auth: AuthorizedInfo,
}

/// Session with temp secretKey.
#[derive(Debug, Clone)]
struct SessionWithKey {
    /// Session
    session: Session,
    /// The private key for session.
    session_key: SecretKey,
}

/// Manager about Session.
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
    /// Serialize to string.
    pub fn to_string(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|_| Error::SerializeToString)
    }
}

impl Session {
    /// Generate new session via given signature and auth info.
    pub fn new(sig: &[u8], auth_info: &AuthorizedInfo) -> Self {
        Self {
            sig: sig.to_vec(),
            auth: auth_info.clone(),
        }
    }

    /// Check session is expired or not.
    pub fn is_expired(&self) -> bool {
        if let Ttl::Some(ttl_ms) = self.auth.ttl_ms {
            let now = utils::get_epoch_ms();
            now > self.auth.ts_ms + ttl_ms as u128
        } else {
            false
        }
    }

    /// Verify session.
    pub fn verify(&self) -> bool {
        if self.is_expired() {
            return false;
        }
        if let Ok(auth_str) = self.auth.to_string() {
            match self.auth.signer {
                Signer::DEFAULT => {
                    signers::default::verify(&auth_str, &self.auth.authorizer.did.into(), &self.sig)
                }
                Signer::EIP191 => {
                    signers::eip191::verify(&auth_str, &self.auth.authorizer.did.into(), &self.sig)
                }
                Signer::EdDSA => match self.authorizer_pubkey() {
                    Ok(p) => signers::ed25519::verify(
                        &auth_str,
                        &self.auth.authorizer.did.into(),
                        &self.sig,
                        p,
                    ),
                    Err(_) => false,
                },
            }
        } else {
            false
        }
    }

    /// Get delegated DID from session.
    pub fn did(&self) -> Result<Did> {
        if !self.verify() {
            Err(Error::VerifySignatureFailed)
        } else {
            Ok(self.auth.session_id)
        }
    }

    /// Get public key from session.
    pub fn authorizer_pubkey(&self) -> Result<PublicKey> {
        let auth = self.auth.to_string()?;
        match self.auth.signer {
            Signer::DEFAULT => signers::default::recover(&auth, &self.sig),
            Signer::EIP191 => signers::eip191::recover(&auth, &self.sig),
            Signer::EdDSA => self
                .auth
                .authorizer
                .pubkey
                .ok_or(Error::EdDSAPublicKeyNotFound),
        }
    }
}

impl SessionManager {
    /// Generate unsign info with a given ed25519 pubkey.
    pub fn gen_unsign_info_with_ed25519_pubkey(
        ttl: Option<Ttl>,
        pubkey: PublicKey,
    ) -> Result<(AuthorizedInfo, SecretKey)> {
        Self::gen_unsign_info_with_pubkey(ttl, Some(Signer::EdDSA), pubkey)
    }

    /// Generate unsigned info with public key.
    pub fn gen_unsign_info_with_pubkey(
        ttl: Option<Ttl>,
        signer: Option<Signer>,
        pubkey: PublicKey,
    ) -> Result<(AuthorizedInfo, SecretKey)> {
        let key = SecretKey::random();
        let signer = signer.unwrap_or(Signer::DEFAULT);
        let authorizer = Authorizer {
            did: pubkey.address().into(),
            pubkey: Some(pubkey),
        };
        let info = AuthorizedInfo {
            signer,
            authorizer,
            session_id: key.address().into(),
            ttl_ms: ttl.unwrap_or(Ttl::Some(DEFAULT_SESSION_TTL_MS)),
            ts_ms: utils::get_epoch_ms(),
        };
        Ok((info, key))
    }

    /// Generate unsigned info with.
    pub fn gen_unsign_info(
        did: Did,
        ttl: Option<Ttl>,
        signer: Option<Signer>,
    ) -> (AuthorizedInfo, SecretKey) {
        let key = SecretKey::random();
        let signer = signer.unwrap_or(Signer::DEFAULT);
        let authorizer = Authorizer { did, pubkey: None };
        let info = AuthorizedInfo {
            signer,
            authorizer,
            session_id: key.address().into(),
            ttl_ms: ttl.unwrap_or(Ttl::Some(DEFAULT_SESSION_TTL_MS)),
            ts_ms: utils::get_epoch_ms(),
        };
        (info, key)
    }

    /// sig: Signature of AuthorizedInfo.
    /// auth_info: generated from `gen_unsign_info`.
    /// session_key: temp key from gen_unsign_info.
    pub fn new(sig: &[u8], auth_info: &AuthorizedInfo, session_key: &SecretKey) -> Self {
        let inner = SessionWithKey {
            session: Session::new(sig, auth_info),
            session_key: *session_key,
        };

        Self {
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    /// Generate Session with private key.
    /// Only use it for unittest.
    pub fn new_with_seckey(key: &SecretKey, ttl: Option<Ttl>) -> Result<Self> {
        let (auth, s_key) = Self::gen_unsign_info(key.address().into(), ttl, None);
        let sig = key.sign(&auth.to_string()?).to_vec();
        Ok(Self::new(&sig, &auth, &s_key))
    }

    /// Renew session with new sig.
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

    /// Get secret key from SessionManager.
    pub fn session_key(&self) -> Result<SecretKey> {
        let inner = self
            .inner
            .try_read()
            .map_err(|_| Error::SessionTryLockFailed)?;
        Ok(inner.session_key)
    }

    /// Get session from SessionManager.
    pub fn session(&self) -> Result<Session> {
        let inner = self
            .inner
            .try_read()
            .map_err(|_| Error::SessionTryLockFailed)?;
        Ok(inner.session.clone())
    }

    /// Sign message with session.
    pub fn sign(&self, msg: &str) -> Result<Vec<u8>> {
        let key = self.session_key()?;
        Ok(signers::default::sign_raw(key, msg).to_vec())
    }

    /// Get authorizer from session.
    pub fn authorizer(&self) -> Result<Did> {
        Ok(self.session()?.auth.authorizer.did)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_session_verify() {
        let key = SecretKey::random();
        let sm = SessionManager::new_with_seckey(&key, None).unwrap();
        let session = sm.session().unwrap();
        assert!(session.verify());
    }

    #[test]
    pub fn test_authorizer_pubkey() {
        let key = SecretKey::random();
        let sm = SessionManager::new_with_seckey(&key, None).unwrap();
        let session = sm.session().unwrap();
        let pubkey = session.authorizer_pubkey().unwrap();
        assert_eq!(key.pubkey(), pubkey);
    }
}
