#![deny(missing_docs)]
//! Understanding Abstract Account and Session keypair in Rings Network
//!
//! Rings network offers a unique mechanism to bolster security and abstract the user's keypair
//! through a feature known as session keypair. The fundamental concept is signing a generated
//! keypair with a time period `{ts, ttl}` by a user without giving this program access to the
//! user's private key. This can be conceptualized as a contract stating, "I delegate to a keypair
//! for the time period `{ts, ttl}`."
//!
//! In our terminology:
//! - `I` is [`Account`].
//! - `keypair` is [`SessionSk`].
//! - The time period `{ts, ttl}` is in the session held by [`SessionSk`].
//!
//! The following is an example to build a [`SessionSk`] in Rust and use it to sign a message.
//! It is not necessary to construct a secret key in Rust. A user may manually set
//! `account_type`, `account_entity`, and `session_sig` instead of providing a secret key.
//! ```
//! use rings_core::dht::Did;
//! use rings_core::session::SessionSkBuilder;
//!
//! let user_secret_key = rings_core::ecc::SecretKey::random();
//! let user_secret_key_did: Did = user_secret_key.address().into();
//! let account_type = "secp256k1".to_string();
//! let account_entity = user_secret_key_did.to_string();
//!
//! let builder = SessionSkBuilder::new(account_entity, account_type);
//! let unsigned_proof = builder.unsigned_proof();
//! let session_sig = user_secret_key.sign(&unsigned_proof).unwrap().to_vec();
//! let session_sk = builder.set_session_sig(session_sig).build().unwrap();
//!
//! assert_eq!(session_sk.account_did(), user_secret_key_did);
//! assert!(session_sk.session().verify_self().is_ok());
//!
//! let msg = "hello world".as_bytes();
//! let msg_sig = session_sk.sign(msg).unwrap();
//! let msg_session = session_sk.session();
//! assert_eq!(msg_session.account_did(), user_secret_key_did);
//! assert!(msg_session.verify(msg, msg_sig).is_ok());
//! ```
//!
//! [`SessionSkBuilder`] and [`SessionSk`] are exported to WebAssembly environments.

mod account;
mod builder;
mod model;
mod signing_key;

pub use account::Account;
pub use builder::SessionSkBuilder;
pub use model::Session;
pub use signing_key::SessionSk;

#[cfg(test)]
mod test_session;
