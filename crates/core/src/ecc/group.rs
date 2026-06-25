//! Finite-group abstractions and elliptic-curve group adapters.
//!
//! The algebraic layer in this module is intentionally smaller than any
//! concrete elliptic-curve library API:
//!
//! - [`GroupOps`] models a finite additive group: identity, addition,
//!   inverse, equality, and scalar action on group elements.
//! - [`CyclicGroup`] adds a distinguished generator `g`; every element in the
//!   subgroup used by the cryptographic algorithm is representable as `xg`.
//! - [`CryptographicGroup`] adds explicit non-zero scalar sampling. This is not
//!   a group axiom; it belongs to cryptographic algorithms such as ElGamal key
//!   generation and encryption randomness.
//! - [`CurveGroup`] is the adapter boundary for elliptic-curve libraries. A
//!   marker type such as [`Secp256k1`] or [`Bls12381G1`] supplies native point
//!   and scalar types, while [`Point<C>`], [`Scalar<C>`], and [`Group<C>`]
//!   expose them through the same algebraic interface.
//!
//! All operations are written in additive notation. For a scalar `x` and
//! generator `g`, `xg` is represented by [`CyclicGroup::generator_mul`].
//!
//! This split gives the rest of the cryptographic code one stable vocabulary:
//! algorithms depend on finite-group laws, not on a concrete crate such as
//! `libsecp256k1`, `p256`, `arkworks`, or `curve25519-dalek`. Adding a curve is
//! therefore a matter of implementing the adapter boundary once; algorithms
//! such as ElGamal do not need per-curve branches.

use std::cell::RefCell;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::ops::Add;
use std::ops::Mul;
use std::ops::Neg;

use ark_bls12_381::Fr as Bls12381ScalarField;
use ark_bls12_381::G1Projective;
use ark_ec::Group as ArkGroup;
use ark_ff::Zero;
use ark_std::UniformRand;
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::scalar::Scalar as Ristretto255ScalarField;
use curve25519_dalek::traits::Identity as _;
use elliptic_curve::ff::Field as _;
use libsecp256k1::curve::Affine;
use libsecp256k1::curve::ECMultContext;
use libsecp256k1::curve::ECMultGenContext;
use libsecp256k1::curve::Jacobian;
use libsecp256k1::curve::Scalar as SecpK1FieldScalar;
use p256::ProjectivePoint;
use p256::Scalar as Secp256r1ScalarField;
use rand::RngCore;
use rand::SeedableRng;
use rand_hc::Hc128Rng;

use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// Additive group abstraction.
///
/// Implementors model one finite additive group. The expected laws are:
///
/// - `add_ref` is associative and commutative.
/// - `identity` is the neutral element.
/// - `neg_ref(a)` is the inverse of `a`.
/// - `mul_ref(a, x)` is the scalar action of `x` on `a`.
///
/// The std `Add`, `Neg`, and `Mul` bounds keep the wrapped element convenient to
/// use in tests and callers. Algorithms should prefer the reference-based
/// methods below so a curve adapter can call its native borrowed operations
/// without cloning large point representations.
pub trait GroupOps {
    /// Group element type.
    type Element: Clone
        + Add<Self::Element, Output = Self::Element>
        + Neg<Output = Self::Element>
        + Mul<Self::Scalar, Output = Self::Element>;
    /// Scalar type acting on the group.
    type Scalar: Clone;

    /// Additive identity element.
    fn identity() -> Self::Element;

    /// Group addition on borrowed elements.
    fn add_ref(lhs: &Self::Element, rhs: &Self::Element) -> Self::Element {
        lhs.clone() + rhs.clone()
    }

    /// Group inverse on a borrowed element.
    fn neg_ref(element: &Self::Element) -> Self::Element {
        -element.clone()
    }

    /// Scalar action on a borrowed element and scalar.
    fn mul_ref(element: &Self::Element, scalar: &Self::Scalar) -> Self::Element {
        element.clone() * scalar.clone()
    }
}

/// Cyclic group abstraction with a distinguished generator.
pub trait CyclicGroup: GroupOps {
    /// Distinguished group generator.
    fn generator() -> Self::Element;

    /// Multiply the distinguished generator by a scalar.
    fn generator_mul(scalar: Self::Scalar) -> Self::Element {
        Self::generator() * scalar
    }
}

