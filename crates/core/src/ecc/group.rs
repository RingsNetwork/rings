//! Algebraic carriers and elliptic-curve adapters.
//!
//! The generic algebraic vocabulary lives in [`crate::algebra`]. This module
//! connects those traits to concrete elliptic-curve libraries:
//!
//! - [`Point<C>`] is the curve element carrier and implements the additive
//!   abelian-group and module traits.
//! - [`Scalar<C>`] is the curve scalar carrier and implements the field traits.
//! - [`CurveGroup`] is the adapter boundary for elliptic-curve libraries. A
//!   marker type such as [`Secp256k1`] or [`Bls12381G1`] supplies native point
//!   operations and scalar action.
//! - [`CurveScalarField`] is the separate adapter boundary for scalar-field
//!   operations and non-zero scalar sampling.
//! - [`CyclicModule`] is the algebraic capability used by cryptographic
//!   algorithms that require a distinguished generator and fresh non-zero
//!   scalars.
//!
//! All operations are written in additive notation. For a scalar `x` and
//! generator `g`, `xg` is represented by [`CyclicModule::generator_mul`].
//!
//! This split gives the rest of the cryptographic code one stable vocabulary:
//! algorithms depend on algebraic carrier laws, not on a concrete crate such as
//! `libsecp256k1`, `p256`, `arkworks`, or `curve25519-dalek`. Adding a curve is
//! therefore a matter of implementing the point and scalar adapter boundaries
//! once; algorithms such as ElGamal do not need per-curve branches.

use std::cell::RefCell;
use std::convert::TryFrom;
use std::ops::Add;
use std::ops::Mul;
use std::ops::Neg;
use std::ops::Sub;
use std::sync::OnceLock;

use ark_bls12_381::Fr as Bls12381ScalarField;
use ark_bls12_381::G1Projective;
use ark_ec::Group as ArkGroup;
use ark_ff::Field as _;
use ark_ff::Zero as _;
use ark_std::UniformRand;
#[cfg(feature = "curve-ristretto255")]
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
#[cfg(feature = "curve-ristretto255")]
use curve25519_dalek::ristretto::RistrettoPoint;
#[cfg(feature = "curve-ristretto255")]
use curve25519_dalek::scalar::Scalar as Ristretto255ScalarField;
#[cfg(feature = "curve-ristretto255")]
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

use crate::algebra::AbelianGroup;
use crate::algebra::CommutativeRing;
use crate::algebra::Field as AlgebraField;
use crate::algebra::Module;
use crate::algebra::One as AlgebraOne;
use crate::algebra::Zero as AlgebraZero;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// Curve-specific point-group and scalar-action operations implemented by curve markers.
///
/// This trait is the adapter boundary between algebraic point carriers and
/// concrete elliptic-curve libraries. For a marker `C`, the native `Point` type
/// must represent elements of one finite abelian group, and `Scalar` must be the
/// native scalar type used for the right module action. Scalar field operations
/// live in [`CurveScalarField`], not here.
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
/// - scalar multiplication is a right module action:
///   `mul(add(p, q), s) == add(mul(p, s), mul(q, s))`.
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

/// Curve-specific scalar-field operations implemented by curve markers.
///
/// This trait is intentionally separate from [`CurveGroup`]. A curve point
/// group and its scalar field are related by the module action, but they are
/// different carriers with different operations and different law obligations.
///
/// Implementors must preserve these laws:
///
/// - scalars form a finite [`Field`](crate::algebra::Field);
/// - `scalar_eq` is total equality over canonical scalar values;
/// - `random_scalar_with_rng` returns a non-zero scalar.
pub trait CurveScalarField: CurveGroup {
    /// Scalar additive identity.
    fn scalar_zero() -> Self::Scalar;

    /// Scalar multiplicative identity.
    fn scalar_one() -> Self::Scalar;

    /// Return whether the scalar is the additive identity.
    fn scalar_is_zero(scalar: &Self::Scalar) -> bool;

