#![deny(missing_docs)]
//! Delegated signing identities in Rings Network
//!
//! A delegator authorizes a generated delegatee key for a bounded period without giving this
//! program access to the delegator's private key. The signed delegation binds the delegatee DID,
//! delegator, creation time, and lifetime.
//!
//! In our terminology:
//! - The authorizing identity is represented by [`Account`].
//! - The authorized signing key is [`DelegateeKey`].
//! - The signed authorization is [`Delegation`].
//!
//! The following is an example to build a [`DelegateeKey`] in Rust and use it to sign a message.
//! It is not necessary to expose the delegator private key to Rings. The caller may sign
//! `delegator_entity`, `delegator_type`, and the generated proof externally.
//! ```
//! use rings_core::delegation::DelegationBuilder;
//! use rings_core::dht::Did;
//!
//! let user_secret_key = rings_core::ecc::SecretKey::random();
//! let user_secret_key_did: Did = user_secret_key.address().into();
//! let delegator_type = "secp256k1".to_string();
//! let delegator_entity = user_secret_key_did.to_string();
//!
//! let builder = DelegationBuilder::new(delegator_entity, delegator_type);
//! let unsigned_proof = builder.unsigned_proof();
//! let delegator_signature = user_secret_key.sign(&unsigned_proof).unwrap().to_vec();
//! let delegatee_key = builder
//!     .set_delegator_signature(delegator_signature)
//!     .build()
//!     .unwrap();
//!
//! assert_eq!(delegatee_key.delegator_did(), user_secret_key_did);
//! assert!(delegatee_key
//!     .delegation()
//!     .verify_delegator_authorization()
//!     .is_ok());
//!
//! let msg = "hello world".as_bytes();
//! let msg_sig = delegatee_key.sign(msg).unwrap();
//! let delegation = delegatee_key.delegation();
//! assert_eq!(delegation.delegator_did(), user_secret_key_did);
//! assert!(delegation.verify(msg, msg_sig).is_ok());
//! ```
//!
//! [`DelegationBuilder`] and [`DelegateeKey`] are exported to WebAssembly environments.

mod account;
mod builder;
mod digest;
mod model;
mod signing_key;

pub use account::Account;
pub use builder::DelegationBuilder;
pub use digest::DelegationDigest;
pub use model::Delegation;
pub use signing_key::DelegateeKey;

#[cfg(test)]
mod test_delegation;
