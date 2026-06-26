#![warn(missing_docs)]

//! Algebraic operation traits for DHT identifier arithmetic.
//!
//! Chord models identifiers as points on a finite circle. For Rings, that
//! circle is `Z / 2^160`, encoded by the domain type [`crate::dht::did::Did`].
//! This module names the algebraic operations without introducing another
//! carrier type: the implementing domain type is the carrier.
//!
//! The DHT uses the additive part of the ring for clockwise offsets, interval
//! biasing, finger targets, and replica placement. Arbitrary identifier
//! multiplication is intentionally not implemented for [`crate::dht::did::Did`]
//! because no Chord state transition uses it.

use std::ops::Add;
use std::ops::Mul;
use std::ops::Neg;
use std::ops::Sub;

use num_traits::One;
use num_traits::Zero;

/// Additive abelian group operations required by Chord identifier arithmetic.
///
/// Invariant: the implementor is the carrier set. Implementing this trait must
/// not require a second isomorphic wrapper around the same values.
///
/// Law: [`Zero::zero`] is the additive identity.
///
/// Law: [`Add`] is closed, associative, and commutative for implementors.
///
/// Law: [`Neg`] returns the additive inverse.
///
/// Law: [`Sub`] is addition with the additive inverse.
pub trait AbelianGroup:
    Sized + Copy + Eq + Add<Self, Output = Self> + Sub<Self, Output = Self> + Neg<Output = Self> + Zero
{
}

/// Commutative ring operations.
///
/// Law: the implementor is an [`AbelianGroup`] under addition.
///
/// Law: [`One::one`] is the multiplicative identity.
///
/// Law: [`Mul`] is closed, associative, and commutative for implementors.
///
/// Law: multiplication distributes over addition.
pub trait Ring: AbelianGroup + Mul<Self, Output = Self> + One {}
