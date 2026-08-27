#[cfg(test)]
use std::fmt::Debug;
use std::ops::Mul;

#[cfg(test)]
use super::assert_abelian_group_laws;
use super::AbelianGroup;

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
