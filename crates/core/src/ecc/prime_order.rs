//! Non-degenerate elements of a prime-order group: `G ∖ {O}` and `Z_n^*`.
//!
//! In a group of prime order `n` (secp256k1 has cofactor 1), the module action restricts to
//!
//! ```text
//! · : (G ∖ {O}) × Z_n^* → G ∖ {O}          P·k = O ⇔ P = O ∨ k ≡ 0 (mod n)
//! · : Z_n^* × Z_n^* → Z_n^*                  (Z_n^* is the multiplicative group of the field)
//! ```
//!
//! so [`NonIdentityPoint`] and [`NonZeroScalar`] are closed under it, and a protocol that only
//! multiplies non-identity points by non-zero scalars (Diffie–Hellman, Sphinx blinding) never
//! meets `O`: the degenerate case is unrepresentable rather than checked.
//!
//! Laws:
//!
//! - **SEC1.** `PublicKey<33> ⇀ NonIdentityPoint` (decoding, partial) and
//!   `NonIdentityPoint → PublicKey<33>` (encoding, total) are mutually inverse on valid strings:
//!   `O` has no 33-byte encoding.
//! - **Diffie–Hellman.** `(x·G)·d` and `(d·G)·x` have the same [`NonIdentityPoint::shared_secret`]
//!   `x(P·k)`, the affine x-coordinate, zeroized on drop.
//! - **Wide reduction.** [`NonZeroScalar::from_wide_bytes`] maps 512 uniform bits to `Z_n` with
//!   statistical distance `< 2^−256` from uniform and rejects `0`.

use std::ops::Mul;

use elliptic_curve::bigint::Encoding;
use elliptic_curve::bigint::U512;
use elliptic_curve::ops::MulByGenerator;
use elliptic_curve::ops::Reduce;
use elliptic_curve::point::AffineCoordinates;
use elliptic_curve::sec1::ToEncodedPoint;
use k256::NonZeroScalar as K256NonZeroScalar;
use k256::ProjectivePoint as K256ProjectivePoint;
use k256::Scalar as K256Scalar;
use rand::CryptoRng;
use rand::RngCore;
use zeroize::Zeroize;
use zeroize::Zeroizing;

use crate::ecc::group::CurveGroup;
use crate::ecc::group::Secp256k1;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// Width of a SEC1 compressed secp256k1 point.
const SEC1_COMPRESSED_BYTES: usize = 33;

/// Width of a shared secret, the affine x-coordinate.
pub const SHARED_SECRET_BYTES: usize = 32;

/// Width of the input of [`NonZeroScalar::from_wide_bytes`].
pub const WIDE_SCALAR_BYTES: usize = 64;

/// An element of `Z_n^*` for curve `C`; zeroized on drop.
pub struct NonZeroScalar<C: PrimeOrderGroup>(C::NonZeroScalar);

/// An element of `G ∖ {O}` for curve `C`.
pub struct NonIdentityPoint<C: PrimeOrderGroup>(C::Point);

/// A curve group of prime order, with a native type of `Z_n^*`.
///
/// Implementors guarantee that the group order is prime, so that the laws of the module
/// documentation hold.
pub trait PrimeOrderGroup: CurveGroup {
    /// The native type of `Z_n^*`, zeroized by [`Zeroize`].
    type NonZeroScalar: Zeroize;
}

impl PrimeOrderGroup for Secp256k1 {
    type NonZeroScalar = K256NonZeroScalar;
}

impl NonZeroScalar<Secp256k1> {
    /// A uniform element of `Z_n^*`.
    pub fn random_with_rng(rng: &mut (impl CryptoRng + RngCore)) -> Self {
        Self(K256NonZeroScalar::random(rng))
    }

    /// `k = w mod n` for 64 bytes `w`, big-endian; `None` when `k = 0` (probability `2^−256`).
    pub fn from_wide_bytes(bytes: &[u8; WIDE_SCALAR_BYTES]) -> Option<Self> {
        let wide = Zeroizing::new(U512::from_be_bytes(*bytes));
        K256NonZeroScalar::new(<K256Scalar as Reduce<U512>>::reduce(*wide))
            .into_option()
            .map(Self)
    }

    /// The secret scalar of a secp256k1 key, non-zero by the key's invariant.
    pub(crate) fn from_secret_key(key: &SecretKey) -> Self {
        Self(key.secp256k1_nonzero_scalar())
    }
}

impl Mul<&NonZeroScalar<Secp256k1>> for &NonZeroScalar<Secp256k1> {
    type Output = NonZeroScalar<Secp256k1>;

    /// The product in `Z_n^*`.
    fn mul(self, rhs: &NonZeroScalar<Secp256k1>) -> Self::Output {
        NonZeroScalar(self.0 * rhs.0)
    }
}

