#![warn(missing_docs)]

//! Signing/verifying and encrypting/decrypting messages.
//!
//! This module provides a mechanism for node A to verify that the message received was sent by node B.
//! It also allows node A to obtain the public key for sending encrypted messages to node B.
//!
//! If we have the private key of node B, we can easily implement it. Just regular actions about cryptograph.
//! But, considering security factors, asking user to provide private key is not practical.
//! On the contrary, we generate a temporary private key and let user sign it. See [SessionManager] for details.
//!
//! To avoid frequent signing, and keep the private key safe
//! - ECDSA Session is based on secp256k1, which create a temporate secret key with one time signing auth
//! - To create a ECDSA Session, we should generate the unsign_info with our pubkey (Address)
//! - `SessionManager::gen_unsign_info(addr, ..)`, it will returns the msg needs for sign, and a temporate private key
//! - Then we can sign the auth message via some web3 provider like metamask or just with raw private key, and create the SessionManger with
//! - SessionManager::new(sig, auth_info, temp_key)

use std::str::FromStr;

use rings_derive::wasm_export;
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

fn pack_session(session_id: Did, ts_ms: u128, ttl_ms: usize) -> String {
    format!("{}\n{}\n{}", session_id, ts_ms, ttl_ms)
}

#[wasm_export]
pub struct SessionManagerBuilder {
    session_key: SecretKey,
    /// Authorizer of session.
    authorizer_entity: String,
    /// Authorizer of session.
    authorizer_type: String,
    /// Session's lifetime
    ttl_ms: usize,
    /// Timestamp when session created
    ts_ms: u128,
    /// Signature
    sig: Vec<u8>,
}

/// SessionManager holds the [Session] and its temporary private key.
#[derive(Debug)]
pub struct SessionManager {
    /// Session
    pub session: Session,
    /// The private key of session. Used for signing and decrypting.
    pub session_key: SecretKey,
}

/// Session contains an AuthorizedInfo and its signature.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct Session {
    /// Did of session.
    session_id: Did,
    /// Authorizer of session.
    authorizer: Authorizer,
    /// Session's lifetime
    ttl_ms: usize,
    /// Timestamp when session created
    ts_ms: u128,
    /// Signature
    sig: Vec<u8>,
}

/// we support both EIP191 and raw ECDSA singing format.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum Authorizer {
    /// ecdsa
    Secp256k1(Did),
    /// ref: <https://eips.ethereum.org/EIPS/eip-191>
    EIP191(Did),
    /// bitcoin bip137 ref: <https://github.com/bitcoin/bips/blob/master/bip-0137.mediawiki>
    BIP137(Did),
    /// ed25519
    Ed25519(PublicKey),
}

impl TryFrom<(String, String)> for Authorizer {
    type Error = Error;

    fn try_from((authorizer_entity, authorizer_type): (String, String)) -> Result<Self> {
        match authorizer_type.as_str() {
            "secp256k1" => Ok(Authorizer::Secp256k1(Did::from_str(&authorizer_entity)?)),
            "eip191" => Ok(Authorizer::EIP191(Did::from_str(&authorizer_entity)?)),
            "bip137" => Ok(Authorizer::BIP137(Did::from_str(&authorizer_entity)?)),
            "ed25519" => Ok(Authorizer::Ed25519(PublicKey::try_from_b58t(
                &authorizer_entity,
            )?)),
            _ => Err(Error::UnknownAuthorizer),
        }
    }
}

#[wasm_export]
impl SessionManagerBuilder {
    pub fn new(authorizer_entity: String, authorizer_type: String) -> SessionManagerBuilder {
        let session_key = SecretKey::random();
        Self {
            session_key,
            authorizer_entity,
            authorizer_type,
            ttl_ms: DEFAULT_SESSION_TTL_MS,
            ts_ms: utils::get_epoch_ms(),
            sig: vec![],
        }
    }

    pub fn validate_authorizer(&self) -> bool {
        Authorizer::try_from((self.authorizer_entity.clone(), self.authorizer_type.clone()))
            .map_err(|e| {
                tracing::warn!("validate_authorizer error: {:?}", e);
                e
            })
            .is_ok()
    }

    pub fn pack_session(&self) -> String {
        pack_session(self.session_key.address().into(), self.ts_ms, self.ttl_ms)
    }

    pub fn sig(mut self, sig: Vec<u8>) -> Self {
        self.sig = sig;
        self
    }