    /// Scalar addition.
    fn scalar_add(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar;

    /// Scalar subtraction.
    fn scalar_sub(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar;

    /// Scalar additive inverse.
    fn scalar_neg(scalar: &Self::Scalar) -> Self::Scalar;

    /// Scalar multiplication.
    fn scalar_mul(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar;

    /// Scalar multiplicative inverse.
    fn scalar_inverse(scalar: &Self::Scalar) -> Option<Self::Scalar>;

    /// Scalar equality.
    fn scalar_eq(lhs: &Self::Scalar, rhs: &Self::Scalar) -> bool;

    /// Generate a fresh non-zero random scalar from an explicit RNG.
    fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar;

    /// Generate a fresh non-zero random scalar from the default thread-local RNG.
    fn random_scalar() -> Self::Scalar {
        with_group_rng(|rng| Self::random_scalar_with_rng(rng))
    }
}

/// Algebraic carrier with a distinguished generator and non-zero scalar sampler.
///
/// This is not a replacement group hierarchy; it is the extra capability needed
/// by cryptographic algorithms such as ElGamal after the carrier already
/// implements [`AbelianGroup`] and [`Module`]. The implementation obligation is
/// that `generator_mul(s)` equals `generator() * s` and that sampled scalars are
/// non-zero field elements.
///
/// `Module<Self::Scalar>` stays as an explicit consumer bound instead of a
/// supertrait here. `CyclicModule` introduces the associated scalar type, while
/// [`Module`] proves the right scalar action for arbitrary elements; consumers
/// that multiply elements by scalars should request both
/// `CyclicModule` and `Module<Element::Scalar>`. This keeps the generator and
/// sampling capability separate from the module-action proof while still
/// requiring `generator_mul(s)` to be observationally equal to `generator() * s`.
///
/// Non-zero scalar sampling also appears in [`CurveScalarField`] intentionally.
/// Curve adapters provide the native scalar-field sampler; `CyclicModule`
/// exposes the same cryptographic capability through the element carrier so
/// algorithms do not need to know the curve marker type.
pub trait CyclicModule: AbelianGroup + Sized {
    /// Scalar field for the module action.
    type Scalar: AlgebraField;

    /// Distinguished generator for the cyclic subgroup used by the algorithm.
    fn generator() -> Self;

    /// Multiply the distinguished generator by a scalar.
    fn generator_mul(scalar: &Self::Scalar) -> Self;

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
#[cfg(feature = "curve-ristretto255")]
#[derive(Debug)]
pub struct Ristretto255;

thread_local! {
    static GROUP_RNG: RefCell<Hc128Rng> = RefCell::new(Hc128Rng::from_entropy());
}

static SECP256K1_GENERATOR: OnceLock<Jacobian> = OnceLock::new();

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

impl<C: CurveGroup> Sub for Point<C> {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        self + (-rhs)
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

impl<C: CurveGroup> AlgebraZero for Point<C> {
    fn zero() -> Self {
        Self::new(C::identity())
    }

    fn is_zero(&self) -> bool {
        C::eq(&self.inner, &C::identity())
    }
}

impl<C: CurveGroup> AbelianGroup for Point<C> {}

impl<C: CurveScalarField> Module<Scalar<C>> for Point<C> {}

impl<C: CurveScalarField> CyclicModule for Point<C> {
    type Scalar = Scalar<C>;

    fn generator() -> Self {
        Self::new(C::generator())
    }

    fn generator_mul(scalar: &Self::Scalar) -> Self {
        Self::new(C::generator_mul(&scalar.inner))
    }

    fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar {
        Scalar::new(C::random_scalar_with_rng(rng))
    }
}

impl<C: CurveScalarField> Add for Scalar<C> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self::new(C::scalar_add(&self.inner, &rhs.inner))
    }
}

impl<C: CurveScalarField> Sub for Scalar<C> {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(C::scalar_sub(&self.inner, &rhs.inner))
    }
}

impl<C: CurveScalarField> Neg for Scalar<C> {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self::new(C::scalar_neg(&self.inner))
    }
}

impl<C: CurveScalarField> Mul for Scalar<C> {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        Self::new(C::scalar_mul(&self.inner, &rhs.inner))
    }
}