impl NonIdentityPoint<Secp256k1> {
    /// `k·G`, non-identity because `G` generates the prime-order group and `k ≠ 0`.
    pub fn generator_mul(scalar: &NonZeroScalar<Secp256k1>) -> Self {
        Self(K256ProjectivePoint::mul_by_generator(scalar.0.as_ref()))
    }

    /// `x(P·k)`, the Diffie–Hellman shared secret of `P` and `k`; zeroized on drop.
    pub fn shared_secret(
        &self,
        scalar: &NonZeroScalar<Secp256k1>,
    ) -> Zeroizing<[u8; SHARED_SECRET_BYTES]> {
        Zeroizing::new((self * scalar).0.to_affine().x().into())
    }

    /// The SEC1 compressed encoding, total on `G ∖ {O}`.
    pub fn to_sec1(&self) -> PublicKey<SEC1_COMPRESSED_BYTES> {
        let encoded = self.0.to_affine().to_encoded_point(true);
        let mut bytes = [0_u8; SEC1_COMPRESSED_BYTES];
        bytes
            .iter_mut()
            .zip(encoded.as_bytes())
            .for_each(|(slot, byte)| *slot = *byte);
        PublicKey(bytes)
    }
}

impl Mul<&NonZeroScalar<Secp256k1>> for &NonIdentityPoint<Secp256k1> {
    type Output = NonIdentityPoint<Secp256k1>;

    /// The module action `P·k`, closed on `G ∖ {O}` by primality.
    fn mul(self, rhs: &NonZeroScalar<Secp256k1>) -> Self::Output {
        NonIdentityPoint(self.0 * rhs.0.as_ref())
    }
}

impl TryFrom<PublicKey<SEC1_COMPRESSED_BYTES>> for NonIdentityPoint<Secp256k1> {
    type Error = Error;

    /// Decode a SEC1 compressed point; every valid 33-byte encoding denotes a point `≠ O`.
    fn try_from(encoded: PublicKey<SEC1_COMPRESSED_BYTES>) -> Result<Self> {
        k256::PublicKey::try_from(encoded).map(|key| Self(key.to_projective()))
    }
}

impl<C: PrimeOrderGroup> Drop for NonZeroScalar<C> {
    /// Zeroizes the scalar.
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    use super::NonIdentityPoint;
    use super::NonZeroScalar;
    use crate::delegation::DelegateeKey;
    use crate::ecc::group::Secp256k1;
    use crate::ecc::PublicKey;
    use crate::ecc::SecretKey;

    /// The secp256k1 group order `n`, big-endian.
    const ORDER: [u8; 32] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36,
        0x41, 0x41,
    ];

    /// `x(d·(x·G)) = x(x·(d·G))`: the delegatee and the sender derive one secret.
    #[test]
    fn test_diffie_hellman_commutes() {
        let mut rng = StdRng::seed_from_u64(1);
        let key = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("delegation");
        let sender = NonZeroScalar::<Secp256k1>::random_with_rng(&mut rng);
        let recipient = NonIdentityPoint::<Secp256k1>::try_from(key.delegatee_public_key())
            .expect("delegatee key is a point");

        assert_eq!(
            *key.diffie_hellman(&NonIdentityPoint::generator_mul(&sender)),
            *recipient.shared_secret(&sender)
        );
    }

    /// SEC1 decoding inverts encoding, and the all-zero string (no point) is rejected.
    #[test]
    fn test_sec1_round_trip() {
        let mut rng = StdRng::seed_from_u64(2);
        let point =
            NonIdentityPoint::generator_mul(&NonZeroScalar::<Secp256k1>::random_with_rng(&mut rng));
        let encoded = point.to_sec1();

        let decoded = NonIdentityPoint::<Secp256k1>::try_from(encoded).expect("valid point");

        assert_eq!(decoded.to_sec1(), encoded);
        assert!(NonIdentityPoint::<Secp256k1>::try_from(PublicKey([0; 33])).is_err());
    }

    /// Wide reduction maps `0` and `n` to `0`, which is rejected, and `n + 1` to `1`.
    #[test]
    fn test_wide_reduction_rejects_zero() {
        let mut order = [0_u8; 64];
        order[32..].copy_from_slice(&ORDER);
        let mut successor = order;
        successor[63] += 1;

        assert!(NonZeroScalar::<Secp256k1>::from_wide_bytes(&[0; 64]).is_none());
        assert!(NonZeroScalar::<Secp256k1>::from_wide_bytes(&order).is_none());
        let one = NonZeroScalar::<Secp256k1>::from_wide_bytes(&successor).expect("n + 1 ≡ 1");
        let mut unit = [0_u8; 64];
        unit[63] = 1;
        let expected = NonZeroScalar::<Secp256k1>::from_wide_bytes(&unit).expect("1");
        assert_eq!(
            NonIdentityPoint::generator_mul(&one).to_sec1(),
            NonIdentityPoint::generator_mul(&expected).to_sec1()
        );
    }
}
