use std::ops::Add;
use std::ops::Mul;
use std::ops::Neg;
use std::ops::Sub;

use rand::RngCore;
use rand::SeedableRng;
use rand_hc::Hc128Rng;

use super::*;
use crate::algebra::AbelianGroup;
use crate::algebra::CommutativeRing;
use crate::algebra::Field as AlgebraField;
use crate::algebra::One;
use crate::algebra::Zero;
use crate::ecc::group::Bls12381G1;
#[cfg(feature = "curve-ristretto255")]
use crate::ecc::group::Ristretto255;
use crate::ecc::group::Secp256k1;
use crate::ecc::group::Secp256r1;

const TEST_GROUP_ORDER: u32 = 65_521;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestElement(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestScalar(u32);

fn scalar_product(lhs: u32, rhs: u32) -> u32 {
    ((u64::from(lhs) * u64::from(rhs)) % u64::from(TEST_GROUP_ORDER)) as u32
}

fn scalar_pow(mut base: TestScalar, mut exponent: u32) -> TestScalar {
    let mut acc = TestScalar::one();
    while exponent > 0 {
        if exponent & 1 == 1 {
            acc = acc * base;
        }
        base = base * base;
        exponent >>= 1;
    }
    acc
}

impl Add for TestElement {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self((self.0 + rhs.0) % TEST_GROUP_ORDER)
    }
}

impl Sub for TestElement {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        self + -rhs
    }
}

impl Neg for TestElement {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self((TEST_GROUP_ORDER - self.0) % TEST_GROUP_ORDER)
    }
}

impl Mul<TestScalar> for TestElement {
    type Output = Self;

    fn mul(self, rhs: TestScalar) -> Self::Output {
        Self(scalar_product(self.0, rhs.0))
    }
}

impl Zero for TestElement {
    fn zero() -> Self {
        Self(0)
    }

    fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl AbelianGroup for TestElement {}

impl Module<TestScalar> for TestElement {}

impl CyclicModule for TestElement {
    type Scalar = TestScalar;

    fn generator() -> Self {
        Self(1)
    }

    fn generator_mul(scalar: &Self::Scalar) -> Self {
        Self::generator() * *scalar
    }

    fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar {
        TestScalar(rng.next_u32() % (TEST_GROUP_ORDER - 1) + 1)
    }
}

impl Add for TestScalar {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self((self.0 + rhs.0) % TEST_GROUP_ORDER)
    }
}

impl Sub for TestScalar {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        self + -rhs
    }
}

impl Neg for TestScalar {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self((TEST_GROUP_ORDER - self.0) % TEST_GROUP_ORDER)
    }
}

impl Mul for TestScalar {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        Self(scalar_product(self.0, rhs.0))
    }
}

impl Zero for TestScalar {
    fn zero() -> Self {
        Self(0)
    }

    fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl One for TestScalar {
    fn one() -> Self {
        Self(1)
    }
}

impl AbelianGroup for TestScalar {}

impl CommutativeRing for TestScalar {}

impl AlgebraField for TestScalar {
    fn try_inverse(&self) -> Option<Self> {
        if self.is_zero() {
            None
        } else {
            Some(scalar_pow(*self, TEST_GROUP_ORDER - 2))
        }
    }
}

#[test]
fn test_encrypt_block_is_pure_group_operation() {
    let secret_key = ElGamalSecretKey::<TestElement>::from_scalar(TestScalar(5));
    let public_key = secret_key.public_key();

    let ciphertext =
        ElGamal::<TestElement>::encrypt_block(TestElement(7), &public_key, TestScalar(3));

    assert_eq!(ciphertext, (TestElement(3), TestElement(22)));
    assert_eq!(
        ElGamal::<TestElement>::decrypt(&[ciphertext], &secret_key),
        vec![TestElement(7)]
    );
}

#[test]
fn test_encrypt_decrypt_over_generic_finite_group() {
    let secret_key = ElGamalSecretKey::<TestElement>::from_scalar(TestScalar(12_345));
    let public_key = secret_key.public_key();
    let message = vec![TestElement(1), TestElement(42), TestElement(65_520)];
    let ciphertext = ElGamal::<TestElement>::encrypt(message.clone(), &public_key);

    assert_eq!(
        ElGamal::<TestElement>::decrypt(&ciphertext, &secret_key),
        message
    );
}

#[test]
fn test_encryption_uses_fresh_ephemeral_point_per_block() {
    let secret_key = ElGamalSecretKey::<TestElement>::from_scalar(TestScalar(42));
    let public_key = secret_key.public_key();
    let message = vec![TestElement(7); 4];
    let mut rng = Hc128Rng::seed_from_u64(7);
    let ciphertext = ElGamal::<TestElement>::encrypt_with_rng(message, &public_key, &mut rng);

    assert!(ciphertext.windows(2).any(|pair| pair[0].0 != pair[1].0));
}

#[test]
fn test_encrypt_with_rng_is_reproducible_for_same_seed() {
    let secret_key = ElGamalSecretKey::<TestElement>::from_scalar(TestScalar(42));
    let public_key = secret_key.public_key();
    let message = vec![TestElement(7), TestElement(8), TestElement(9)];
    let mut rng_a = Hc128Rng::seed_from_u64(42);
    let mut rng_b = Hc128Rng::seed_from_u64(42);

    let ciphertext_a =
        ElGamal::<TestElement>::encrypt_with_rng(message.clone(), &public_key, &mut rng_a);
    let ciphertext_b = ElGamal::<TestElement>::encrypt_with_rng(message, &public_key, &mut rng_b);

    assert_eq!(ciphertext_a, ciphertext_b);
}

fn encrypt_decrypt_over_curve_group<Element>()
where
    Element: CyclicModule + Module<Element::Scalar> + Clone + Eq + std::fmt::Debug,
    Element::Scalar: Clone,
{
    let mut rng = Hc128Rng::seed_from_u64(11);
    let keypair = ElGamalKeyPair::<Element>::random_with_rng(&mut rng);
    let message = vec![
        Element::generator(),
        Element::generator_mul(&Element::random_scalar_with_rng(&mut rng)),
        Element::zero(),
    ];
    let ciphertext =
        ElGamal::<Element>::encrypt_with_rng(message.clone(), keypair.public_key(), &mut rng);

    assert_eq!(
        ElGamal::<Element>::decrypt(&ciphertext, keypair.secret_key()),
        message
    );
}

#[test]
fn test_supported_curve_groups_encrypt_and_decrypt() {
    encrypt_decrypt_over_curve_group::<Point<Secp256k1>>();
    encrypt_decrypt_over_curve_group::<Point<Secp256r1>>();
    encrypt_decrypt_over_curve_group::<Point<Bls12381G1>>();
    #[cfg(feature = "curve-ristretto255")]
    encrypt_decrypt_over_curve_group::<Point<Ristretto255>>();
}