impl<C: CurveScalarField> PartialEq for Scalar<C> {
    fn eq(&self, other: &Self) -> bool {
        C::scalar_eq(&self.inner, &other.inner)
    }
}

impl<C: CurveScalarField> Eq for Scalar<C> {}

impl<C: CurveScalarField> AlgebraZero for Scalar<C> {
    fn zero() -> Self {
        Self::new(C::scalar_zero())
    }

    fn is_zero(&self) -> bool {
        C::scalar_is_zero(&self.inner)
    }
}

impl<C: CurveScalarField> AlgebraOne for Scalar<C> {
    fn one() -> Self {
        Self::new(C::scalar_one())
    }
}

impl<C: CurveScalarField> AbelianGroup for Scalar<C> {}

impl<C: CurveScalarField> CommutativeRing for Scalar<C> {}

impl<C: CurveScalarField> AlgebraField for Scalar<C> {
    fn try_inverse(&self) -> Option<Self> {
        C::scalar_inverse(&self.inner).map(Self::new)
    }
}

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
            eq: $eq:expr,
            scalar_zero: $scalar_zero:expr,
            scalar_one: $scalar_one:expr,
            scalar_is_zero: $scalar_is_zero:expr,
            scalar_add: $scalar_add:expr,
            scalar_sub: $scalar_sub:expr,
            scalar_neg: $scalar_neg:expr,
            scalar_mul: $scalar_mul:expr,
            scalar_inverse: $scalar_inverse:expr,
            scalar_eq: $scalar_eq:expr $(,)?
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

        impl CurveScalarField for $curve {
            fn scalar_zero() -> Self::Scalar {
                $scalar_zero
            }

            fn scalar_one() -> Self::Scalar {
                $scalar_one
            }

            fn scalar_is_zero(scalar: &Self::Scalar) -> bool {
                ($scalar_is_zero)(scalar)
            }

            fn scalar_add(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar {
                ($scalar_add)(lhs, rhs)
            }

            fn scalar_sub(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar {
                ($scalar_sub)(lhs, rhs)
            }

            fn scalar_neg(scalar: &Self::Scalar) -> Self::Scalar {
                ($scalar_neg)(scalar)
            }

            fn scalar_mul(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar {
                ($scalar_mul)(lhs, rhs)
            }

            fn scalar_inverse(scalar: &Self::Scalar) -> Option<Self::Scalar> {
                ($scalar_inverse)(scalar)
            }

            fn scalar_eq(lhs: &Self::Scalar, rhs: &Self::Scalar) -> bool {
                ($scalar_eq)(lhs, rhs)
            }

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
        *SECP256K1_GENERATOR.get_or_init(secp256k1_generator)
    }

    fn generator_mul(scalar: &Self::Scalar) -> Self::Point {
        let mut result = Jacobian::default();
        secp256k1_generator_context().ecmult_gen(&mut result, scalar);
        result
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
        let mut result = Jacobian::default();
        secp256k1_multiplication_context().ecmult_const(
            &mut result,
            &Affine::from_gej(point),
            scalar,
        );
        result
    }

    fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool {
        secp256k1_jacobian_bytes(*lhs) == secp256k1_jacobian_bytes(*rhs)
    }
}

impl CurveScalarField for Secp256k1 {
    fn scalar_zero() -> Self::Scalar {
        SecpK1FieldScalar::from_int(0)
    }

    fn scalar_one() -> Self::Scalar {
        SecpK1FieldScalar::from_int(1)
    }

    fn scalar_is_zero(scalar: &Self::Scalar) -> bool {
        scalar.is_zero()
    }

    fn scalar_add(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar {
        *lhs + *rhs
    }

    fn scalar_sub(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar {
        *lhs + -*rhs
    }

    fn scalar_neg(scalar: &Self::Scalar) -> Self::Scalar {
        -*scalar
    }

    fn scalar_mul(lhs: &Self::Scalar, rhs: &Self::Scalar) -> Self::Scalar {
        *lhs * *rhs
    }

    fn scalar_inverse(scalar: &Self::Scalar) -> Option<Self::Scalar> {
        if scalar.is_zero() {
            None
        } else {
            Some(scalar.inv())
        }
    }

    fn scalar_eq(lhs: &Self::Scalar, rhs: &Self::Scalar) -> bool {
        lhs == rhs
    }

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
        scalar_zero: Secp256r1ScalarField::ZERO,
        scalar_one: Secp256r1ScalarField::ONE,
        scalar_is_zero: |scalar: &Secp256r1ScalarField| bool::from(scalar.is_zero()),
        scalar_add: |lhs: &Secp256r1ScalarField, rhs: &Secp256r1ScalarField| *lhs + *rhs,
        scalar_sub: |lhs: &Secp256r1ScalarField, rhs: &Secp256r1ScalarField| *lhs - *rhs,
        scalar_neg: |scalar: &Secp256r1ScalarField| -*scalar,
        scalar_mul: |lhs: &Secp256r1ScalarField, rhs: &Secp256r1ScalarField| *lhs * *rhs,
        scalar_inverse: |scalar: &Secp256r1ScalarField| scalar.invert().into_option(),
        scalar_eq: |lhs: &Secp256r1ScalarField, rhs: &Secp256r1ScalarField| lhs == rhs,
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
        scalar_zero: Bls12381ScalarField::ZERO,
        scalar_one: Bls12381ScalarField::ONE,
        scalar_is_zero: |scalar: &Bls12381ScalarField| scalar.is_zero(),
        scalar_add: |lhs: &Bls12381ScalarField, rhs: &Bls12381ScalarField| *lhs + *rhs,
        scalar_sub: |lhs: &Bls12381ScalarField, rhs: &Bls12381ScalarField| *lhs - *rhs,
        scalar_neg: |scalar: &Bls12381ScalarField| -*scalar,
        scalar_mul: |lhs: &Bls12381ScalarField, rhs: &Bls12381ScalarField| *lhs * *rhs,
        scalar_inverse: |scalar: &Bls12381ScalarField| scalar.inverse(),
        scalar_eq: |lhs: &Bls12381ScalarField, rhs: &Bls12381ScalarField| lhs == rhs,
    }
}

#[cfg(feature = "curve-ristretto255")]
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
        scalar_zero: Ristretto255ScalarField::ZERO,
        scalar_one: Ristretto255ScalarField::ONE,
        scalar_is_zero: |scalar: &Ristretto255ScalarField| *scalar == Ristretto255ScalarField::ZERO,
        scalar_add: |lhs: &Ristretto255ScalarField, rhs: &Ristretto255ScalarField| *lhs + *rhs,
        scalar_sub: |lhs: &Ristretto255ScalarField, rhs: &Ristretto255ScalarField| *lhs - *rhs,
        scalar_neg: |scalar: &Ristretto255ScalarField| -*scalar,
        scalar_mul: |lhs: &Ristretto255ScalarField, rhs: &Ristretto255ScalarField| *lhs * *rhs,
        scalar_inverse: |scalar: &Ristretto255ScalarField| {
            if *scalar == Ristretto255ScalarField::ZERO {
                None
            } else {
                Some(scalar.invert())
            }
        },
        scalar_eq: |lhs: &Ristretto255ScalarField, rhs: &Ristretto255ScalarField| lhs == rhs,
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
    let scalar = SecpK1FieldScalar::from_int(1);
    let mut point = Jacobian::default();
    secp256k1_generator_context().ecmult_gen(&mut point, &scalar);
    point
}

// Pre:
// - libsecp256k1 exposes immutable precomputed tables under `static-context`.
// - group randomness remains independent mutable state and stays in `GROUP_RNG`.
// Invariant:
// - every secp256k1 group operation borrows the same process-global contexts.
// - no worker thread constructs its own `ECMultContext` or `ECMultGenContext`.
// Post:
// - secp256k1 arithmetic does not allocate per-thread precomputation tables.
fn secp256k1_generator_context() -> &'static ECMultGenContext {
    &libsecp256k1::ECMULT_GEN_CONTEXT
}

fn secp256k1_multiplication_context() -> &'static ECMultContext {
    &libsecp256k1::ECMULT_CONTEXT
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
    use crate::algebra::assert_field_laws;
    use crate::algebra::assert_module_action_laws;
    use crate::algebra::One;
    use crate::algebra::Zero;

    fn cyclic_module_laws<Element>()
    where
        Element: CyclicModule + Module<Element::Scalar> + Clone + Eq + std::fmt::Debug,
        Element::Scalar: Clone + Eq + std::fmt::Debug,
    {
        let scalar_a = Element::random_scalar();
        let scalar_b = Element::random_scalar();
        let scalar_c = Element::random_scalar();
        let a = Element::generator() * scalar_a.clone();
        let b = Element::generator() * scalar_b;
        let c = Element::generator() * scalar_c;

        assert_eq!(a.clone() + Element::zero(), a);
        assert_eq!(Element::zero() + a.clone(), a);
        assert_eq!(a.clone() + -a.clone(), Element::zero());
        assert_eq!((a.clone() + b.clone()) + c.clone(), a + (b + c));
        assert_eq!(
            Element::generator_mul(&scalar_a),
            Element::generator() * scalar_a
        );
    }

    fn algebra_laws<C>()
    where
        C: CurveScalarField,
        Point<C>: Eq + std::fmt::Debug,
        Scalar<C>: Eq + std::fmt::Debug,
    {
        let scalar_a = Scalar::<C>::new(C::random_scalar());
        let scalar_b = Scalar::<C>::new(C::random_scalar());
        let scalar_c = Scalar::<C>::new(C::random_scalar());
        let scalars = vec![
            Scalar::<C>::zero(),
            Scalar::<C>::one(),
            scalar_a.clone(),
            scalar_b.clone(),
            scalar_c.clone(),
        ];

        let generator = Point::<C>::new(C::generator());
        let points = vec![
            Point::<C>::zero(),
            generator.clone(),
            generator.clone() * scalar_a,
            generator.clone() * scalar_b,
            generator * scalar_c,
        ];

        assert_field_laws(&scalars);
        assert_module_action_laws(&scalars, &points);
    }

    #[test]
    fn supported_curve_groups_satisfy_basic_laws() {
        cyclic_module_laws::<Point<Secp256k1>>();
        cyclic_module_laws::<Point<Secp256r1>>();
        cyclic_module_laws::<Point<Bls12381G1>>();
        #[cfg(feature = "curve-ristretto255")]
        cyclic_module_laws::<Point<Ristretto255>>();
    }

    #[test]
    fn supported_curve_groups_satisfy_algebra_laws() {
        algebra_laws::<Secp256k1>();
        algebra_laws::<Secp256r1>();
        algebra_laws::<Bls12381G1>();
        #[cfg(feature = "curve-ristretto255")]
        algebra_laws::<Ristretto255>();
    }

    #[test]
    fn secp256k1_contexts_are_shared_across_threads() {
        const THREAD_COUNT: usize = 4;

        let context_addresses = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREAD_COUNT)
                .map(|_| {
                    scope.spawn(|| {
                        let scalar = SecpK1FieldScalar::from_int(2);
                        let generator = <Secp256k1 as CurveGroup>::generator();
                        let _ = <Secp256k1 as CurveGroup>::generator_mul(&scalar);
                        let _ = <Secp256k1 as CurveGroup>::mul(&generator, &scalar);

                        (
                            secp256k1_generator_context() as *const ECMultGenContext as usize,
                            secp256k1_multiplication_context() as *const ECMultContext as usize,
                        )
                    })
                })
                .collect();

            let mut addresses = std::collections::BTreeSet::new();
            for handle in handles {
                match handle.join() {
                    Ok(address) => {
                        addresses.insert(address);
                    }
                    Err(payload) => std::panic::resume_unwind(payload),
                }
            }
            addresses
        });

        assert_eq!(
            context_addresses.len(),
            1,
            "secp256k1 precomputed contexts must be process-global"
        );
    }
}
