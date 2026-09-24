//! Non-degenerate subtypes of the group carriers of [`crate::ecc::group`]: `G ∖ {O}` and `Z_n^*`.
//!
//! For a curve of prime order `n` the module action restricts to the subtypes,
//!
//! ```text
//! ι : NonIdentityPoint<C> ↪ Point<C>          (From; the partial inverse is TryFrom, rejecting O)
//! ι : NonZeroScalar<C>    ↪ Scalar<C>         (TryFrom from the carrier, rejecting 0)
//! · : (G ∖ {O}) × Z_n^* → G ∖ {O}             P·k = O ⇔ P = O ∨ k ≡ 0 (mod n)
//! · : Z_n^* × Z_n^* → Z_n^*                   Z_n^* is the multiplicative group of the field
//! ```
//!
//! and `ι` commutes with the action. A protocol that only multiplies non-identity points by
//! non-zero scalars (Diffie–Hellman, Sphinx-style blinding) never meets `O`: the degenerate case
//! is unrepresentable rather than checked. [`PrimeOrder`] is sealed, since its law is a proof
//! obligation no downstream implementation can be held to.
//!
//! This module holds the one SEC1 compressed encoder of secp256k1, on the affine carrier: every
//! conversion of a point, public key or verifying key into `PublicKey<33>` factors through it,
//! already-affine keys without a field inversion, and the SEC1 decoder produces
//! `G ∖ {O}` directly from `k256::PublicKey`, which already excludes `O`. Sampling is the
//! carrier's non-zero sampler.

use std::ops::Mul;

use elliptic_curve::bigint::U512;
use elliptic_curve::group::GroupEncoding;
use elliptic_curve::ops::Reduce;
use elliptic_curve::point::AffineCoordinates;
use k256::AffinePoint as K256AffinePoint;
use k256::PublicKey as K256PublicKey;
use k256::Scalar as K256Scalar;
use k256::WideBytes;
use rand::CryptoRng;
use rand::RngCore;
use zeroize::Zeroize;
use zeroize::Zeroizing;

use crate::algebra::Zero;
use crate::ecc::group::CurveScalarField;
use crate::ecc::group::Point;
use crate::ecc::group::Scalar;
use crate::ecc::group::Secp256k1;
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

/// The seal of [`PrimeOrder`]: only this crate names a curve prime-order.
mod sealed {
    /// Implemented exactly for the curves this crate has proved prime-order.
    pub trait Sealed {}
}

/// A curve whose point carrier represents a group of prime order `n`, with zeroizable scalars.
///
/// Law: the order of the group of `Point<C>` is prime, so the restrictions of the module
/// documentation are closed. Sealed: implemented only here, for curves of cofactor 1.
pub trait PrimeOrder: CurveScalarField<Scalar: Zeroize> + sealed::Sealed {}

impl sealed::Sealed for Secp256k1 {}

/// secp256k1 has cofactor 1.
impl PrimeOrder for Secp256k1 {}

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
            Err(Error::IdentityElement)
        } else {
            Ok(Self(point))
        }
    }
}

impl<C: PrimeOrder> TryFrom<Scalar<C>> for NonZeroScalar<C> {
    type Error = Error;

    /// The restriction of the carrier to `Z_n^*`, rejecting `0`.
    fn try_from(scalar: Scalar<C>) -> Result<Self> {
        if C::scalar_is_zero(scalar.as_inner()) {
            Err(Error::ZeroScalar)
        } else {
            Ok(Self(scalar))
        }
    }
}

impl<C: PrimeOrder> NonZeroScalar<C> {
    /// A uniform element of `Z_n^*` from a cryptographic RNG: the carrier's non-zero sampler.
    pub fn random_with_rng(rng: &mut (impl CryptoRng + RngCore)) -> Self {
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
        NonZeroScalar(Scalar::new(C::scalar_mul(
            self.0.as_inner(),
            rhs.0.as_inner(),
        )))
    }
}

impl<C: PrimeOrder> Mul<&NonZeroScalar<C>> for &NonIdentityPoint<C> {
    type Output = NonIdentityPoint<C>;

