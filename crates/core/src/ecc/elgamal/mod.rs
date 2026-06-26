//! Elgamal Crypto Implementation
//! ----------------
//! Algorithm Description
//! # Encrypt
//! A second party, Bob, encrypts a message 𝑀 to Alice under her public key (𝐺,𝑞,𝑔,ℎ)
//! as follows:
//!    Map the message 𝑀 to an element 𝑚 of 𝐺 using a reversible mapping function.
//! Choose an integer 𝑦
//! randomly from {1,…,𝑞−1}
//! 1. Compute 𝑠:=ℎ𝑦 This is called the shared secret.
//! 2. Compute 𝑐1:=𝑔𝑦
//! 3. Compute 𝑐2:=𝑚⋅𝑠
//! 4. Bob sends the ciphertext (𝑐1,𝑐2) to Alice.
//!
//! # Decrypt
//! Alice decrypts a ciphertext 𝑐1,𝑐2 with her private key 𝑠𝑘 as follows:
//! 1. Compute 𝑠:=𝑐𝑥1
//! 2. Compute 𝑠−1, the inverse of 𝑠 in the group 𝐺
//! 3. Compute 𝑚:=𝑐2⋅𝑠−1
//!
//! ref:
//!    T. ElGamal. A Public Key Cryptosystem and a Signature Scheme Based on Discrete Logarithms. IEEE Trans. Info. Theory, IT 31:469–472, 1985.
//!    ElGamal encryption <https://en.wikipedia.org/wiki/ElGamal_encryption>
//!    <http://www.docsdrive.com/pdfs/ansinet/itj/2005/299-306.pdf>
//!
//! # Abstract group implementation
//!
//! ElGamal is a public-key encryption algorithm over a finite cyclic group. It
//! is not tied to a particular elliptic curve; an elliptic curve point group is
//! one possible implementation of the carrier.
//!
//! In multiplicative notation, `c2 := m * s` and decryption computes
//! `m := c2 * s^{-1}`. The implementation below uses additive notation because
//! that is the natural convention for elliptic curve groups.
//!
//! With the group written additively, encryption of a group element `m` under
//! public key `h = xg` is:
//!
//! 1. choose fresh random scalar `r`
//! 2. compute `c1 = rg`
//! 3. compute shared secret `s = rh`
//! 4. compute `c2 = m + s`
//!
//! Decryption computes `m = c2 - x c1`.
//!
//! The pure group operation is [`ElGamal::encrypt_block`]. Randomness is passed
//! into [`ElGamal::encrypt_with_rng`] explicitly; [`ElGamal::encrypt`] is only a
//! convenience shell around the default thread-local sampler.

use std::marker::PhantomData;

use rand::RngCore;

use crate::algebra::Module;
use crate::ecc::group::Bls12381G1;
use crate::ecc::group::CyclicModule;
use crate::ecc::group::Point;
#[cfg(feature = "curve-ristretto255")]
use crate::ecc::group::Ristretto255;
use crate::ecc::group::Secp256k1;
use crate::ecc::group::Secp256r1;

pub mod impls;

/// Ciphertext pairs over one cyclic module carrier.
pub type GroupCiphertext<Element> = Vec<(Element, Element)>;

/// ElGamal over secp256k1 group elements.
pub type Secp256k1ElGamal = ElGamal<Point<Secp256k1>>;

/// ElGamal over secp256r1/P-256 group elements.
pub type Secp256r1ElGamal = ElGamal<Point<Secp256r1>>;

/// ElGamal over BLS12-381 G1 group elements.
pub type Bls12381G1ElGamal = ElGamal<Point<Bls12381G1>>;

/// ElGamal over Ristretto255 group elements.
#[cfg(feature = "curve-ristretto255")]
pub type Ristretto255ElGamal = ElGamal<Point<Ristretto255>>;

/// ElGamal public key `h = xg` over one cyclic module carrier.
pub struct ElGamalPublicKey<Element: CyclicModule> {
    element: Element,
}

