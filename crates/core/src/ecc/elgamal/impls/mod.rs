//! Curve-specific plaintext/ciphertext adapters for ElGamal.
//!
//! Raw ElGamal is implemented once in [`crate::ecc::elgamal::ElGamal`] over any
//! group that implements the algebraic group traits. Modules under `impls`
//! contain only curve-specific encoding and serialization glue, such as
//! reversible mapping between application messages and curve points.
//!
//! This layout keeps algorithm logic and encoding policy separate. The
//! algorithm works over group elements; an adapter decides how a domain object
//! such as a UTF-8 string becomes group elements and how ciphertext points are
//! serialized for compatibility with existing callers.
//!
//! The generic algorithm is available for every curve group exported by
//! [`crate::ecc::group`]. A module appears here only when a curve also needs a
//! domain-specific plaintext mapping or wire-format compatibility layer.

pub mod secp256k1;
