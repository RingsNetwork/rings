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

#[cfg(test)]
use std::fmt::Debug;
use std::ops::Add;
use std::ops::Mul;
use std::ops::Neg;
use std::ops::Sub;

/// Join-semilattice for state-based CRDT merge.
///
/// `join` returns the least upper bound of two states from the same carrier.
/// Implementors must make this operation inflationary with respect to the
/// carrier's induced partial order `a <= b iff a.join(b) == b`; because Rust
/// does not expose that order here, the obligation is witnessed through the
/// idempotence, commutativity, and associativity laws below.
///
/// The operation takes both arguments by value to model a pure state transition
/// into a canonical least upper bound. Implementations that need an in-place
/// merge can provide that adapter separately and keep this law-facing
/// signature as the common algebraic surface.
///
/// Law: `a.join(a) == a`.
///
/// Law: `a.join(b) == b.join(a)`.
///
/// Law: `a.join(b).join(c) == a.join(b.join(c))`.
pub trait JoinSemilattice: Sized {
    /// Return the least upper bound of `self` and `other`.
    fn join(self, other: Self) -> Self;
}

/// Additive identity for an additive carrier.
///
/// Implement this only for a type whose [`Add`] operation has an identity
/// element. `is_zero` must recognize exactly that same value; algorithms use it
/// as a semantic predicate, not as an encoding shortcut.
///
/// Law: `a + zero() == a` and `zero() + a == a`.
pub trait Zero: Sized + Add<Self, Output = Self> {
    /// Return the additive identity.
    fn zero() -> Self;

    /// Return whether this value is the additive identity.
    fn is_zero(&self) -> bool;
}

/// Multiplicative identity for a multiplicative carrier.
///
/// Implement this only for a type whose [`Mul`] operation has an identity
/// element.
///
/// Law: `a * one() == a` and `one() * a == a`.
pub trait One: Sized + Mul<Self, Output = Self> {
    /// Return the multiplicative identity.
    fn one() -> Self;
}

/// Abelian group under addition.
///
/// This is the additive structure used by Chord identifiers and elliptic-curve
/// points. Subtraction must be the derived operation `a - b == a + (-b)`, not an
/// unrelated primitive.
///
/// Law: addition is associative and commutative.
///
/// Law: [`Zero::zero`] is the additive identity.
///
/// Law: [`Neg`] returns the additive inverse.
///
/// Law: [`Sub`] is addition with the additive inverse.
pub trait AbelianGroup:
    Sized + Add<Self, Output = Self> + Sub<Self, Output = Self> + Neg<Output = Self> + Zero
{
}

/// Unital commutative ring.
///
/// `CommutativeRing` is a capability boundary: implement it only when both
/// addition and multiplication are semantic operations of the domain type. A
/// type may have a mathematically possible multiplication and still not
/// implement `CommutativeRing` when that operation is outside the protocol
/// model.
///
/// Law: the implementor is an [`AbelianGroup`] under addition.
///
/// Law: multiplication is associative and commutative.
///
/// Law: [`One::one`] is the multiplicative identity.
///
/// Law: multiplication distributes over addition.
pub trait CommutativeRing: AbelianGroup + Mul<Self, Output = Self> + One {}

/// Field.
///
/// A field is a commutative ring whose non-zero values form a multiplicative
/// group. `try_inverse` is fallible only because zero has no multiplicative
/// inverse; returning `None` for a non-zero value violates the trait law.
///
/// Law: [`Zero::zero`] is distinct from [`One::one`].
///
/// Law: non-zero values have a multiplicative inverse.
pub trait Field: CommutativeRing {
    /// Return the multiplicative inverse.
    ///
    /// Post: if this returns `Some(inv)`, then `self * inv == one()` and
    /// `inv * self == one()`.
    fn try_inverse(&self) -> Option<Self>;
}

/// Right scalar action of a commutative ring on an abelian group.
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
/// Law: `a * Scalar::one() == a`.
pub trait Module<Scalar>: AbelianGroup + Mul<Scalar, Output = Self>
where Scalar: CommutativeRing
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

/// Assert join-semilattice laws for a representative finite sample.
///
/// This helper checks the state-based CRDT merge laws that imply strong
/// eventual consistency for replicas that observe the same set of deltas in any
/// order, with any duplication.
#[cfg(test)]
pub fn assert_join_semilattice_laws<T>(values: &[T])
where T: JoinSemilattice + Clone + Eq + Debug {
    for a in values {
        assert_eq!(a.clone().join(a.clone()), *a);

        for b in values {
            assert_eq!(a.clone().join(b.clone()), b.clone().join(a.clone()));

            for c in values {
                let lhs = a.clone().join(b.clone()).join(c.clone());
                let rhs = a.clone().join(b.clone().join(c.clone()));
                assert_eq!(lhs, rhs);
            }
        }
    }
}

/// Assert strong eventual consistency for one base state and a finite delta set.
///
/// The witness applies the same deltas in forward, reverse, and duplicated
/// schedules. A lawful join-semilattice must materialize the same least upper
/// bound for each schedule.
#[cfg(test)]
pub fn assert_strong_eventual_consistency<T>(base: T, deltas: &[T])
where T: JoinSemilattice + Clone + Eq + Debug {
    let forward = deltas
        .iter()
        .cloned()
        .fold(base.clone(), JoinSemilattice::join);
    let reverse = deltas
        .iter()
        .rev()
        .cloned()
        .fold(base.clone(), JoinSemilattice::join);
    let duplicated = deltas
        .iter()
        .cloned()
        .chain(deltas.iter().cloned())
        .fold(base, JoinSemilattice::join);

    assert_eq!(forward, reverse);
    assert_eq!(forward, duplicated);
}

/// Assert the commutative-ring laws for a representative finite sample.
///
/// This helper first checks the additive abelian-group laws, then checks
/// multiplicative identity, multiplicative commutativity, multiplicative
/// associativity, and left distributivity over `values`. Because multiplication
/// is required to be commutative, left distributivity witnesses right
/// distributivity on the same sample.
#[cfg(test)]
pub fn assert_commutative_ring_laws<T>(values: &[T])
where T: CommutativeRing + Clone + Eq + Debug {
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
/// This helper first checks the commutative-ring laws and the field
/// non-degeneracy law `zero() != one()`. It then checks that zero has no inverse
/// and every sampled non-zero value has a two-sided inverse.
#[cfg(test)]
pub fn assert_field_laws<T>(values: &[T])
where T: Field + Clone + Eq + Debug {
    assert_commutative_ring_laws(values);
    assert_ne!(T::zero(), T::one());

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
    Scalar: CommutativeRing + Clone + Eq + Debug,
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
