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
//! is not tied to a particular elliptic curve; an elliptic curve group is one
//! possible implementation of the group operation.
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

use crate::ecc::group::Bls12381G1;
use crate::ecc::group::CryptographicGroup;
use crate::ecc::group::CyclicGroup;
use crate::ecc::group::Group;
use crate::ecc::group::GroupOps;
use crate::ecc::group::Ristretto255Group;
use crate::ecc::group::Secp256k1;
use crate::ecc::group::Secp256r1;

pub mod impls;

/// Ciphertext pairs over one cyclic group.
pub type GroupCiphertext<G> = Vec<(<G as GroupOps>::Element, <G as GroupOps>::Element)>;

/// ElGamal over secp256k1 group elements.
pub type Secp256k1ElGamal = ElGamal<Group<Secp256k1>>;

/// ElGamal over secp256r1/P-256 group elements.
pub type Secp256r1ElGamal = ElGamal<Group<Secp256r1>>;

/// ElGamal over BLS12-381 G1 group elements.
pub type Bls12381G1ElGamal = ElGamal<Group<Bls12381G1>>;

/// ElGamal over Ristretto255 group elements.
pub type Ristretto255ElGamal = ElGamal<Ristretto255Group>;

/// ElGamal public key `h = xg` over one cyclic group.
pub struct ElGamalPublicKey<G: GroupOps> {
    element: G::Element,
}

/// ElGamal secret scalar `x` over one cyclic group.
pub struct ElGamalSecretKey<G: GroupOps> {
    scalar: G::Scalar,
}

/// ElGamal key pair over one cyclic group.
pub struct ElGamalKeyPair<G: GroupOps> {
    secret: ElGamalSecretKey<G>,
    public: ElGamalPublicKey<G>,
}

/// Generic ElGamal implementation parameterized only by a cyclic group.
pub struct ElGamal<G>(PhantomData<G>);

impl<G: GroupOps> ElGamalPublicKey<G> {
    /// Build a public key from an existing group element.
    pub fn from_element(element: G::Element) -> Self {
        Self { element }
    }

    /// Borrow the public group element.
    pub fn as_element(&self) -> &G::Element {
        &self.element
    }

    /// Unwrap into the public group element.
    pub fn into_element(self) -> G::Element {
        self.element
    }
}

impl<G: GroupOps> Clone for ElGamalPublicKey<G>
where G::Element: Clone
{
    fn clone(&self) -> Self {
        Self::from_element(self.element.clone())
    }
}

impl<G: GroupOps> ElGamalSecretKey<G> {
    /// Build a secret key from an existing scalar.
    pub fn from_scalar(scalar: G::Scalar) -> Self {
        Self { scalar }
    }

    /// Borrow the secret scalar.
    pub fn as_scalar(&self) -> &G::Scalar {
        &self.scalar
    }

    /// Unwrap into the secret scalar.
    pub fn into_scalar(self) -> G::Scalar {
        self.scalar
    }
}

impl<G: GroupOps> Clone for ElGamalSecretKey<G>
where G::Scalar: Clone
{
    fn clone(&self) -> Self {
        Self::from_scalar(self.scalar.clone())
    }
}

impl<G> ElGamalSecretKey<G>
where G: CyclicGroup
{
    /// Derive the public key `h = xg`.
    pub fn public_key(&self) -> ElGamalPublicKey<G> {
        ElGamalPublicKey::from_element(G::generator_mul(self.scalar.clone()))
    }
}

impl<G> ElGamalKeyPair<G>
where G: GroupOps
{
    /// Borrow the public key.
    pub fn public_key(&self) -> &ElGamalPublicKey<G> {
        &self.public
    }

    /// Borrow the secret key.
    pub fn secret_key(&self) -> &ElGamalSecretKey<G> {
        &self.secret
    }
}

impl<G> ElGamalSecretKey<G>
where G: CryptographicGroup
{
    /// Generate a fresh non-zero ElGamal secret scalar from an explicit RNG.
    pub fn random_with_rng(rng: &mut impl RngCore) -> Self {
        Self::from_scalar(G::random_scalar_with_rng(rng))
    }

    /// Generate a fresh non-zero ElGamal secret scalar.
    pub fn random() -> Self {
        Self::from_scalar(G::random_scalar())
    }
}

