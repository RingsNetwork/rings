//! Rings SNARK
//! ===============
//! This implementation is based on NOVA

#![deny(missing_docs)]

/// Circuit adapters for supported proof backends.
pub mod circuit;
/// Error types used by SNARK operations.
pub mod error;
/// Re-exported dependencies and backend types.
pub mod prelude;
/// R1CS loading and parsing utilities.
pub mod r1cs;
/// High-level SNARK proving and verification wrappers.
pub mod snark;
#[cfg(test)]
mod tests;
/// Witness loading and calculation utilities.
pub mod witness;
