//! ElGamal encryption over an abstract cyclic group.
//!
//! With the group written additively, encryption of a group element `m` under
//! public key `h = xG` is:
//!
//! 1. choose fresh random scalar `r`
//! 2. compute `c1 = rG`
//! 3. compute shared secret `s = rh`
//! 4. compute `c2 = m + s`
//!
//! Decryption computes `m = c2 - x c1`.

use std::marker::PhantomData;

use crate::ecc::group::CyclicGroup;
use crate::ecc::group::GroupOps;

/// Ciphertext pairs over one cyclic group.
pub type GroupCiphertext<G> = Vec<(<G as GroupOps>::Element, <G as GroupOps>::Element)>;

/// Generic ElGamal implementation parameterized only by a cyclic group.
pub struct ElGamal<G>(PhantomData<G>);

impl<G> ElGamal<G>
where G: CyclicGroup
{
    /// Encrypt group elements under the given public group element.
    pub fn encrypt<I>(message: I, public_key: G::Element) -> GroupCiphertext<G>
    where I: IntoIterator<Item = G::Element> {
        message
            .into_iter()
            .map(|message_element| {
                let ephemeral_scalar = G::random_scalar();
                let shared_secret = public_key.clone() * ephemeral_scalar.clone();
                let c1 = G::generator_mul(ephemeral_scalar);
                let c2 = message_element + shared_secret;
                (c1, c2)
            })
            .collect()
    }

    /// Decrypt ciphertext pairs into group elements with the given scalar.
    pub fn decrypt(
        ciphertext: &[(G::Element, G::Element)],
        secret_key: G::Scalar,
    ) -> Vec<G::Element> {
        ciphertext
            .iter()
            .map(|(c1, c2)| c2.clone() + -(c1.clone() * secret_key.clone()))
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

        fn random_scalar() -> Self::Scalar {
            TestScalar(rand::thread_rng().gen_range(1..TEST_GROUP_ORDER))
        }
    }

    #[test]
    fn encrypt_decrypt_over_generic_finite_group() {
        let secret_key = TestScalar(12_345);
        let public_key = TestGroup::generator() * secret_key;
        let message = vec![TestElement(1), TestElement(42), TestElement(65_520)];
        let ciphertext = ElGamal::<TestGroup>::encrypt(message.clone(), public_key);

        assert_eq!(
            ElGamal::<TestGroup>::decrypt(&ciphertext, secret_key),
            message
        );
    }

    #[test]
    fn encryption_uses_fresh_ephemeral_point_per_block() {
        let secret_key = TestScalar(42);
        let public_key = TestGroup::generator() * secret_key;
        let message = vec![TestElement(7); 4];
        let ciphertext = ElGamal::<TestGroup>::encrypt(message, public_key);

        assert!(ciphertext.windows(2).any(|pair| pair[0].0 != pair[1].0));
    }
}
