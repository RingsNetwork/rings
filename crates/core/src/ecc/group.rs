//! Elliptic-curve group abstractions and curve adapters.

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
}

/// Cyclic group abstraction with a distinguished generator.
pub trait CyclicGroup: GroupOps {
    /// Distinguished group generator.
    fn generator() -> Self::Element;

    /// Multiply the distinguished generator by a scalar.
    fn generator_mul(scalar: Self::Scalar) -> Self::Element {
        Self::generator() * scalar
    }

    /// Generate a fresh non-zero random scalar.
    fn random_scalar() -> Self::Scalar;
}

/// Curve-specific group operations implemented by curve markers.
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

    /// Generate a fresh non-zero random scalar.
    fn random_scalar() -> Self::Scalar;

    /// Group addition.
    fn add(lhs: &Self::Point, rhs: &Self::Point) -> Self::Point;

    /// Group inverse.
    fn neg(point: &Self::Point) -> Self::Point;

    /// Scalar multiplication.
    fn mul(point: &Self::Point, scalar: &Self::Scalar) -> Self::Point;

    /// Element equality.
    fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool;
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
}

impl<C: CurveGroup> CyclicGroup for Group<C> {
    fn generator() -> Self::Element {
        Point::new(C::generator())
    }

    fn generator_mul(scalar: Self::Scalar) -> Self::Element {
        Point::new(C::generator_mul(&scalar.inner))
    }

    fn random_scalar() -> Self::Scalar {
        Scalar::new(C::random_scalar())
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

    fn random_scalar() -> Self::Scalar {
        with_group_rng(|rng| libsecp256k1::SecretKey::random(rng).into())
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

impl CurveGroup for Secp256r1 {
    type Point = ProjectivePoint;
    type Scalar = Secp256r1ScalarField;

    fn identity() -> Self::Point {
        ProjectivePoint::IDENTITY
    }

    fn generator() -> Self::Point {
        ProjectivePoint::GENERATOR
    }

    fn random_scalar() -> Self::Scalar {
        loop {
            let scalar = with_group_rng(|rng| Secp256r1ScalarField::random(rng));
            if !bool::from(scalar.is_zero()) {
                return scalar;
            }
        }
    }

    fn add(lhs: &Self::Point, rhs: &Self::Point) -> Self::Point {
        *lhs + *rhs
    }

    fn neg(point: &Self::Point) -> Self::Point {
        -*point
    }

    fn mul(point: &Self::Point, scalar: &Self::Scalar) -> Self::Point {
        *point * *scalar
    }

    fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool {
        lhs == rhs
    }
}

impl CurveGroup for Bls12381G1 {
    type Point = G1Projective;
    type Scalar = Bls12381ScalarField;

    fn identity() -> Self::Point {
        G1Projective::zero()
    }

    fn generator() -> Self::Point {
        G1Projective::generator()
    }

    fn random_scalar() -> Self::Scalar {
        loop {
            let scalar = with_group_rng(Bls12381ScalarField::rand);
            if !scalar.is_zero() {
                return scalar;
            }
        }
    }

    fn add(lhs: &Self::Point, rhs: &Self::Point) -> Self::Point {
        *lhs + *rhs
    }

    fn neg(point: &Self::Point) -> Self::Point {
        -*point
    }

    fn mul(point: &Self::Point, scalar: &Self::Scalar) -> Self::Point {
        *point * *scalar
    }

    fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool {
        lhs == rhs
    }
}

impl CurveGroup for Ristretto255 {
    type Point = RistrettoPoint;
    type Scalar = Ristretto255ScalarField;

    fn identity() -> Self::Point {
        RistrettoPoint::identity()
    }

    fn generator() -> Self::Point {
        RISTRETTO_BASEPOINT_POINT
    }

    fn random_scalar() -> Self::Scalar {
        loop {
            let mut bytes = [0u8; 64];
            with_group_rng(|rng| rng.fill_bytes(&mut bytes));
            let scalar = Ristretto255ScalarField::from_bytes_mod_order_wide(&bytes);
            if scalar != Ristretto255ScalarField::zero() {
                return scalar;
            }
        }
    }

    fn add(lhs: &Self::Point, rhs: &Self::Point) -> Self::Point {
        lhs + rhs
    }

    fn neg(point: &Self::Point) -> Self::Point {
        -point
    }

    fn mul(point: &Self::Point, scalar: &Self::Scalar) -> Self::Point {
        point * scalar
    }

    fn eq(lhs: &Self::Point, rhs: &Self::Point) -> bool {
        lhs == rhs
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

impl From<ProjectivePoint> for Point<Secp256r1> {
    fn from(point: ProjectivePoint) -> Self {
        Self::new(point)
    }
}

impl From<Point<Secp256r1>> for ProjectivePoint {
    fn from(point: Point<Secp256r1>) -> Self {
        point.inner
    }
}

impl From<G1Projective> for Point<Bls12381G1> {
    fn from(point: G1Projective) -> Self {
        Self::new(point)
    }
}

impl From<Point<Bls12381G1>> for G1Projective {
    fn from(point: Point<Bls12381G1>) -> Self {
        point.inner
    }
}

impl From<RistrettoPoint> for Point<Ristretto255> {
    fn from(point: RistrettoPoint) -> Self {
        Self::new(point)
    }
}

impl From<Point<Ristretto255>> for RistrettoPoint {
    fn from(point: Point<Ristretto255>) -> Self {
        point.inner
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
        G: CyclicGroup,
        G::Element: Eq + std::fmt::Debug,
    {
        let a = G::generator() * G::random_scalar();
        let b = G::generator() * G::random_scalar();
        let c = G::generator() * G::random_scalar();

        assert_eq!(a.clone() + G::identity(), a);
        assert_eq!(G::identity() + a.clone(), a);
        assert_eq!(a.clone() + -a.clone(), G::identity());
        assert_eq!((a.clone() + b.clone()) + c.clone(), a + (b + c));
    }

    #[test]
    fn supported_curve_groups_satisfy_basic_laws() {
        group_laws::<Group<Secp256k1>>();
        group_laws::<Group<Secp256r1>>();
        group_laws::<Group<Bls12381G1>>();
        group_laws::<Ristretto255Group>();
    }
}
