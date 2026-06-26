#![warn(missing_docs)]

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
//! [`Add<Output = Self>`], [`Neg<Output = Self>`], or [`Mul<Output = Self>`].
//! They cannot prove associativity, commutativity, distributivity, identity, or
//! inverse laws. Implementing one of these traits is therefore a proof
//! obligation: the implementation asserts the law, and law tests witness the
//! assertion on representative samples.
//!
//! The trait tower is intentionally split into additive and multiplicative
//! towers:
//!
//! - [`AdditiveMagma`] -> [`AdditiveSemigroup`] -> [`AdditiveMonoid`] ->
//!   [`AdditiveGroup`] -> [`AbelianGroup`].
//! - [`MultiplicativeMagma`] -> [`MultiplicativeSemigroup`] ->
//!   [`MultiplicativeMonoid`].
//! - [`Ring`] combines an additive abelian group with a multiplicative monoid.
//! - [`Field`] is a ring whose non-zero elements have multiplicative inverses.
//! - [`Module`] is a right scalar action of a ring on an abelian group.
//!
//! ## Rings DHT
//!
//! [`crate::dht::Did`] is the carrier for Chord identifier arithmetic. It is an
//! [`AbelianGroup`] under addition in `Z / 2^160`, which is exactly the
//! operation used for clockwise offsets, biased ordering, finger targets, and
//! affine replica placement. It intentionally does not implement [`Ring`]:
//! Chord does not use identifier multiplication as a protocol operation, so the
//! public model should not expose it.
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

#[cfg(test)]
use std::fmt::Debug;
use std::ops::Add;
use std::ops::Mul;
use std::ops::Neg;
use std::ops::Sub;

/// Additive identity for an additive carrier.
///
/// Implement this only for a type whose `zero()` value is the neutral element
/// for its [`Add`] operation. `is_zero` must recognize exactly that same value;
/// algorithms use it as a semantic predicate, not as an encoding shortcut.
///
/// Law: `a + zero() == a` and `zero() + a == a`.
pub trait Zero: Sized {
    /// Return the additive identity.
    fn zero() -> Self;

    /// Return whether this value is the additive identity.
    fn is_zero(&self) -> bool;
}

/// Multiplicative identity for a multiplicative carrier.
///
/// Implement this only for a type whose `one()` value is the neutral element
/// for its [`Mul`] operation.
///
/// Law: `a * one() == a` and `one() * a == a`.
pub trait One: Sized {
    /// Return the multiplicative identity.
    fn one() -> Self;
}

/// Magma under addition.
///
/// This is the first additive layer. It states that `+` is a total operation on
/// the carrier: adding two valid values returns another valid value of the same
/// carrier.
///
/// Law: addition is closed over `Self`.
pub trait AdditiveMagma: Sized + Add<Self, Output = Self> {}

/// Semigroup under addition.
///
/// This layer adds associativity to the additive operation.
///
/// Law: addition is associative.
pub trait AdditiveSemigroup: AdditiveMagma {}

/// Monoid under addition.
///
/// This layer adds an additive identity to an associative additive operation.
///
/// Law: [`Zero::zero`] is the additive identity.
pub trait AdditiveMonoid: AdditiveSemigroup + Zero {}

/// Group under addition.
///
/// This layer adds additive inverses. Subtraction must be the derived operation
/// `a - b == a + (-b)`, not an unrelated primitive.
///
/// Law: [`Neg`] returns the additive inverse.
///
/// Law: [`Sub`] is addition with the additive inverse.
pub trait AdditiveGroup: AdditiveMonoid + Neg<Output = Self> + Sub<Self, Output = Self> {}

/// Abelian group under addition.
///
/// This is the additive structure used by Chord identifiers and elliptic-curve
/// points. It adds commutativity to [`AdditiveGroup`].
///
/// Law: addition is commutative.
pub trait AbelianGroup: AdditiveGroup {}

/// Magma under multiplication.
///
/// This is the first multiplicative layer. It states that `*` is a total
/// operation on the carrier.
///
/// Law: multiplication is closed over `Self`.
pub trait MultiplicativeMagma: Sized + Mul<Self, Output = Self> {}

/// Semigroup under multiplication.
///
/// This layer adds associativity to the multiplicative operation.
///
/// Law: multiplication is associative.
pub trait MultiplicativeSemigroup: MultiplicativeMagma {}

/// Monoid under multiplication.
///
/// This layer adds a multiplicative identity to an associative multiplicative
/// operation.
///
/// Law: [`One::one`] is the multiplicative identity.
pub trait MultiplicativeMonoid: MultiplicativeSemigroup + One {}

/// Unital commutative ring.
///
/// `Ring` is a capability boundary: implement it only when both addition and
/// multiplication are semantic operations of the domain type. A type may have a
/// mathematically possible multiplication and still not implement `Ring` when
/// that operation is outside the protocol model.
///
/// Law: the implementor is an [`AbelianGroup`] under addition.
///
/// Law: the implementor is a [`MultiplicativeMonoid`] under multiplication.
///
/// Law: multiplication is commutative.
///
/// Law: multiplication distributes over addition.
pub trait Ring: AbelianGroup + MultiplicativeMonoid {}

/// Field.
///
/// A field is a ring whose non-zero values form a multiplicative group.
/// `try_inverse` is fallible only because zero has no multiplicative inverse;
/// returning `None` for a non-zero value violates the trait law.
///
/// Law: non-zero values have a multiplicative inverse.
pub trait Field: Ring {
    /// Return the multiplicative inverse.
    ///
    /// Pre: `self != zero()`.
    /// Post: `self * self.try_inverse()? == one()`.
    fn try_inverse(&self) -> Option<Self>;
}