/// ElGamal secret scalar `x` over one cyclic module carrier.
pub struct ElGamalSecretKey<Element: CyclicModule> {
    scalar: Element::Scalar,
}

/// ElGamal key pair over one cyclic module carrier.
pub struct ElGamalKeyPair<Element: CyclicModule> {
    secret: ElGamalSecretKey<Element>,
    public: ElGamalPublicKey<Element>,
}

/// Generic ElGamal implementation parameterized only by the element carrier.
pub struct ElGamal<Element>(PhantomData<Element>);

impl<Element> ElGamalPublicKey<Element>
where Element: CyclicModule
{
    /// Build a public key from an existing group element.
    pub fn from_element(element: Element) -> Self {
        Self { element }
    }

    /// Borrow the public group element.
    pub fn as_element(&self) -> &Element {
        &self.element
    }

    /// Unwrap into the public group element.
    pub fn into_element(self) -> Element {
        self.element
    }
}

impl<Element> Clone for ElGamalPublicKey<Element>
where Element: CyclicModule + Clone
{
    fn clone(&self) -> Self {
        Self::from_element(self.element.clone())
    }
}

impl<Element> ElGamalSecretKey<Element>
where Element: CyclicModule
{
    /// Build a secret key from an existing scalar.
    pub fn from_scalar(scalar: Element::Scalar) -> Self {
        Self { scalar }
    }

    /// Borrow the secret scalar.
    pub fn as_scalar(&self) -> &Element::Scalar {
        &self.scalar
    }

    /// Unwrap into the secret scalar.
    pub fn into_scalar(self) -> Element::Scalar {
        self.scalar
    }

    /// Derive the public key `h = xg`.
    pub fn public_key(&self) -> ElGamalPublicKey<Element> {
        ElGamalPublicKey::from_element(Element::generator_mul(&self.scalar))
    }
}

impl<Element> Clone for ElGamalSecretKey<Element>
where
    Element: CyclicModule,
    Element::Scalar: Clone,
{
    fn clone(&self) -> Self {
        Self::from_scalar(self.scalar.clone())
    }
}

impl<Element> ElGamalKeyPair<Element>
where Element: CyclicModule
{
    /// Borrow the public key.
    pub fn public_key(&self) -> &ElGamalPublicKey<Element> {
        &self.public
    }

    /// Borrow the secret key.
    pub fn secret_key(&self) -> &ElGamalSecretKey<Element> {
        &self.secret
    }
}

impl<Element> ElGamalSecretKey<Element>
where Element: CyclicModule
{
    /// Generate a fresh non-zero ElGamal secret scalar from an explicit RNG.
    pub fn random_with_rng(rng: &mut impl RngCore) -> Self {
        Self::from_scalar(Element::random_scalar_with_rng(rng))
    }

    /// Generate a fresh non-zero ElGamal secret scalar.
    pub fn random() -> Self {
        Self::from_scalar(Element::random_scalar())
    }
}

impl<Element> ElGamalKeyPair<Element>
where Element: CyclicModule
{
    /// Generate a fresh ElGamal key pair from an explicit RNG.
    pub fn random_with_rng(rng: &mut impl RngCore) -> Self {
        let secret = ElGamalSecretKey::<Element>::random_with_rng(rng);
        let public = secret.public_key();
        Self { secret, public }
    }

    /// Generate a fresh ElGamal key pair.
    pub fn random() -> Self {
        let secret = ElGamalSecretKey::<Element>::random();
        let public = secret.public_key();
        Self { secret, public }
    }
}

