//! Non-degenerate subtypes of the group carriers of [`crate::ecc::group`]: `G ∖ {O}` and `Z_n^*`.
//!
//! For a curve of prime order `n` the module action restricts to the subtypes,
//!
//! ```text
//! ι : NonIdentityPoint<C> ↪ Point<C>          (From; the partial inverse is TryFrom, rejecting O)
//! ι : NonZeroScalar<C>    ↪ Scalar<C>         (From; the partial inverse is TryFrom, rejecting 0)
//! · : (G ∖ {O}) × Z_n^* → G ∖ {O}             P·k = O ⇔ P = O ∨ k ≡ 0 (mod n)
//! · : Z_n^* × Z_n^* → Z_n^*                   Z_n^* is the multiplicative group of the field
//! ```
//!
//! and `ι` commutes with the action: `ι(P·k) = ι(P)·ι(k)`. A protocol that only multiplies
//! non-identity points by non-zero scalars (Diffie–Hellman, Sphinx-style blinding) therefore
//! never meets `O`: the degenerate case is unrepresentable rather than checked. Sampling and the
//! SEC1 codec are those of the carriers, restricted: [`NonZeroScalar::random_with_rng`] is
//! [`CurveScalarField::random_scalar_with_rng`], and the secp256k1 SEC1 encoding of
//! `Point<Secp256k1>` factors through `NonIdentityPoint<Secp256k1>`, where it is total.

use std::ops::Mul;

use elliptic_curve::bigint::U512;
use elliptic_curve::ops::Reduce;
use elliptic_curve::point::AffineCoordinates;
use elliptic_curve::sec1::ToEncodedPoint;
use k256::Scalar as K256Scalar;
use k256::WideBytes;
use rand::RngCore;
use zeroize::Zeroize;
use zeroize::Zeroizing;

use crate::algebra::Zero;
use crate::ecc::group::CurveScalarField;
use crate::ecc::group::Point;
use crate::ecc::group::Scalar;
use crate::ecc::group::Secp256k1;
use crate::ecc::group::Secp256r1;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// Width of a SEC1 compressed secp256k1 point.
const SEC1_COMPRESSED_BYTES: usize = 33;

/// Width of a Diffie–Hellman shared secret, the affine x-coordinate.
pub const SHARED_SECRET_BYTES: usize = 32;

/// Width of the input of [`NonZeroScalar::from_wide_bytes`].
pub const WIDE_SCALAR_BYTES: usize = 64;

/// A curve whose point carrier represents a group of prime order `n`, with zeroizable scalars.
///
/// Law: the order of the group of `Point<C>` is prime, so the restrictions of the module
/// documentation are closed. This is a proof obligation of the implementor, as for [`Eq`].
pub trait PrimeOrder: CurveScalarField<Scalar: Zeroize> {}

/// secp256k1 has cofactor 1.
impl PrimeOrder for Secp256k1 {}

/// secp256r1 has cofactor 1.
impl PrimeOrder for Secp256r1 {}

/// An element of `G ∖ {O}`: a [`Point`] other than the identity.
pub struct NonIdentityPoint<C: PrimeOrder>(Point<C>);

/// An element of `Z_n^*`: a [`Scalar`] other than zero, zeroized on drop.
pub struct NonZeroScalar<C: PrimeOrder>(Scalar<C>);

impl<C: PrimeOrder> Clone for NonIdentityPoint<C> {
    /// Copies the point.
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<C: PrimeOrder> From<NonIdentityPoint<C>> for Point<C> {
    /// The inclusion `ι : G ∖ {O} ↪ G`.
    fn from(point: NonIdentityPoint<C>) -> Self {
        point.0
    }
}

impl<C: PrimeOrder> TryFrom<Point<C>> for NonIdentityPoint<C> {
    type Error = Error;

    /// The partial inverse of `ι`, rejecting `O`.
    fn try_from(point: Point<C>) -> Result<Self> {
        if point == Point::zero() {
            Err(Error::InvalidPublicKey)
        } else {
            Ok(Self(point))
        }
    }
}

impl<C: PrimeOrder> From<&NonZeroScalar<C>> for Scalar<C> {
    /// The inclusion `ι : Z_n^* ↪ Z_n`.
    fn from(scalar: &NonZeroScalar<C>) -> Self {
        scalar.0.clone()
    }
}

impl<C: PrimeOrder> TryFrom<Scalar<C>> for NonZeroScalar<C> {
    type Error = Error;