    pub fn ttl(mut self, ttl_ms: Option<usize>) -> Self {
        self.ttl_ms = ttl_ms.unwrap_or(DEFAULT_SESSION_TTL_MS);
        self
    }
}

impl SessionManagerBuilder {
    pub fn build(self) -> Result<SessionManager> {
        let authorizer = Authorizer::try_from((self.authorizer_entity, self.authorizer_type))?;
        let session = Session {
            session_id: self.session_key.address().into(),
            authorizer,
            ttl_ms: self.ttl_ms,
            ts_ms: self.ts_ms,
            sig: self.sig,
        };

        session.verify_self()?;

        Ok(SessionManager {
            session,
            session_key: self.session_key,
        })
    }
}

impl Session {
    pub fn pack(&self) -> String {
        pack_session(self.session_id, self.ts_ms, self.ttl_ms)
    }

    /// Check session is expired or not.
    pub fn is_expired(&self) -> bool {
        let now = utils::get_epoch_ms();
        now > self.ts_ms + self.ttl_ms as u128
    }

    /// Verify session.
    pub fn verify_self(&self) -> Result<()> {
        if self.is_expired() {
            return Err(Error::SessionExpired);
        }

        let auth_str = self.pack();

        if !(match self.authorizer {
            Authorizer::Secp256k1(did) => {
                signers::secp256k1::verify(&auth_str, &did.into(), &self.sig)
            }
            Authorizer::EIP191(did) => signers::eip191::verify(&auth_str, &did.into(), &self.sig),
            Authorizer::BIP137(did) => signers::bip137::verify(&auth_str, &did.into(), &self.sig),
            Authorizer::Ed25519(pk) => {
                signers::ed25519::verify(&auth_str, &pk.address(), &self.sig, pk)
            }
        }) {
            return Err(Error::VerifySignatureFailed);
        }

        Ok(())
    }

    pub fn verify(&self, msg: &str, sig: impl AsRef<[u8]>) -> Result<()> {
        self.verify_self()?;
        if !signers::secp256k1::verify(msg, &self.session_id, sig) {
            return Err(Error::VerifySignatureFailed);
        }
        Ok(())
    }

    /// Get public key from session.
    pub fn authorizer_pubkey(&self) -> Result<PublicKey> {
        let auth_str = self.pack();
        match self.authorizer {
            Authorizer::Secp256k1(_) => signers::secp256k1::recover(&auth_str, &self.sig),
            Authorizer::BIP137(_) => signers::bip137::recover(&auth_str, &self.sig),
            Authorizer::EIP191(_) => signers::eip191::recover(&auth_str, &self.sig),
            Authorizer::Ed25519(pk) => Ok(pk),
        }
    }
}

impl SessionManager {
    /// Generate Session with private key.
    /// Only use it for unittest.
    pub fn new_with_seckey(key: &SecretKey) -> Result<Self> {
        let authorizer_entity = Did::from(key.address()).to_string();
        let authorizer_type = "secp256k1".to_string();

        let mut builder = SessionManagerBuilder::new(authorizer_entity, authorizer_type);

        let sig = key.sign(&builder.pack_session());
        builder = builder.sig(sig.to_vec());

        builder.build()
    }

    /// Get session from SessionManager.
    pub fn session(&self) -> Session {
        self.session.clone()
    }

    /// Sign message with session.
    pub fn sign(&self, msg: &str) -> Result<Vec<u8>> {
        let key = self.session_key;
        Ok(signers::secp256k1::sign_raw(key, msg).to_vec())
    }

    /// Get authorizer from session.
    pub fn authorizer(&self) -> Did {
        match self.session.authorizer {
            Authorizer::Secp256k1(did) => did,
            Authorizer::BIP137(did) => did,
            Authorizer::EIP191(did) => did,
            Authorizer::Ed25519(pk) => pk.address().into(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_session_verify() {
        let key = SecretKey::random();
        let sm = SessionManager::new_with_seckey(&key).unwrap();
        let session = sm.session();
        assert!(session.verify_self().is_ok());
    }

    #[test]
    pub fn test_authorizer_pubkey() {
        let key = SecretKey::random();
        let sm = SessionManager::new_with_seckey(&key).unwrap();
        let session = sm.session();
        let pubkey = session.authorizer_pubkey().unwrap();
        assert_eq!(key.pubkey(), pubkey);
    }
}