/// Cryptographic group extension that can sample non-zero scalars.
pub trait CryptographicGroup: CyclicGroup {
    /// Generate a fresh non-zero random scalar from an explicit RNG.
    fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar;

    /// Generate a fresh non-zero random scalar from the default thread-local RNG.
    fn random_scalar() -> Self::Scalar {
        with_group_rng(|rng| Self::random_scalar_with_rng(rng))
    }
}

/// Curve-specific group operations implemented by curve markers.
///
/// This trait is the adapter boundary between abstract finite-group algorithms
/// and concrete elliptic-curve libraries. For a marker `C`, the native
/// `Point` type must represent elements of one finite abelian group, and
/// `Scalar` must represent the scalar ring used for the group action.
///
/// Implementors must preserve the following laws after accounting for native
/// representation details such as projective coordinates:
///
/// - `identity` is a left and right identity for `add`.
/// - `add` is associative and commutative over the represented group.
/// - `neg(p)` is the additive inverse of `p`.
/// - `eq` is an equivalence relation over group elements, not merely raw
///   representation equality; equivalent projective representatives must
///   compare equal.
/// - `eq` is compatible with `add`, `neg`, `mul`, and `generator_mul`.
/// - `generator_mul(s)` is equivalent to `mul(generator(), s)`.
///
/// Because this abstraction keeps `Scalar` opaque and only requires `Clone`,
/// scalar-ring laws such as distributivity cannot be stated directly in the
/// trait bounds. They are still semantic requirements of any cryptographic
/// curve adapter.
pub trait CurveGroup {
    /// Native point representation for this curve group.
    type Point: Clone;
    /// Native scalar representation for this curve group.
    type Scalar: Clone;

    /// Additive identity.
    fn identity() -> Self::Point;

    /// Distinguished generator.
    fn generator() -> Self::Point;

    /// Multiply the distinguished generator by a scalar.
    fn generator_mul(scalar: &Self::Scalar) -> Self::Point {
        let generator = Self::generator();
        Self::mul(&generator, scalar)
    }

    /// Group addition.
    fn add(lhs: &Self::Point, rhs: &Self::Point) -> Self::Point;

    /// Group inverse.
    fn neg(point: &Self::Point) -> Self::Point;

    /// Scalar multiplication.
    fn mul(point: &Self::Point, scalar: &Self::Scalar) -> Self::Point;

    /// Element equality.
    fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool;
}

/// Curve adapter extension that can sample non-zero scalars for cryptography.
pub trait CurveScalarSampler: CurveGroup {
    /// Generate a fresh non-zero random scalar from an explicit RNG.
    fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar;

    /// Generate a fresh non-zero random scalar from the default thread-local RNG.
    fn random_scalar() -> Self::Scalar {
        with_group_rng(|rng| Self::random_scalar_with_rng(rng))
    }
}

/// Generic group element for curve marker `C`.
#[derive(Debug)]
pub struct Point<C: CurveGroup> {
    inner: C::Point,
}

/// Generic scalar for curve marker `C`.
#[derive(Debug)]
pub struct Scalar<C: CurveGroup> {
    inner: C::Scalar,
}

/// Generic group for curve marker `C`.
#[derive(Debug)]
pub struct Group<C: CurveGroup>(PhantomData<C>);

/// secp256k1 curve marker.
#[derive(Debug)]
pub struct Secp256k1;

/// secp256r1/P-256 curve marker.
#[derive(Debug)]
pub struct Secp256r1;

/// BLS12-381 G1 curve marker.
#[derive(Debug)]
pub struct Bls12381G1;

/// Ristretto255 group marker.
#[derive(Debug)]
pub struct Ristretto255;

/// Ristretto255 group.
pub type Ristretto255Group = Group<Ristretto255>;

thread_local! {
    static SECP256K1_GENERATOR: Point<Secp256k1> = Point::new(secp256k1_generator());
    static SECP256K1_GEN_CONTEXT: Box<ECMultGenContext> = ECMultGenContext::new_boxed();
    static SECP256K1_MUL_CONTEXT: Box<ECMultContext> = ECMultContext::new_boxed();
    static GROUP_RNG: RefCell<Hc128Rng> = RefCell::new(Hc128Rng::from_entropy());
}