    /// The module action `P·k`, closed on `G ∖ {O}` by primality.
    fn mul(self, rhs: &NonZeroScalar<C>) -> Self::Output {
        NonIdentityPoint(Point::new(C::mul(self.0.as_inner(), rhs.0.as_inner())))
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

    /// The secret scalar of a secp256k1 key, non-zero by the key's invariant. The key's accessor
    /// returns a copy; this value, not that transient, is the one zeroized on drop.
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

impl From<K256PublicKey> for NonIdentityPoint<Secp256k1> {
    /// A `k256` public key is a point `≠ O` by its own invariant.
    fn from(key: K256PublicKey) -> Self {
        Self(Point::new(key.to_projective()))
    }
}

impl From<&NonIdentityPoint<Secp256k1>> for PublicKey<SEC1_COMPRESSED_BYTES> {
    /// The SEC1 compressed encoding of a projective point: one normalisation, then the encoder.
    fn from(point: &NonIdentityPoint<Secp256k1>) -> Self {
        encode_sec1(&point.0.as_inner().to_affine())
    }
}

/// The one SEC1 compressed encoder of secp256k1, on the affine carrier, so an input that is
/// already affine (a `k256` public or verifying key) costs no field inversion.
///
/// Pre: `point ≠ O`: every caller holds an element of `G ∖ {O}` (a [`NonIdentityPoint`] or a
/// `k256::PublicKey`), on which the encoding is total and exactly 33 bytes.
fn encode_sec1(point: &K256AffinePoint) -> PublicKey<SEC1_COMPRESSED_BYTES> {
    let encoded = point.to_bytes();
    let mut bytes = [0_u8; SEC1_COMPRESSED_BYTES];
    bytes
        .iter_mut()
        .zip(encoded.iter())
        .for_each(|(slot, byte)| *slot = *byte);
    PublicKey(bytes)
}

impl TryFrom<PublicKey<SEC1_COMPRESSED_BYTES>> for NonIdentityPoint<Secp256k1> {
    type Error = Error;

    /// SEC1 decoding, straight into `G ∖ {O}`: every valid encoding denotes a point `≠ O`.
    fn try_from(encoded: PublicKey<SEC1_COMPRESSED_BYTES>) -> Result<Self> {
        K256PublicKey::try_from(encoded).map(Self::from)
    }
}

impl TryFrom<Point<Secp256k1>> for PublicKey<SEC1_COMPRESSED_BYTES> {
    type Error = Error;

    /// SEC1 compressed encoding of the carrier: defined exactly on `G ∖ {O}`.
    fn try_from(point: Point<Secp256k1>) -> Result<Self> {
        NonIdentityPoint::try_from(point).map(|point| Self::from(&point))
    }
}

impl TryFrom<K256AffinePoint> for PublicKey<SEC1_COMPRESSED_BYTES> {
    type Error = Error;

    /// SEC1 compressed encoding of an affine point: defined exactly on `G ∖ {O}`.
    fn try_from(point: K256AffinePoint) -> Result<Self> {
        Self::try_from(Point::<Secp256k1>::from(point))
    }
}

impl From<K256PublicKey> for PublicKey<SEC1_COMPRESSED_BYTES> {
    /// SEC1 compressed encoding of a `k256` public key, already affine and `≠ O`.
    fn from(key: K256PublicKey) -> Self {
        encode_sec1(key.as_affine())
    }
}

impl From<k256::ecdsa::VerifyingKey> for PublicKey<SEC1_COMPRESSED_BYTES> {
    /// SEC1 compressed encoding of an ECDSA verifying key, already affine and `≠ O`.
    fn from(key: k256::ecdsa::VerifyingKey) -> Self {
        encode_sec1(key.as_affine())
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

    /// The point `k·G` for the wide encoding of `k`, compared by its SEC1 encoding.
    fn sec1_of(scalar: &NonZeroScalar<Secp256k1>) -> PublicKey<33> {
        PublicKey::from(&NonIdentityPoint::generator_mul(scalar))
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

    /// The inclusions reject exactly the degenerate elements, and `ι` commutes with the action:
    /// `ι(P·k) = ι(P)·k` in the carrier.
    #[test]
    fn test_inclusions_and_action() {
        let mut rng = StdRng::seed_from_u64(2);
        let base = NonZeroScalar::<Secp256k1>::random_with_rng(&mut rng);
        let point = NonIdentityPoint::generator_mul(&base);
        let scalar = NonZeroScalar::<Secp256k1>::random_with_rng(&mut rng);

        assert!(NonIdentityPoint::<Secp256k1>::try_from(Point::zero()).is_err());
        assert!(NonZeroScalar::<Secp256k1>::try_from(Scalar::zero()).is_err());
        assert!(
            Point::from(&point * &scalar)
                == Point::from(NonIdentityPoint::generator_mul(&(&base * &scalar)))
        );
    }

    /// SEC1 decoding inverts the one encoder, the carrier encodings agree with it, and the
    /// all-zero string (no point) is rejected.
    #[test]
    fn test_sec1_round_trip() {
        let point = NonIdentityPoint::generator_mul(&NonZeroScalar::<Secp256k1>::random_with_rng(
            &mut StdRng::seed_from_u64(3),
        ));
        let encoded = PublicKey::from(&point);

        let decoded = NonIdentityPoint::<Secp256k1>::try_from(encoded).expect("valid point");

        assert_eq!(PublicKey::from(&decoded), encoded);
        assert_eq!(
            PublicKey::from(k256::PublicKey::try_from(encoded).expect("valid key")),
            encoded
        );
        assert_eq!(PublicKey::try_from(Point::from(point)).ok(), Some(encoded));
        assert!(NonIdentityPoint::<Secp256k1>::try_from(PublicKey([0; 33])).is_err());
        assert!(PublicKey::<33>::try_from(Point::<Secp256k1>::zero()).is_err());
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
        assert_eq!(sec1_of(&one), sec1_of(&expected));
    }
}
