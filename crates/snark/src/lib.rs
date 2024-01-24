//! Rings SNARK
//! ===============
//! This implementation is based on NOVA

#![warn(missing_docs)]

pub mod circuit;
pub mod error;
pub mod prelude;
pub mod r1cs;
pub mod snark;
#[cfg(test)]
mod tests;
pub mod witness;