impl<G> ElGamalKeyPair<G>
where G: CryptographicGroup
{
    /// Generate a fresh ElGamal key pair from an explicit RNG.
    pub fn random_with_rng(rng: &mut impl RngCore) -> Self {
        let secret = ElGamalSecretKey::<G>::random_with_rng(rng);
        let public = secret.public_key();
        Self { secret, public }
    }

    /// Generate a fresh ElGamal key pair.
    pub fn random() -> Self {
        let secret = ElGamalSecretKey::<G>::random();
        let public = secret.public_key();
        Self { secret, public }
    }
}

impl<G> ElGamal<G>
where G: GroupOps
{
    /// Decrypt ciphertext pairs into group elements with the given scalar.
    pub fn decrypt(
        ciphertext: &[(G::Element, G::Element)],
        secret_key: &ElGamalSecretKey<G>,
    ) -> Vec<G::Element> {
        ciphertext
            .iter()
            .map(|(c1, c2)| {
                let shared_secret = G::mul_ref(c1, secret_key.as_scalar());
                G::add_ref(c2, &G::neg_ref(&shared_secret))
            })
            .collect()
    }
}

impl<G> ElGamal<G>
where G: CyclicGroup
{
    /// Encrypt one group element using caller-supplied randomness.
    ///
    /// This is the pure ElGamal kernel over additive group notation:
    /// `(c1, c2) = (rg, m + rh)` where `h` is the public key and `r` is the
    /// ephemeral scalar. It performs no sampling and has no hidden side effects.
    pub fn encrypt_block(
        message_element: G::Element,
        public_key: &ElGamalPublicKey<G>,
        ephemeral_scalar: G::Scalar,
    ) -> (G::Element, G::Element) {
        let shared_secret = G::mul_ref(public_key.as_element(), &ephemeral_scalar);
        let c1 = G::generator_mul(ephemeral_scalar);
        let c2 = G::add_ref(&message_element, &shared_secret);
        (c1, c2)
    }
}

