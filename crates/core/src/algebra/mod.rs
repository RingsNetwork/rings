#![deny(missing_docs)]

//! Algebraic structure traits shared by DHT identifiers and elliptic-curve
//! groups.
//!
//! This module names the algebraic structure carried by a domain type. It is
//! deliberately about carriers and operations, not about object hierarchies:
//! the implementing type is the carrier set, and each trait states which
//! operations and laws are part of that type's public model.
//!
//! ## Model Boundary
//!
//! Rust trait bounds can require operation shapes such as
//! [`std::ops::Add`], [`std::ops::Neg`], or [`std::ops::Mul`].
//! They cannot prove associativity, commutativity, distributivity, identity, or
//! inverse laws. Implementing one of these traits is therefore a proof
//! obligation: the implementation asserts the law, and law tests witness the
//! assertion on representative samples.
//!
//! The public surface is intentionally small:
//!
//! - [`Zero`] and [`One`] name additive and multiplicative identities together
//!   with the operations whose identities they are.
//! - [`AbelianGroup`] is the additive structure used by Chord identifiers and
//!   elliptic-curve points.
//! - [`CommutativeRing`] combines an additive abelian group with commutative
//!   multiplication and a multiplicative identity.
//! - [`Field`] is a commutative ring whose non-zero elements have inverses.
//! - [`Module`] is a right scalar action of a commutative ring on an abelian
//!   group.
//! - [`JoinSemilattice`] is the CRDT merge structure used by replicated DHT
//!   state.
//!
//! ## Rings DHT
//!
//! [`crate::dht::Did`] is the carrier for Chord identifier arithmetic. It is an
//! [`AbelianGroup`] under addition in `Z / 2^160`, which is exactly the
//! operation used for clockwise offsets, biased ordering, finger targets, and
//! affine replica placement. It intentionally does not implement
//! [`CommutativeRing`]: Chord does not use identifier multiplication as a
//! protocol operation, so the public model should not expose it.
//!
//! ## Elliptic Curves
//!
//! Curve points are additive abelian groups. Curve scalars are finite fields.
//! A point group with scalar multiplication is modeled as
//! `Point<C>: Module<Scalar<C>>`. The module action is a right action because
//! Rust's operator implementation in this crate is `Point<C> * Scalar<C>`.
//! This keeps cryptographic algorithms phrased in algebraic terms while curve
//! libraries remain adapters behind [`crate::ecc::group::CurveGroup`].
//!
//! ## Law Witnesses
//!
//! The `assert_*_laws` functions are test helpers, not proofs in the type
//! system. They are useful because every implementation can be checked through
//! the same vocabulary, but they remain finite-sample witnesses. A new
//! implementation must still explain why its carrier and operations satisfy the
//! stated laws for all values, usually by delegating to a native finite-field or
//! group implementation with documented semantics.
//!
//! Identities are functions rather than associated constants because several
//! curve adapters obtain identity values through their native libraries.

mod field;
mod group;
mod lattice;
mod module;
mod ring;

#[cfg(test)]
pub use field::assert_field_laws;
pub use field::Field;
#[cfg(test)]
pub use group::assert_abelian_group_laws;
pub use group::AbelianGroup;
pub use group::Zero;
#[cfg(test)]
pub use lattice::assert_join_semilattice_laws;
#[cfg(test)]
pub use lattice::assert_strong_eventual_consistency;
pub use lattice::JoinSemilattice;
#[cfg(test)]
pub use module::assert_module_action_laws;
pub use module::Module;
#[cfg(test)]
pub use ring::assert_commutative_ring_laws;
pub use ring::CommutativeRing;
pub use ring::One;