/// Right scalar action of a ring on an abelian group.
///
/// `Module<Scalar>` is parameterized by the scalar carrier. The element carrier
/// is `Self`, and the scalar action is expressed by `Self: Mul<Scalar>`. In this
/// crate that matches elliptic-curve notation as `point * scalar`.
///
/// A left action would be a different Rust operation shape,
/// `Scalar: Mul<Self>`. Do not implement this trait for a left-only action by
/// swapping argument meaning in the implementation.
///
/// Law: `a * (s + t) == a * s + a * t`.
///
/// Law: `(a + b) * s == a * s + b * s`.
///
/// Law: `a * (s * t) == (a * s) * t`.
///
/// Law: `a * one() == a`.
pub trait Module<Scalar>: AbelianGroup + Mul<Scalar, Output = Self>
where Scalar: Ring
{
}

/// Assert the abelian-group laws for a representative finite sample.
///
/// This helper checks identity, inverse, involutive negation, commutativity, and
/// associativity over `values`. It is a shared test witness for implementations
/// of [`AbelianGroup`]; it does not replace the implementor's obligation to
/// explain why the laws hold for the whole carrier.
#[cfg(test)]
pub fn assert_abelian_group_laws<T>(values: &[T])
where T: AbelianGroup + Clone + Eq + Debug {
    for a in values {
        assert_eq!(a.clone() + T::zero(), *a);
        assert_eq!(T::zero() + a.clone(), *a);
        assert_eq!(a.clone() + (-a.clone()), T::zero());
        assert_eq!((-a.clone()) + a.clone(), T::zero());
        assert_eq!(-(-a.clone()), *a);

        for b in values {
            let lhs = a.clone() + b.clone();
            let rhs = b.clone() + a.clone();
            assert_eq!(lhs, rhs);

            for c in values {
                let lhs = (a.clone() + b.clone()) + c.clone();
                let rhs = a.clone() + (b.clone() + c.clone());
                assert_eq!(lhs, rhs);
            }
        }
    }
}

/// Assert the commutative-ring laws for a representative finite sample.
///
/// This helper first checks the additive abelian-group laws, then checks
/// multiplicative identity, multiplicative commutativity, multiplicative
/// associativity, and left distributivity over `values`. Because multiplication
/// is required to be commutative, left distributivity witnesses right
/// distributivity on the same sample.
#[cfg(test)]
pub fn assert_ring_laws<T>(values: &[T])
where T: Ring + Clone + Eq + Debug {
    assert_abelian_group_laws(values);

    for a in values {
        assert_eq!(a.clone() * T::one(), *a);
        assert_eq!(T::one() * a.clone(), *a);

        for b in values {
            let lhs = a.clone() * b.clone();
            let rhs = b.clone() * a.clone();
            assert_eq!(lhs, rhs);

            for c in values {
                let lhs = (a.clone() * b.clone()) * c.clone();
                let rhs = a.clone() * (b.clone() * c.clone());
                assert_eq!(lhs, rhs);

                let lhs = a.clone() * (b.clone() + c.clone());
                let rhs = (a.clone() * b.clone()) + (a.clone() * c.clone());
                assert_eq!(lhs, rhs);
            }
        }
    }
}

/// Assert the field inverse laws for a representative finite sample.
///
/// This helper first checks the ring laws. It then checks that zero has no
/// inverse and every sampled non-zero value has a two-sided inverse.
#[cfg(test)]
pub fn assert_field_laws<T>(values: &[T])
where T: Field + Clone + Eq + Debug {
    assert_ring_laws(values);

    for a in values {
        if a.is_zero() {
            assert_eq!(a.try_inverse(), None);
            continue;
        }

        let Some(inverse) = a.try_inverse() else {
            panic!("non-zero field element has no inverse");
        };
        assert_eq!(a.clone() * inverse.clone(), T::one());
        assert_eq!(inverse * a.clone(), T::one());
    }
}

/// Assert module laws for representative scalar and element samples.
///
/// This helper checks the scalar ring laws and the right scalar-action laws. It
/// is written for right actions: `element * scalar`.
#[cfg(test)]
pub fn assert_module_laws<Scalar, Element>(scalars: &[Scalar], elements: &[Element])
where
    Scalar: Ring + Clone + Eq + Debug,
    Element: Module<Scalar> + Clone + Eq + Debug,
{
    assert_ring_laws(scalars);
    assert_module_action_laws(scalars, elements);
}

/// Assert right scalar-action laws for representative samples.
///
/// This helper assumes the caller has already checked the scalar carrier laws.
/// It still checks the element abelian-group laws because module elements carry
/// their own additive group structure. Use it when a test has already run a
/// stricter scalar law helper, such as [`assert_field_laws`], and only needs the
/// module action witness afterward.
#[cfg(test)]
pub fn assert_module_action_laws<Scalar, Element>(scalars: &[Scalar], elements: &[Element])
where
    Scalar: Ring + Clone + Eq + Debug,
    Element: Module<Scalar> + Clone + Eq + Debug,
{
    assert_abelian_group_laws(elements);

    for s in scalars {
        for t in scalars {
            for a in elements {
                let lhs = a.clone() * (s.clone() + t.clone());
                let rhs = (a.clone() * s.clone()) + (a.clone() * t.clone());
                assert_eq!(lhs, rhs);

                let lhs = a.clone() * (s.clone() * t.clone());
                let rhs = (a.clone() * s.clone()) * t.clone();
                assert_eq!(lhs, rhs);

                for b in elements {
                    let lhs = (a.clone() + b.clone()) * s.clone();
                    let rhs = (a.clone() * s.clone()) + (b.clone() * s.clone());
                    assert_eq!(lhs, rhs);
                }
            }
        }

        for a in elements {
            assert_eq!(a.clone() * Scalar::one(), *a);
        }
    }
}