impl<Element> ElGamal<Element>
where
    Element: CyclicModule + Module<Element::Scalar> + Clone,
    Element::Scalar: Clone,
{
    /// Decrypt ciphertext pairs into group elements with the given scalar.
    pub fn decrypt(
        ciphertext: &[(Element, Element)],
        secret_key: &ElGamalSecretKey<Element>,
    ) -> Vec<Element> {
        ciphertext
            .iter()
            .map(|(c1, c2)| {
                let shared_secret = c1.clone() * secret_key.as_scalar().clone();
                c2.clone() - shared_secret
            })
            .collect()
    }

    /// Encrypt one group element using caller-supplied randomness.
    ///
    /// This is the pure ElGamal kernel over additive group notation:
    /// `(c1, c2) = (rg, m + rh)` where `h` is the public key and `r` is the
    /// ephemeral scalar. It performs no sampling and has no hidden side effects.
    pub fn encrypt_block(
        message_element: Element,
        public_key: &ElGamalPublicKey<Element>,
        ephemeral_scalar: Element::Scalar,
    ) -> (Element, Element) {
        let c1 = Element::generator_mul(&ephemeral_scalar);
        let shared_secret = public_key.as_element().clone() * ephemeral_scalar;
        let c2 = message_element + shared_secret;
        (c1, c2)
    }

    /// Encrypt group elements under the given public group element using an
    /// explicit RNG for ephemeral scalars.
    pub fn encrypt_with_rng<I>(
        message: I,
        public_key: &ElGamalPublicKey<Element>,
        rng: &mut impl RngCore,
    ) -> GroupCiphertext<Element>
    where
        I: IntoIterator<Item = Element>,
    {
        message
            .into_iter()
            .map(|message_element| {
                let ephemeral_scalar = Element::random_scalar_with_rng(rng);
                Self::encrypt_block(message_element, public_key, ephemeral_scalar)
            })
            .collect()
    }

    /// Encrypt group elements under the given public group element.
    pub fn encrypt<I>(
        message: I,
        public_key: &ElGamalPublicKey<Element>,
    ) -> GroupCiphertext<Element>
    where
        I: IntoIterator<Item = Element>,
    {
        message
            .into_iter()
            .map(|message_element| {
                let ephemeral_scalar = Element::random_scalar();
                Self::encrypt_block(message_element, public_key, ephemeral_scalar)
            })
            .collect()
    }
}

#[cfg(test)]
mod test {
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
    fn encrypt_block_is_pure_group_operation() {
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
    fn encrypt_decrypt_over_generic_finite_group() {
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
    fn encryption_uses_fresh_ephemeral_point_per_block() {
        let secret_key = ElGamalSecretKey::<TestElement>::from_scalar(TestScalar(42));
        let public_key = secret_key.public_key();
        let message = vec![TestElement(7); 4];
        let mut rng = Hc128Rng::seed_from_u64(7);
        let ciphertext = ElGamal::<TestElement>::encrypt_with_rng(message, &public_key, &mut rng);

        assert!(ciphertext.windows(2).any(|pair| pair[0].0 != pair[1].0));
    }

    #[test]
    fn encrypt_with_rng_is_reproducible_for_same_seed() {
        let secret_key = ElGamalSecretKey::<TestElement>::from_scalar(TestScalar(42));
        let public_key = secret_key.public_key();
        let message = vec![TestElement(7), TestElement(8), TestElement(9)];
        let mut rng_a = Hc128Rng::seed_from_u64(42);
        let mut rng_b = Hc128Rng::seed_from_u64(42);

        let ciphertext_a =
            ElGamal::<TestElement>::encrypt_with_rng(message.clone(), &public_key, &mut rng_a);
        let ciphertext_b =
            ElGamal::<TestElement>::encrypt_with_rng(message, &public_key, &mut rng_b);

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
    fn supported_curve_groups_encrypt_and_decrypt() {
        encrypt_decrypt_over_curve_group::<Point<Secp256k1>>();
        encrypt_decrypt_over_curve_group::<Point<Secp256r1>>();
        encrypt_decrypt_over_curve_group::<Point<Bls12381G1>>();
        #[cfg(feature = "curve-ristretto255")]
        encrypt_decrypt_over_curve_group::<Point<Ristretto255>>();
    }
}
