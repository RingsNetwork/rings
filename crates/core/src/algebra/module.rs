#[cfg(test)]
use std::fmt::Debug;
use std::ops::Mul;

#[cfg(test)]
use super::assert_abelian_group_laws;
use super::AbelianGroup;
use super::CommutativeRing;

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