impl<C: CurveGroup> Point<C> {
    /// Build a group element from the curve-native point type.
    pub fn new(inner: C::Point) -> Self {
        Self { inner }
    }

    /// Borrow the curve-native point type.
    pub fn as_inner(&self) -> &C::Point {
        &self.inner
    }

    /// Unwrap into the curve-native point type.
    pub fn into_inner(self) -> C::Point {
        self.inner
    }
}

impl<C: CurveGroup> Scalar<C> {
    /// Build a scalar from the curve-native scalar type.
    pub fn new(inner: C::Scalar) -> Self {
        Self { inner }
    }

    /// Borrow the curve-native scalar type.
    pub fn as_inner(&self) -> &C::Scalar {
        &self.inner
    }

    /// Unwrap into the curve-native scalar type.
    pub fn into_inner(self) -> C::Scalar {
        self.inner
    }
}

impl<C: CurveGroup> Clone for Point<C> {
    fn clone(&self) -> Self {
        Self::new(self.inner.clone())
    }
}

impl<C> Copy for Point<C>
where
    C: CurveGroup,
    C::Point: Copy,
{
}

impl<C: CurveGroup> Clone for Scalar<C> {
    fn clone(&self) -> Self {
        Self::new(self.inner.clone())
    }
}

impl<C> Copy for Scalar<C>
where
    C: CurveGroup,
    C::Scalar: Copy,
{
}

impl<C: CurveGroup> GroupOps for Group<C> {
    type Element = Point<C>;
    type Scalar = Scalar<C>;

    fn identity() -> Self::Element {
        Point::new(C::identity())
    }

    fn add_ref(lhs: &Self::Element, rhs: &Self::Element) -> Self::Element {
        Point::new(C::add(&lhs.inner, &rhs.inner))
    }

    fn neg_ref(element: &Self::Element) -> Self::Element {
        Point::new(C::neg(&element.inner))
    }

    fn mul_ref(element: &Self::Element, scalar: &Self::Scalar) -> Self::Element {
        Point::new(C::mul(&element.inner, &scalar.inner))
    }
}

impl<C: CurveGroup> CyclicGroup for Group<C> {
    fn generator() -> Self::Element {
        Point::new(C::generator())
    }

    fn generator_mul(scalar: Self::Scalar) -> Self::Element {
        Point::new(C::generator_mul(&scalar.inner))
    }
}

impl<C: CurveScalarSampler> CryptographicGroup for Group<C> {
    fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar {
        Scalar::new(C::random_scalar_with_rng(rng))
    }
}

impl<C: CurveGroup> Add for Point<C> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self::new(C::add(&self.inner, &rhs.inner))
    }
}

impl<C: CurveGroup> Neg for Point<C> {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self::new(C::neg(&self.inner))
    }
}

impl<C: CurveGroup> Mul<Scalar<C>> for Point<C> {
    type Output = Self;

    fn mul(self, rhs: Scalar<C>) -> Self::Output {
        Self::new(C::mul(&self.inner, &rhs.inner))
    }
}

impl<C: CurveGroup> PartialEq for Point<C> {
    fn eq(&self, other: &Self) -> bool {
        C::eq(&self.inner, &other.inner)
    }
}

impl<C: CurveGroup> Eq for Point<C> {}

