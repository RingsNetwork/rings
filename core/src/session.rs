#![warn(missing_docs)]
//! Understanding Abstract Account and Session keypair in Rings Network
//!
//! The Rings network offers a unique mechanism to bolster security and abstract the user's keypair through a feature known as session keypair.
//!
//! The fundamental concept behind keypair session involves creating an association between a user's keypair and a randomly generated keypair. In our terminology:
//!
//!    The user's original keypair (private key, public key) is referred to as the "account" (sk, pk).
//!    The randomly generated keypair by the Rings network is known as the "session" (sk, pk).
//!
//! * Here's how the process works:
//!
//! 1. A random delegate private key (sk) is generated, along with its corresponding public key (pk).
//!
//! 2. A session is formed based on the session's public key and the account's public key. This can be conceptualized as a contract stating, "I delegate to {pk} for the time period {ts, ttl}".
//!
//! 3. The account must sign the session, now termed "Session", using its private key.
//!
//! 4. When sending and receiving messages, the Rings network will handle message signing and verification using the session's keypair (sk, pk).
//!
//!
//! SessionSkBuilder, SessionSk was exported to wasm envirement, so in browser/wasm envirement it can be done with nodejs code:
//! ```js
//!    // prepare auth & send to metamask for sign
//!    let sessionBuilder = SessionSkBuilder.new(account, 'eip191')
//!    let unsignedSession = sessionBuilder.unsigned_proof()
//!    const { signed } = await sendMessage(
//!      'sign-message',
//!      {
//!        auth: unsignedSession,
//!      },
//!      'popup'
//!    )
//!    const signature = new Uint8Array(hexToBytes(signed))
//!    sessionBuilder = sessionBuilder.set_session_sig(signature)
//!    let sessionSk: SessionSk = sessionBuilder.build()
//! ```

//!
//! See [SessionSk] and [SessionSkBuilder] for details.

use std::str::FromStr;

use rings_derive::wasm_export;
use serde::Deserialize;
use serde::Serialize;

use crate::consts::DEFAULT_SESSION_TTL_MS;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::utils;

fn pack_session(session_id: Did, ts_ms: u128, ttl_ms: u64) -> String {
    format!("{}\n{}\n{}", session_id, ts_ms, ttl_ms)
}

/// SessionSkBuilder is used to build a [SessionSk].
///
/// Firstly, you need to provide the account's entity and type to `new` method.
/// Then you can call `pack_session` to get the session dump for signing.
/// After signing, you can call `sig` to set the signature back to builder.
/// Finally, you can call `build` to get the [SessionSk].
#[wasm_export]
pub struct SessionSkBuilder {
    sk: SecretKey,
    /// Account of session.
    account_entity: String,
    /// Account of session.
    account_type: String,
    /// Session's lifetime
    ttl_ms: u64,
    /// Timestamp when session created
    ts_ms: u128,
    /// Signature of session
    sig: Vec<u8>,
}

/// SessionSk holds the [Session] and its session private key.
/// To prove that the message was sent by the [Account] of [Session],
/// we need to attach session and the signature signed by sk to the payload.
///
/// SessionSk provide a `session` method to clone the session.
/// SessionSk also provide `sign` method to sign a message.
///
/// To verify the session, use `verify_self()` method of [Session].
/// To verify a message, use `verify(msg, sig)` method of [Session].
#[wasm_export]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct SessionSk {
    /// Session
    session: Session,
    /// The private key of session. Used for signing and decrypting.
    sk: SecretKey,
}

/// Session is used to verify the message.
/// It's serializable and can be attached to the message payload.
///
/// To verify the session is provided by the account, use session.verify_self().
/// To verify the message, use session.verify(msg, sig).
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct Session {
    /// Did of session, this is hash of sessionPk
    session_id: Did,
    /// Account of session
    account: Account,
    /// Session's lifetime
    ttl_ms: u64,
    /// Timestamp when session created
    ts_ms: u128,
    /// Signature to verify that the session was signed by the account.
    sig: Vec<u8>,
}

/// We will support as many protocols/algorithms as possible.
/// Currently, it comprises Secp256k1, EIP191, BIP137, and Ed25519.
/// We welcome any issues and PRs for additional implementations.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum Account {
    /// ecdsa
    Secp256k1(Did),
    /// ref: <https://eips.ethereum.org/EIPS/eip-191>
    EIP191(Did),
    /// bitcoin bip137 ref: <https://github.com/bitcoin/bips/blob/master/bip-0137.mediawiki>
    BIP137(Did),
    /// ed25519
    Ed25519(PublicKey),
}

impl TryFrom<(String, String)> for Account {
    type Error = Error;

    fn try_from((account_entity, account_type): (String, String)) -> Result<Self> {
        match account_type.as_str() {
            "secp256k1" => Ok(Account::Secp256k1(Did::from_str(&account_entity)?)),
            "eip191" => Ok(Account::EIP191(Did::from_str(&account_entity)?)),
            "bip137" => Ok(Account::BIP137(Did::from_str(&account_entity)?)),
            "ed25519" => Ok(Account::Ed25519(PublicKey::try_from_b58t(&account_entity)?)),
            _ => Err(Error::UnknownAccount),
        }
    }
}

