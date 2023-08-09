#![warn(missing_docs)]

//! Signing/verifying and encrypting/decrypting messages.
//!
//! This module provides a mechanism for node A to verify that the message received was sent by node B.
//! It also allows node A to obtain the public key for sending encrypted messages to node B.
//!
//! Considering security factors, asking user to provide private key is not practical.
//! On the contrary, we generate a delegatee private key and let user sign it.
//!
//! See [DelegateeSk] and [DelegateeSkBuilder] for details.

use std::str::FromStr;

use rings_derive::wasm_export;
use serde::Deserialize;
use serde::Serialize;

use crate::consts::DEFAULT_SESSION_TTL_MS;
use crate::dht::Did;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::utils;

fn pack_session(session_id: Did, ts_ms: u128, ttl_ms: usize) -> String {
    format!("{}\n{}\n{}", session_id, ts_ms, ttl_ms)
}

/// DelegateeSkBuilder is used to build a [DelegateeSk].
///
/// Firstly, you need to provide the delegator's entity and type to `new` method.
/// Then you can call `pack_session` to get the session dump for signing.
/// After signing, you can call `sig` to set the signature back to builder.
/// Finally, you can call `build` to get the [DelegateeSk].
#[wasm_export]
pub struct DelegateeSkBuilder {
    sk: SecretKey,
    /// Delegator of session.
    delegator_entity: String,
    /// Delegator of session.
    delegator_type: String,
    /// Session's lifetime
    ttl_ms: usize,
    /// Timestamp when session created
    ts_ms: u128,
    /// Signature of delegation
    sig: Vec<u8>,
}

/// DelegateeSk holds the [Session] and its delegatee private key.
/// To prove that the message was sent by the [Delegator] of [Session],
/// we need to attach session and the signature signed by sk to the payload.
///
/// DelegateeSk provide a `session` method to clone the session.
/// DelegateeSk also provide `sign` method to sign a message.
///
/// To verify the session, use `verify_self()` method of [Session].
/// To verify a message, use `verify(msg, sig)` method of [Session].
#[wasm_export]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct DelegateeSk {
    /// Session
    session: Session,
    /// The private key of session. Used for signing and decrypting.
    sk: SecretKey,
}

/// Session is used to verify the message.
/// It's serializable and can be attached to the message payload.
///
/// To verify the session is provided by the delegator, use session.verify_self().
/// To verify the message, use session.verify(msg, sig).
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct Session {
    /// Did of session
    session_id: Did,
    /// Delegator of session
    delegator: Delegator,
    /// Session's lifetime
    ttl_ms: usize,
    /// Timestamp when session created
    ts_ms: u128,
    /// Signature to verify that the session was signed by the delegator.
    sig: Vec<u8>,
}

/// We will support as many protocols/algorithms as possible.
/// Currently, it comprises Secp256k1, EIP191, BIP137, and Ed25519.
/// We welcome any issues and PRs for additional implementations.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum Delegator {
    /// ecdsa
    Secp256k1(Did),
    /// ref: <https://eips.ethereum.org/EIPS/eip-191>
    EIP191(Did),
    /// bitcoin bip137 ref: <https://github.com/bitcoin/bips/blob/master/bip-0137.mediawiki>
    BIP137(Did),
    /// ed25519
    Ed25519(PublicKey),
}

impl TryFrom<(String, String)> for Delegator {
    type Error = Error;

    fn try_from((delegator_entity, delegator_type): (String, String)) -> Result<Self> {
        match delegator_type.as_str() {
            "secp256k1" => Ok(Delegator::Secp256k1(Did::from_str(&delegator_entity)?)),
            "eip191" => Ok(Delegator::EIP191(Did::from_str(&delegator_entity)?)),
            "bip137" => Ok(Delegator::BIP137(Did::from_str(&delegator_entity)?)),
            "ed25519" => Ok(Delegator::Ed25519(PublicKey::try_from_b58t(
                &delegator_entity,
            )?)),
            _ => Err(Error::UnknownDelegator),
        }
    }
}

// A DelegateeSk can be converted to a string using JSON and then encoded with base58.
// To load the DelegateeSk from a string, use `DelegateeSk::from_str`.
impl FromStr for DelegateeSk {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = base58_monero::decode_check(s).map_err(|_| Error::Decode)?;
        let delegatee_sk: DelegateeSk = serde_json::from_slice(&s).map_err(Error::Deserialize)?;
        Ok(delegatee_sk)
    }
}

#[wasm_export]
impl DelegateeSkBuilder {
    /// Create a new DelegateeSkBuilder.
    /// The "delegator_type" is lower case of [Delegator] variant.
    /// The "delegator_entity" refers to the entity that is encapsulated by the [Delegator] variant, in string format.
    pub fn new(delegator_entity: String, delegator_type: String) -> DelegateeSkBuilder {
        let sk = SecretKey::random();
        Self {
            sk,
            delegator_entity,
            delegator_type,
            ttl_ms: DEFAULT_SESSION_TTL_MS,
            ts_ms: utils::get_epoch_ms(),
            sig: vec![],
        }
    }

    /// This is a helper method to let user know if the delegator params is valid.
    pub fn validate_delegator(&self) -> bool {
        Delegator::try_from((self.delegator_entity.clone(), self.delegator_type.clone()))
            .map_err(|e| {
                tracing::warn!("validate_delegator error: {:?}", e);
                e
            })
            .is_ok()
    }