impl<G> ElGamal<G>
where G: CryptographicGroup
{
    /// Encrypt group elements under the given public group element using an
    /// explicit RNG for ephemeral scalars.
    pub fn encrypt_with_rng<I>(
        message: I,
        public_key: &ElGamalPublicKey<G>,
        rng: &mut impl RngCore,
    ) -> GroupCiphertext<G>
    where
        I: IntoIterator<Item = G::Element>,
    {
        message
            .into_iter()
            .map(|message_element| {
                let ephemeral_scalar = G::random_scalar_with_rng(rng);
                Self::encrypt_block(message_element, public_key, ephemeral_scalar)
            })
            .collect()
    }

    /// Encrypt group elements under the given public group element.
    pub fn encrypt<I>(message: I, public_key: &ElGamalPublicKey<G>) -> GroupCiphertext<G>
    where I: IntoIterator<Item = G::Element> {
        message
            .into_iter()
            .map(|message_element| {
                let ephemeral_scalar = G::random_scalar();
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

    use rand::RngCore;
    use rand::SeedableRng;
    use rand_hc::Hc128Rng;

    use super::*;
    use crate::ecc::group::Bls12381G1;
    use crate::ecc::group::CyclicGroup;
    use crate::ecc::group::Group;
    use crate::ecc::group::Ristretto255Group;
    use crate::ecc::group::Secp256k1;
    use crate::ecc::group::Secp256r1;

    const TEST_GROUP_ORDER: u32 = 65_521;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct TestElement(u32);

    #[derive(Clone, Copy, Debug)]
    struct TestScalar(u32);

    struct TestGroup;

    impl Add for TestElement {
        type Output = Self;

        fn add(self, rhs: Self) -> Self::Output {
            Self((self.0 + rhs.0) % TEST_GROUP_ORDER)
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
            Self((self.0 * rhs.0) % TEST_GROUP_ORDER)
        }
    }

    impl GroupOps for TestGroup {
        type Element = TestElement;
        type Scalar = TestScalar;

        fn identity() -> Self::Element {
            TestElement(0)
        }
    }

    impl CyclicGroup for TestGroup {
        fn generator() -> Self::Element {
            TestElement(1)
        }
    }

    impl CryptographicGroup for TestGroup {
        fn random_scalar_with_rng(rng: &mut impl RngCore) -> Self::Scalar {
            TestScalar(rng.next_u32() % (TEST_GROUP_ORDER - 1) + 1)
        }
    }

    #[test]
    fn encrypt_block_is_pure_group_operation() {
        let secret_key = ElGamalSecretKey::<TestGroup>::from_scalar(TestScalar(5));
        let public_key = secret_key.public_key();

        let ciphertext =
            ElGamal::<TestGroup>::encrypt_block(TestElement(7), &public_key, TestScalar(3));

        assert_eq!(ciphertext, (TestElement(3), TestElement(22)));
        assert_eq!(
            ElGamal::<TestGroup>::decrypt(&[ciphertext], &secret_key),
            vec![TestElement(7)]
        );
    }

    #[test]
    fn encrypt_decrypt_over_generic_finite_group() {
        let secret_key = ElGamalSecretKey::<TestGroup>::from_scalar(TestScalar(12_345));
        let public_key = secret_key.public_key();
        let message = vec![TestElement(1), TestElement(42), TestElement(65_520)];
        let ciphertext = ElGamal::<TestGroup>::encrypt(message.clone(), &public_key);

        assert_eq!(
            ElGamal::<TestGroup>::decrypt(&ciphertext, &secret_key),
            message
        );
    }

    #[test]
    fn encryption_uses_fresh_ephemeral_point_per_block() {
        let secret_key = ElGamalSecretKey::<TestGroup>::from_scalar(TestScalar(42));
        let public_key = secret_key.public_key();
        let message = vec![TestElement(7); 4];
        let mut rng = Hc128Rng::seed_from_u64(7);
        let ciphertext = ElGamal::<TestGroup>::encrypt_with_rng(message, &public_key, &mut rng);

        assert!(ciphertext.windows(2).any(|pair| pair[0].0 != pair[1].0));
    }

    #[test]
    fn encrypt_with_rng_is_reproducible_for_same_seed() {
        let secret_key = ElGamalSecretKey::<TestGroup>::from_scalar(TestScalar(42));
        let public_key = secret_key.public_key();
        let message = vec![TestElement(7), TestElement(8), TestElement(9)];
        let mut rng_a = Hc128Rng::seed_from_u64(42);
        let mut rng_b = Hc128Rng::seed_from_u64(42);

        let ciphertext_a =
            ElGamal::<TestGroup>::encrypt_with_rng(message.clone(), &public_key, &mut rng_a);
        let ciphertext_b = ElGamal::<TestGroup>::encrypt_with_rng(message, &public_key, &mut rng_b);

        assert_eq!(ciphertext_a, ciphertext_b);
    }

    fn encrypt_decrypt_over_curve_group<G>()
    where
        G: CryptographicGroup,
        G::Element: Eq + std::fmt::Debug,
    {
        let mut rng = Hc128Rng::seed_from_u64(11);
        let keypair = ElGamalKeyPair::<G>::random_with_rng(&mut rng);
        let message = vec![
            G::generator(),
            G::generator_mul(G::random_scalar_with_rng(&mut rng)),
            G::identity(),
        ];
        let ciphertext =
            ElGamal::<G>::encrypt_with_rng(message.clone(), keypair.public_key(), &mut rng);

        assert_eq!(
            ElGamal::<G>::decrypt(&ciphertext, keypair.secret_key()),
            message
        );
    }

    #[test]
    fn supported_curve_groups_encrypt_and_decrypt() {
        encrypt_decrypt_over_curve_group::<Group<Secp256k1>>();
        encrypt_decrypt_over_curve_group::<Group<Secp256r1>>();
        encrypt_decrypt_over_curve_group::<Group<Bls12381G1>>();
        encrypt_decrypt_over_curve_group::<Ristretto255Group>();
    }
}
