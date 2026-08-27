#[cfg(test)]
use std::fmt::Debug;
use std::ops::Add;
use std::ops::Neg;
use std::ops::Sub;

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