    /// The partial inverse of `ι`, rejecting `0`.
    fn try_from(scalar: Scalar<C>) -> Result<Self> {
        if C::scalar_is_zero(scalar.as_inner()) {
            Err(Error::InvalidPublicKey)
        } else {
            Ok(Self(scalar))
        }
    }
}

impl<C: PrimeOrder> NonZeroScalar<C> {
    /// A uniform element of `Z_n^*`: the carrier's non-zero sampler.
    pub fn random_with_rng(rng: &mut impl RngCore) -> Self {
        Self(Scalar::new(C::random_scalar_with_rng(rng)))
    }
}

impl<C: PrimeOrder> NonIdentityPoint<C> {
    /// `k·G`, non-identity because `G` generates the prime-order group and `k ≠ 0`.
    pub fn generator_mul(scalar: &NonZeroScalar<C>) -> Self {
        Self(Point::new(C::generator_mul(scalar.0.as_inner())))
    }
}

impl<C: PrimeOrder> Mul<&NonZeroScalar<C>> for &NonZeroScalar<C> {
    type Output = NonZeroScalar<C>;

    /// The product in `Z_n^*`.
    fn mul(self, rhs: &NonZeroScalar<C>) -> Self::Output {
        NonZeroScalar(self.0.clone() * rhs.0.clone())
    }
}

impl<C: PrimeOrder> Mul<&NonZeroScalar<C>> for &NonIdentityPoint<C> {
    type Output = NonIdentityPoint<C>;

    /// The module action `P·k`, closed on `G ∖ {O}` by primality.
    fn mul(self, rhs: &NonZeroScalar<C>) -> Self::Output {
        NonIdentityPoint(self.0.clone() * rhs.0.clone())
    }
}

impl<C: PrimeOrder> Drop for NonZeroScalar<C> {
    /// Zeroizes the scalar.
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl NonZeroScalar<Secp256k1> {
    /// `k = w mod n` for 64 big-endian bytes `w`, within `2^−256` of uniform for uniform `w`;
    /// `None` when `k = 0`.
    pub fn from_wide_bytes(bytes: &[u8; WIDE_SCALAR_BYTES]) -> Option<Self> {
        let mut wide = Zeroizing::new(WideBytes::default());
        wide.iter_mut()
            .zip(bytes.iter())
            .for_each(|(slot, byte)| *slot = *byte);
        Self::try_from(Scalar::new(<K256Scalar as Reduce<U512>>::reduce_bytes(
            &wide,
        )))
        .ok()
    }

    /// The secret scalar of a secp256k1 key, non-zero by the key's invariant.
    pub(crate) fn from_secret_key(key: &SecretKey) -> Self {
        Self(Scalar::new(key.secp256k1_scalar()))
    }
}

impl NonIdentityPoint<Secp256k1> {
    /// `x(P·k)`, the Diffie–Hellman shared secret of `P` and `k`, zeroized on drop.
    ///
    /// Law: `x((d·G)·x) = x((x·G)·d)`, so both parties of an exchange derive one secret.
    pub fn shared_secret(
        &self,
        scalar: &NonZeroScalar<Secp256k1>,
    ) -> Zeroizing<[u8; SHARED_SECRET_BYTES]> {
        Zeroizing::new((self * scalar).0.as_inner().to_affine().x().into())
    }
}

impl From<&NonIdentityPoint<Secp256k1>> for PublicKey<SEC1_COMPRESSED_BYTES> {
    /// The SEC1 compressed encoding, total on `G ∖ {O}`: every non-identity point has a
    /// 33-byte encoding.
    fn from(point: &NonIdentityPoint<Secp256k1>) -> Self {
        let encoded = point.0.as_inner().to_affine().to_encoded_point(true);
        let mut bytes = [0_u8; SEC1_COMPRESSED_BYTES];
        bytes
            .iter_mut()
            .zip(encoded.as_bytes())
            .for_each(|(slot, byte)| *slot = *byte);
        Self(bytes)
    }
}

impl TryFrom<PublicKey<SEC1_COMPRESSED_BYTES>> for NonIdentityPoint<Secp256k1> {
    type Error = Error;