// The simple curve adapters below all have the same shape: the native library
// already exposes identity, generator, addition, negation, scalar
// multiplication, equality, and point conversion. Keeping that pattern in one
// macro makes each supported curve a short declaration while preserving the
// explicit algebraic operations at the trait boundary. secp256k1 remains
// hand-written because it needs precomputed multiplication contexts and
// explicit infinity handling from `libsecp256k1`.
macro_rules! impl_curve_group_adapter {
    (
        $curve:ty {
            point: $point:ty,
            scalar: $scalar:ty,
            identity: $identity:expr,
            generator: $generator:expr,
            random_scalar: |$rng:ident| $random_scalar:block,
            add: $add:expr,
            neg: $neg:expr,
            mul: $mul:expr,
            eq: $eq:expr $(,)?
        }
    ) => {
        impl CurveGroup for $curve {
            type Point = $point;
            type Scalar = $scalar;

            fn identity() -> Self::Point {
                $identity
            }

            fn generator() -> Self::Point {
                $generator
            }

            fn add(lhs: &Self::Point, rhs: &Self::Point) -> Self::Point {
                ($add)(lhs, rhs)
            }

            fn neg(point: &Self::Point) -> Self::Point {
                ($neg)(point)
            }

            fn mul(point: &Self::Point, scalar: &Self::Scalar) -> Self::Point {
                ($mul)(point, scalar)
            }

            fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool {
                ($eq)(lhs, rhs)
            }
        }

        impl CurveScalarSampler for $curve {
            fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar {
                let $rng = rng;
                $random_scalar
            }
        }

        impl From<$point> for Point<$curve> {
            fn from(point: $point) -> Self {
                Self::new(point)
            }
        }

        impl From<Point<$curve>> for $point {
            fn from(point: Point<$curve>) -> Self {
                point.inner
            }
        }
    };
}

impl CurveGroup for Secp256k1 {
    type Point = Jacobian;
    type Scalar = SecpK1FieldScalar;

    fn identity() -> Self::Point {
        secp256k1_identity()
    }

    fn generator() -> Self::Point {
        SECP256K1_GENERATOR.with(|generator| generator.inner)
    }

    fn generator_mul(scalar: &Self::Scalar) -> Self::Point {
        SECP256K1_GEN_CONTEXT.with(|context| {
            let mut result = Jacobian::default();
            context.ecmult_gen(&mut result, scalar);
            result
        })
    }

    fn add(lhs: &Self::Point, rhs: &Self::Point) -> Self::Point {
        lhs.add_var(rhs, None)
    }

    fn neg(point: &Self::Point) -> Self::Point {
        point.neg()
    }

    fn mul(point: &Self::Point, scalar: &Self::Scalar) -> Self::Point {
        if point.is_infinity() {
            return secp256k1_identity();
        }
        SECP256K1_MUL_CONTEXT.with(|context| {
            let mut result = Jacobian::default();
            context.ecmult_const(&mut result, &Affine::from_gej(point), scalar);
            result
        })
    }

    fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool {
        secp256k1_jacobian_bytes(*lhs) == secp256k1_jacobian_bytes(*rhs)
    }
}

impl CurveScalarSampler for Secp256k1 {
    fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar {
        libsecp256k1::SecretKey::random(rng).into()
    }
}

impl_curve_group_adapter! {
    Secp256r1 {
        point: ProjectivePoint,
        scalar: Secp256r1ScalarField,
        identity: ProjectivePoint::IDENTITY,
        generator: ProjectivePoint::GENERATOR,
        random_scalar: |rng| {
            loop {
                let scalar = Secp256r1ScalarField::random(&mut *rng);
                if !bool::from(scalar.is_zero()) {
                    break scalar;
                }
            }
        },
        add: |lhs: &ProjectivePoint, rhs: &ProjectivePoint| *lhs + *rhs,
        neg: |point: &ProjectivePoint| -*point,
        mul: |point: &ProjectivePoint, scalar: &Secp256r1ScalarField| *point * *scalar,
        eq: |lhs: &ProjectivePoint, rhs: &ProjectivePoint| lhs == rhs,
    }
}

impl_curve_group_adapter! {
    Bls12381G1 {
        point: G1Projective,
        scalar: Bls12381ScalarField,
        identity: G1Projective::zero(),
        generator: G1Projective::generator(),
        random_scalar: |rng| {
            loop {
                let scalar = Bls12381ScalarField::rand(&mut *rng);
                if !scalar.is_zero() {
                    break scalar;
                }
            }
        },
        add: |lhs: &G1Projective, rhs: &G1Projective| *lhs + *rhs,
        neg: |point: &G1Projective| -*point,
        mul: |point: &G1Projective, scalar: &Bls12381ScalarField| *point * *scalar,
        eq: |lhs: &G1Projective, rhs: &G1Projective| lhs == rhs,
    }
}