    #[deprecated(note="`pack_session` is deprecated, use `unsigned_delegation` instead")]
    /// Will be remove in next version
    pub fn pack_session(&self) -> String {
        pack_session(self.sk.address().into(), self.ts_ms, self.ttl_ms)
    }


    /// Constuct unsigned_info string for signing.
    pub fn unsigned_delegation(&self) -> String {
        pack_session(self.sk.address().into(), self.ts_ms, self.ttl_ms)
    }

    /// Set the signature of session that signed by delegator.
    pub fn set_delegation_sig(mut self, sig: Vec<u8>) -> Self {
        self.sig = sig;
        self
    }

    /// Set the lifetime of session.
    pub fn set_ttl(mut self, ttl_ms: usize) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Build the [DelegateeSk].
    pub fn build(self) -> Result<DelegateeSk> {
        let delegator = Delegator::try_from((self.delegator_entity, self.delegator_type))?;
        let session = Session {
            session_id: self.sk.address().into(),
            delegator,
            ttl_ms: self.ttl_ms,
            ts_ms: self.ts_ms,
            sig: self.sig,
        };

        session.verify_self()?;

        Ok(DelegateeSk {
            session,
            sk: self.sk,
        })
    }
}

impl Session {
    /// Pack the session into a string for verification or public key recovery.
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

        if !(match self.delegator {
            Delegator::Secp256k1(did) => {
                signers::secp256k1::verify(&auth_str, &did.into(), &self.sig)
            }
            Delegator::EIP191(did) => signers::eip191::verify(&auth_str, &did.into(), &self.sig),
            Delegator::BIP137(did) => signers::bip137::verify(&auth_str, &did.into(), &self.sig),
            Delegator::Ed25519(pk) => {
                signers::ed25519::verify(&auth_str, &pk.address(), &self.sig, pk)
            }
        }) {
            return Err(Error::VerifySignatureFailed);
        }

        Ok(())
    }

    /// Verify message.
    pub fn verify(&self, msg: &str, sig: impl AsRef<[u8]>) -> Result<()> {
        self.verify_self()?;
        if !signers::secp256k1::verify(msg, &self.session_id, sig) {
            return Err(Error::VerifySignatureFailed);
        }
        Ok(())
    }

    /// Get public key from session for encryption.
    pub fn delegator_pubkey(&self) -> Result<PublicKey> {
        let auth_str = self.pack();
        match self.delegator {
            Delegator::Secp256k1(_) => signers::secp256k1::recover(&auth_str, &self.sig),
            Delegator::BIP137(_) => signers::bip137::recover(&auth_str, &self.sig),
            Delegator::EIP191(_) => signers::eip191::recover(&auth_str, &self.sig),
            Delegator::Ed25519(pk) => Ok(pk),
        }
    }

    /// Get delegator did.
    pub fn delegator_did(&self) -> Did {
        match self.delegator {
            Delegator::Secp256k1(did) => did,
            Delegator::BIP137(did) => did,
            Delegator::EIP191(did) => did,
            Delegator::Ed25519(pk) => pk.address().into(),
        }
    }
}

impl DelegateeSk {
    /// Generate Session with private key.
    /// Only use it for unittest.
    pub fn new_with_seckey(key: &SecretKey) -> Result<Self> {
        let delegator_entity = Did::from(key.address()).to_string();
        let delegator_type = "secp256k1".to_string();

        let mut builder = DelegateeSkBuilder::new(delegator_entity, delegator_type);

        let sig = key.sign(&builder.unsigned_delegation());
        builder = builder.set_delegation_sig(sig.to_vec());

        builder.build()
    }

    /// Get session from DelegateeSk.
    pub fn session(&self) -> Session {
        self.session.clone()
    }

    /// Sign message with session.
    pub fn sign(&self, msg: &str) -> Result<Vec<u8>> {
        let key = self.sk;
        Ok(signers::secp256k1::sign_raw(key, msg).to_vec())
    }

    /// Get delegator did from session.
    pub fn delegator_did(&self) -> Did {
        self.session.delegator_did()
    }

    /// Dump delegatee_sk to string, allowing user to save it in a config file.
    /// It can be restored using `DelegateeSk::from_str`.
    pub fn dump(&self) -> Result<String> {
        let s = serde_json::to_string(&self).map_err(|_| Error::SerializeError)?;
        base58_monero::encode_check(s.as_bytes()).map_err(|_| Error::Encode)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_session_verify() {
        let key = SecretKey::random();
        let sm = DelegateeSk::new_with_seckey(&key).unwrap();
        let session = sm.session();
        assert!(session.verify_self().is_ok());
    }

    #[test]
    pub fn test_delegator_pubkey() {
        let key = SecretKey::random();
        let sm = DelegateeSk::new_with_seckey(&key).unwrap();
        let session = sm.session();
        let pubkey = session.delegator_pubkey().unwrap();
        assert_eq!(key.pubkey(), pubkey);
    }

    #[test]
    pub fn test_dump_restore() {
        let key = SecretKey::random();
        let sm = DelegateeSk::new_with_seckey(&key).unwrap();
        let dump = sm.dump().unwrap();
        let sm2 = DelegateeSk::from_str(&dump).unwrap();
        assert_eq!(sm, sm2);
    }
}