    /// SEC1 decoding of the carrier, then the subtype check.
    fn try_from(encoded: PublicKey<SEC1_COMPRESSED_BYTES>) -> Result<Self> {
        Point::<Secp256k1>::try_from(encoded).and_then(Self::try_from)
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    use super::NonIdentityPoint;
    use super::NonZeroScalar;
    use crate::algebra::Zero;
    use crate::delegation::DelegateeKey;
    use crate::ecc::group::Point;
    use crate::ecc::group::Scalar;
    use crate::ecc::group::Secp256k1;
    use crate::ecc::PublicKey;
    use crate::ecc::SecretKey;

    /// The secp256k1 group order `n`, big-endian.
    const ORDER: [u8; 32] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36,
        0x41, 0x41,
    ];

    /// A fixed delegatee key, so the test is deterministic.
    fn fixture_key() -> DelegateeKey {
        let secret = SecretKey::try_from("07".repeat(32).as_str()).expect("fixture scalar");
        DelegateeKey::new_with_seckey(&secret).expect("fixture delegation")
    }

    /// `x(d·(x·G)) = x(x·(d·G))`: the delegatee and the sender derive one secret.
    #[test]
    fn test_diffie_hellman_commutes() {
        let key = fixture_key();
        let sender = NonZeroScalar::<Secp256k1>::random_with_rng(&mut StdRng::seed_from_u64(1));
        let recipient = NonIdentityPoint::<Secp256k1>::try_from(key.delegatee_public_key())
            .expect("delegatee key is a point");

        assert_eq!(
            *key.diffie_hellman(&NonIdentityPoint::generator_mul(&sender)),
            *recipient.shared_secret(&sender)
        );
    }

    /// The inclusions reject exactly the degenerate elements, and `ι` commutes with the action.
    #[test]
    fn test_inclusions_and_action() {
        let mut rng = StdRng::seed_from_u64(2);
        let point =
            NonIdentityPoint::generator_mul(&NonZeroScalar::<Secp256k1>::random_with_rng(&mut rng));
        let scalar = NonZeroScalar::<Secp256k1>::random_with_rng(&mut rng);

        assert!(NonIdentityPoint::<Secp256k1>::try_from(Point::zero()).is_err());
        assert!(NonZeroScalar::<Secp256k1>::try_from(Scalar::zero()).is_err());
        assert!(
            Point::from(&point * &scalar) == Point::from(point.clone()) * Scalar::from(&scalar)
        );
    }

    /// SEC1 decoding inverts the total encoding, and the all-zero string (no point) is rejected.
    #[test]
    fn test_sec1_round_trip() {
        let point = NonIdentityPoint::generator_mul(&NonZeroScalar::<Secp256k1>::random_with_rng(
            &mut StdRng::seed_from_u64(3),
        ));
        let encoded = PublicKey::from(&point);

        let decoded = NonIdentityPoint::<Secp256k1>::try_from(encoded).expect("valid point");

        assert_eq!(PublicKey::from(&decoded), encoded);
        assert!(NonIdentityPoint::<Secp256k1>::try_from(PublicKey([0; 33])).is_err());
    }

    /// Wide reduction maps `0` and `n` to `0`, which is rejected, and `n + 1` to `1`.
    #[test]
    fn test_wide_reduction_rejects_zero() {
        let mut order = [0_u8; 64];
        order[32..].copy_from_slice(&ORDER);
        let mut successor = order;
        successor[63] += 1;
        let mut unit = [0_u8; 64];
        unit[63] = 1;

        assert!(NonZeroScalar::<Secp256k1>::from_wide_bytes(&[0; 64]).is_none());
        assert!(NonZeroScalar::<Secp256k1>::from_wide_bytes(&order).is_none());
        let one = NonZeroScalar::<Secp256k1>::from_wide_bytes(&successor).expect("n + 1 ≡ 1");
        let expected = NonZeroScalar::<Secp256k1>::from_wide_bytes(&unit).expect("1");
        assert!(Scalar::from(&one) == Scalar::from(&expected));
    }
}
