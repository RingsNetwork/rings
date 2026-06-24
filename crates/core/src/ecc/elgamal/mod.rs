//! ElGamal encryption over an abstract cyclic group.
//! ----------------
//! Algorithm Description
//!
//! ElGamal is a public-key encryption algorithm over a finite cyclic group. It
//! is not tied to a particular elliptic curve; an elliptic curve group is one
//! possible implementation of the group operation.
//!
//! # Encrypt
//! Bob encrypts a message `M` to Alice under her public key `(G, q, g, h)`,
//! where `G` is a cyclic group of order `q`, `g` is a generator, and
//! `h = xg` is Alice's public key.
//!
//! 1. Map the message `M` to an element `m` of `G` using a reversible mapping.
//! 2. Choose a fresh random scalar `y` from `{1, ..., q - 1}`.
//! 3. Compute the shared secret `s := yh`.
//! 4. Compute `c1 := yg`.
//! 5. Compute `c2 := m + s` in additive notation.
//! 6. Send the ciphertext `(c1, c2)` to Alice.
//!
//! # Decrypt
//! Alice decrypts `(c1, c2)` with private scalar `x`:
//!
//! 1. Compute `s := xc1`.
//! 2. Compute the inverse of `s`, written `-s` in additive notation.
//! 3. Recover `m := c2 - s`.
//!
//! In multiplicative notation, `c2 := m * s` and decryption computes
//! `m := c2 * s^{-1}`. The implementation below uses additive notation because
//! that is the natural convention for elliptic curve groups.
//!
//! ref:
//! - T. ElGamal. A Public Key Cryptosystem and a Signature Scheme Based on
//!   Discrete Logarithms. IEEE Trans. Info. Theory, IT 31:469-472, 1985.
//! - ElGamal encryption <https://en.wikipedia.org/wiki/ElGamal_encryption>
//! - <http://www.docsdrive.com/pdfs/ansinet/itj/2005/299-306.pdf>

use std::marker::PhantomData;

use crate::ecc::group::Bls12381G1;
use crate::ecc::group::CryptographicGroup;
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
where G: CryptographicGroup
{
    /// Generate a fresh non-zero ElGamal secret scalar.
    pub fn random() -> Self {
        Self::from_scalar(G::random_scalar())
    }

    /// Derive the public key `h = xg`.
    pub fn public_key(&self) -> ElGamalPublicKey<G> {
        ElGamalPublicKey::from_element(G::generator_mul(self.scalar.clone()))
    }
}

impl<G> ElGamalKeyPair<G>
where G: CryptographicGroup
{
    /// Generate a fresh ElGamal key pair.
    pub fn random() -> Self {
        let secret = ElGamalSecretKey::<G>::random();
        let public = secret.public_key();
        Self { secret, public }
    }

    /// Borrow the public key.
    pub fn public_key(&self) -> &ElGamalPublicKey<G> {
        &self.public
    }

    /// Borrow the secret key.
    pub fn secret_key(&self) -> &ElGamalSecretKey<G> {
        &self.secret
    }
}

impl<G> ElGamal<G>
where G: CryptographicGroup
{
    /// Encrypt group elements under the given public group element.
    pub fn encrypt<I>(message: I, public_key: &ElGamalPublicKey<G>) -> GroupCiphertext<G>
    where I: IntoIterator<Item = G::Element> {
        message
            .into_iter()
            .map(|message_element| {
                let ephemeral_scalar = G::random_scalar();
                let shared_secret = public_key.as_element().clone() * ephemeral_scalar.clone();
                let c1 = G::generator_mul(ephemeral_scalar);
                let c2 = message_element + shared_secret;
                (c1, c2)
            })
            .collect()
    }

    /// Decrypt ciphertext pairs into group elements with the given scalar.
    pub fn decrypt(
        ciphertext: &[(G::Element, G::Element)],
        secret_key: &ElGamalSecretKey<G>,
    ) -> Vec<G::Element> {
        ciphertext
            .iter()
            .map(|(c1, c2)| c2.clone() + -(c1.clone() * secret_key.as_scalar().clone()))
            .collect()
    }
}

#[cfg(test)]
mod test {
    use std::ops::Add;
    use std::ops::Mul;
    use std::ops::Neg;

    use rand::Rng;

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
        fn random_scalar() -> Self::Scalar {
            TestScalar(rand::thread_rng().gen_range(1..TEST_GROUP_ORDER))
        }
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
        let ciphertext = ElGamal::<TestGroup>::encrypt(message, &public_key);

        assert!(ciphertext.windows(2).any(|pair| pair[0].0 != pair[1].0));
    }

    fn encrypt_decrypt_over_curve_group<G>()
    where
        G: CryptographicGroup,
        G::Element: Eq + std::fmt::Debug,
    {
        let keypair = ElGamalKeyPair::<G>::random();
        let message = vec![
            G::generator(),
            G::generator_mul(G::random_scalar()),
            G::identity(),
        ];
        let ciphertext = ElGamal::<G>::encrypt(message.clone(), keypair.public_key());

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