// A SessionSk can be converted to a string using JSON and then encoded with base58.
// To load the SessionSk from a string, use `SessionSk::from_str`.
impl FromStr for SessionSk {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = base58_monero::decode_check(s).map_err(|_| Error::Decode)?;
        let session_sk: SessionSk = serde_json::from_slice(&s).map_err(Error::Deserialize)?;
        Ok(session_sk)
    }
}

#[wasm_export]
impl SessionSkBuilder {
    /// Create a new SessionSkBuilder.
    /// The "account_type" is lower case of [Account] variant.
    /// The "account_entity" refers to the entity that is encapsulated by the [Account] variant, in string format.
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

    /// This is a helper method to let user know if the account params is valid.
    pub fn validate_account(&self) -> bool {
        Account::try_from((self.account_entity.clone(), self.account_type.clone()))
            .map_err(|e| {
                tracing::warn!("validate_account error: {:?}", e);
                e
            })
            .is_ok()
    }

    /// Construct unsigned_info string for signing.
    pub fn unsigned_proof(&self) -> String {
        pack_session(self.sk.address().into(), self.ts_ms, self.ttl_ms)
    }

    /// Set the signature of session that signed by account.
    pub fn set_session_sig(mut self, sig: Vec<u8>) -> Self {
        self.sig = sig;
        self
    }

    /// Set the lifetime of session.
    pub fn set_ttl(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Build the [SessionSk].
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

        Ok(SessionSk {
            session,
            sk: self.sk,
        })
    }
}

impl Session {
    /// Pack the session into a string for verification or public key recovery.
    pub fn pack(&self) -> Vec<u8> {
        pack_session(self.session_id, self.ts_ms, self.ttl_ms)
            .as_bytes()
            .to_vec()
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

        let auth_bytes = self.pack();

        if !(match self.account {
            Account::Secp256k1(did) => {
                signers::secp256k1::verify(&auth_bytes, &did.into(), &self.sig)
            }
            Account::EIP191(did) => signers::eip191::verify(&auth_bytes, &did.into(), &self.sig),
            Account::BIP137(did) => signers::bip137::verify(&auth_bytes, &did.into(), &self.sig),
            Account::Ed25519(pk) => {
                signers::ed25519::verify(&auth_bytes, &pk.address(), &self.sig, pk)
            }
        }) {
            return Err(Error::VerifySignatureFailed);
        }

        Ok(())
    }

    /// Verify message.
    pub fn verify(&self, msg: &[u8], sig: impl AsRef<[u8]>) -> Result<()> {
        self.verify_self()?;
        if !signers::secp256k1::verify(msg, &self.session_id, sig) {
            return Err(Error::VerifySignatureFailed);
        }
        Ok(())
    }

    /// Get public key from session for encryption.
    pub fn account_pubkey(&self) -> Result<PublicKey> {
        let auth_bytes = self.pack();
        match self.account {
            Account::Secp256k1(_) => signers::secp256k1::recover(&auth_bytes, &self.sig),
            Account::BIP137(_) => signers::bip137::recover(&auth_bytes, &self.sig),
            Account::EIP191(_) => signers::eip191::recover(&auth_bytes, &self.sig),
            Account::Ed25519(pk) => Ok(pk),
        }
    }

    /// Get account did.
    pub fn account_did(&self) -> Did {
        match self.account {
            Account::Secp256k1(did) => did,
            Account::BIP137(did) => did,
            Account::EIP191(did) => did,
            Account::Ed25519(pk) => pk.address().into(),
        }
    }
}

impl SessionSk {
    /// Generate Session with private key.
    /// Only use it for unittest.
    pub fn new_with_seckey(key: &SecretKey) -> Result<Self> {
        let account_entity = Did::from(key.address()).to_string();
        let account_type = "secp256k1".to_string();

        let mut builder = SessionSkBuilder::new(account_entity, account_type);

        let sig = key.sign(&builder.unsigned_proof());
        builder = builder.set_session_sig(sig.to_vec());

        builder.build()
    }

    /// Get session from SessionSk.
    pub fn session(&self) -> Session {
        self.session.clone()
    }

    /// Sign message with session.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        let key = self.sk;
        let h = keccak256(msg);
        Ok(signers::secp256k1::sign(key, &h).to_vec())
    }

    /// Get account did from session.
    pub fn account_did(&self) -> Did {
        self.session.account_did()
    }

    /// Dump session_sk to string, allowing user to save it in a config file.
    /// It can be restored using `SessionSk::from_str`.
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
        let sm = SessionSk::new_with_seckey(&key).unwrap();
        let session = sm.session();
        assert!(session.verify_self().is_ok());
    }

    #[test]
    pub fn test_account_pubkey() {
        let key = SecretKey::random();
        let sm = SessionSk::new_with_seckey(&key).unwrap();
        let session = sm.session();
        let pubkey = session.account_pubkey().unwrap();
        assert_eq!(key.pubkey(), pubkey);
    }

    #[test]
    pub fn test_dump_restore() {
        let key = SecretKey::random();
        let sm = SessionSk::new_with_seckey(&key).unwrap();
        let dump = sm.dump().unwrap();
        let sm2 = SessionSk::from_str(&dump).unwrap();
        assert_eq!(sm, sm2);
    }
}