impl_curve_group_adapter! {
    Ristretto255 {
        point: RistrettoPoint,
        scalar: Ristretto255ScalarField,
        identity: RistrettoPoint::identity(),
        generator: RISTRETTO_BASEPOINT_POINT,
        random_scalar: |rng| {
            loop {
                let mut bytes = [0u8; 64];
                rng.fill_bytes(&mut bytes);
                let scalar = Ristretto255ScalarField::from_bytes_mod_order_wide(&bytes);
                if scalar != Ristretto255ScalarField::ZERO {
                    break scalar;
                }
            }
        },
        add: |lhs: &RistrettoPoint, rhs: &RistrettoPoint| lhs + rhs,
        neg: |point: &RistrettoPoint| -point,
        mul: |point: &RistrettoPoint, scalar: &Ristretto255ScalarField| point * scalar,
        eq: |lhs: &RistrettoPoint, rhs: &RistrettoPoint| lhs == rhs,
    }
}

impl From<SecretKey> for Scalar<Secp256k1> {
    fn from(secret_key: SecretKey) -> Self {
        Self::new(secret_key.into())
    }
}

impl From<Affine> for Point<Secp256k1> {
    fn from(point: Affine) -> Self {
        Self::new(Jacobian::from_ge(&normalize_affine(point)))
    }
}

impl From<Point<Secp256k1>> for Affine {
    fn from(point: Point<Secp256k1>) -> Self {
        Affine::from_gej(&point.inner)
    }
}

impl TryFrom<PublicKey<33>> for Point<Secp256k1> {
    type Error = Error;

    fn try_from(public_key: PublicKey<33>) -> Result<Self> {
        let point: Affine = public_key.try_into()?;
        Ok(point.into())
    }
}

impl TryFrom<Point<Secp256k1>> for PublicKey<33> {
    type Error = Error;

    fn try_from(point: Point<Secp256k1>) -> Result<Self> {
        if point.inner.is_infinity() {
            return Err(Error::InvalidPublicKey);
        }
        Affine::from(point).try_into()
    }
}

fn secp256k1_generator() -> Jacobian {
    let mut one = [0u8; 32];
    one[31] = 1;
    let scalar: SecpK1FieldScalar = libsecp256k1::SecretKey::parse(&one)
        .expect("scalar one is valid")
        .into();
    SECP256K1_GEN_CONTEXT.with(|context| {
        let mut point = Jacobian::default();
        context.ecmult_gen(&mut point, &scalar);
        point
    })
}

fn secp256k1_identity() -> Jacobian {
    let mut point = Jacobian::default();
    point.set_infinity();
    point
}

fn with_group_rng<R>(f: impl FnOnce(&mut Hc128Rng) -> R) -> R {
    GROUP_RNG.with(|rng| {
        let mut rng = rng.borrow_mut();
        f(&mut rng)
    })
}

fn normalize_affine(mut point: Affine) -> Affine {
    point.x.normalize();
    point.y.normalize();
    point
}

fn secp256k1_jacobian_bytes(point: Jacobian) -> Option<([u8; 32], [u8; 32])> {
    if point.is_infinity() {
        return None;
    }
    let mut affine = Affine::from_gej(&point);
    affine.x.normalize();
    affine.y.normalize();
    Some((affine.x.b32(), affine.y.b32()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_laws<G>()
    where
        G: CryptographicGroup,
        G::Element: Eq + std::fmt::Debug,
    {
        let scalar_a = G::random_scalar();
        let scalar_b = G::random_scalar();
        let scalar_c = G::random_scalar();
        let a = G::generator() * scalar_a.clone();
        let b = G::generator() * scalar_b;
        let c = G::generator() * scalar_c;

        assert_eq!(a.clone() + G::identity(), a);
        assert_eq!(G::identity() + a.clone(), a);
        assert_eq!(a.clone() + -a.clone(), G::identity());
        assert_eq!((a.clone() + b.clone()) + c.clone(), a + (b + c));
        assert_eq!(
            G::generator_mul(scalar_a.clone()),
            G::mul_ref(&G::generator(), &scalar_a)
        );
        assert_eq!(
            G::generator_mul(scalar_a.clone()),
            G::generator() * scalar_a
        );
    }

    #[test]
    fn supported_curve_groups_satisfy_basic_laws() {
        group_laws::<Group<Secp256k1>>();
        group_laws::<Group<Secp256r1>>();
        group_laws::<Group<Bls12381G1>>();
        group_laws::<Ristretto255Group>();
    }
}
