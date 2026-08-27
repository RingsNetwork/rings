#[cfg(test)]
use std::fmt::Debug;

#[cfg(test)]
use super::assert_commutative_ring_laws;
use super::CommutativeRing;

/// Field.
///
/// A field is a commutative ring whose non-zero values form a multiplicative
/// group. `try_inverse` is fallible only because zero has no multiplicative
/// inverse; returning `None` for a non-zero value violates the trait law.
///
/// Law: [`super::Zero::zero`] is distinct from [`super::One::one`].
///
/// Law: non-zero values have a multiplicative inverse.
pub trait Field: CommutativeRing {
    /// Return the multiplicative inverse.
    ///
    /// Post: if this returns `Some(inv)`, then `self * inv == one()` and
    /// `inv * self == one()`.
    fn try_inverse(&self) -> Option<Self>;
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
