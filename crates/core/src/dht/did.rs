#![warn(missing_docs)]

//! Distributed identities for the Rings DHT.
//!
//! A [`Did`] is a protocol identity located on the Chord identifier circle. It
//! is also the concrete carrier for the additive cyclic group of `Z / 2^160`
//! used by Chord routing and placement. The [`crate::algebra::AbelianGroup`]
//! trait names the operation set; `Did` supplies the representation and
//! implementation. `Did` does not implement [`crate::algebra::CommutativeRing`]
//! because Chord never uses identifier multiplication.
//!
//! ## Chord identity model
//!
//! Chord assigns every node, resource, and placement target a point in a
//! 160-bit circular identifier space. Clockwise distance is not ordinary
//! integer distance: it is subtraction in `Z / 2^160`. For an observer `b`, the
//! relative position of `x` is therefore `x - b`. This translation makes the
//! observer's position the local zero point and lets a total byte order witness
//! clockwise ordering from that observer.
//!
//! [`BiasId`] is the domain type for that translated view. It is used when a
//! caller needs to compare identifiers relative to a reference point instead of
//! comparing their raw encodings. The raw [`Did`] order remains the canonical
//! representation order; biased order is a separate protocol proposition.
//!
//! ## Placement model
//!
//! Redundant storage placement uses affine offsets around the identifier ring.
//! For redundancy `n`, [`Did::rotate_affine`] returns
//! `self + floor(2^160 * i / n)` for every `i in 0..n`. This is a DHT placement
//! operation over identities, not a new carrier type. The additive group law is
//! witnessed by `Did`'s [`crate::algebra::AbelianGroup`] implementation.
//!
//! ## Boundary
//!
//! `Did` owns parsing, serialization, display, biasing, range checks, fixed
//! width arithmetic, and DHT placement. Protocol handlers depend on `Did`
//! operations and do not perform byte-level arithmetic.

use std::num::NonZeroU32;
use std::ops::Add;
use std::ops::Deref;
use std::ops::Neg;
use std::ops::Sub;
use std::str::FromStr;

use ethereum_types::H160;
use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

use crate::algebra::AbelianGroup;
use crate::algebra::Zero;
use crate::ecc::HashStr;
use crate::error::Error;
use crate::error::Result;

/// Non-zero witness for the 360-degree denominator used by [`Rotate`].
const FULL_ROTATION_DENOMINATOR: NonZeroU32 = NonZeroU32::MIN.saturating_add(359);

/// DHT identity over the `Z / 2^160` identifier ring.
///
/// Invariant: the inner [`H160`] is the canonical 20-byte big-endian encoding
/// of one residue class modulo `2^160`.
///
/// Law: `Did` addition, subtraction, and negation are the lifted additive group
/// operations of the underlying 160-bit ring.
///
/// Law: parsing, display, serialization, and conversion through [`H160`]
/// preserve the same 20-byte canonical encoding.
#[derive(Copy, Clone, Eq, Ord, PartialEq, PartialOrd, Debug, Serialize, Deserialize, Hash)]
pub struct Did(H160);

impl std::fmt::Display for Did {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let inner = &self.0;
        write!(f, "0x{inner:x}")
    }
}

/// DHT identity observed from a reference point.
///
/// Chord interval comparisons are relative to an observer. Given raw
/// identifiers `a` and `b`, there is no single protocol answer to "which is
/// closer" until a reference identifier `x` is chosen. `BiasId` records that
/// reference and stores `did - x`, so the reference point becomes zero in the
/// lifted ring order.
///
/// Invariant: `did` is always stored as `raw_did - bias`.
///
/// Law: `BiasId::new(x, y).to_did() == y`.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize, Hash)]
pub struct BiasId {
    /// the zero point for determine order of Did.
    bias: Did,
    /// did data without bias.
    did: Did,
}

/// Affine rotation on the 160-bit Chord circle.
///
/// For [`Did`], degrees are mapped to the dyadic ring offset
/// `floor(2^160 * angle / 360)`.
pub trait Rotate<Rhs = u16> {
    /// output type of rotate operation
    type Output;
    /// rotate a Did with given angle
    fn rotate(&self, angle: Rhs) -> Self::Output;
}

impl Rotate<u16> for Did {
    type Output = Self;
    fn rotate(&self, angle: u16) -> Self::Output {
        *self + Did::dyadic_fraction(angle.into(), FULL_ROTATION_DENOMINATOR)
    }
}

impl BiasId {
    /// Wrap a Did into BiasDid with given bias.
    pub fn new(bias: Did, did: Did) -> BiasId {
        BiasId {
            bias,
            did: did - bias,
        }
    }

    /// Get wrapped biased value from did
    pub fn to_did(self) -> Did {
        self.did + self.bias
    }

    /// Get unwrap value from a BiasDid
    pub fn pos(&self) -> Did {
        self.did
    }
}

impl PartialOrd for BiasId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<Did> for BiasId {
    fn eq(&self, rhs: &Did) -> bool {
        let id: Did = self.into();
        id == *rhs
    }
}

impl Ord for BiasId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if other.bias != self.bias {
            let did: Did = other.into();
            let bid = BiasId::new(self.bias, did);
            self.did.cmp(&bid.did)
        } else {
            self.did.cmp(&other.did)
        }
    }
}

impl From<BiasId> for Did {
    fn from(id: BiasId) -> Did {
        BiasId::to_did(id)
    }
}

impl From<&BiasId> for Did {
    fn from(id: &BiasId) -> Did {
        BiasId::to_did(*id)
    }
}

impl From<u32> for Did {
    fn from(id: u32) -> Did {
        let bytes = id.to_be_bytes();
        let mut out = [0u8; Self::BYTE_LEN];
        for (dst, src) in out.iter_mut().rev().zip(bytes.iter().rev()) {
            *dst = *src;
        }
        Self::from_be_bytes(out)
    }
}

impl TryFrom<HashStr> for Did {
    type Error = Error;
    fn try_from(s: HashStr) -> Result<Self> {
        Did::from_str(&s.inner())
    }
}

impl Did {
    const BITS: usize = 160;

    const BYTE_LEN: usize = 20;

    const ZERO: Self = Self(H160([0u8; Self::BYTE_LEN]));

    fn from_be_bytes(bytes: [u8; Self::BYTE_LEN]) -> Self {
        Self(H160::from(bytes))
    }

    fn to_be_bytes(self) -> [u8; Self::BYTE_LEN] {
        self.0.to_fixed_bytes()
    }

    /// Test whether this identity is inside the open clockwise interval
    /// `(a, b)` observed from `base_id`.
    ///
    /// Post: returns `true` exactly when `self - base_id` is strictly after
    /// `a - base_id` and strictly before `b - base_id` in the canonical ring
    /// order.
    pub fn in_range(&self, base_id: Self, a: Self, b: Self) -> bool {
        // Test x > a && b > x
        *self - base_id > a - base_id && b - base_id > *self - base_id
    }

    /// Transform this identity into the view whose zero point is `did`.
    pub fn bias(&self, did: Self) -> BiasId {
        BiasId::new(did, *self)
    }

    /// Rotate this DID into a redundant placement vector.
    ///
    /// Pre: `scalar > 0`.
    ///
    /// Law: `place(self, n)[i] = self + floor(2^160 * i / n)` for
    /// `i in 0..n`.
    ///
    /// Law: `place(self, n)[0] = self`.
    ///
    /// Law: for `i != j`, `place(self, n)[i] != place(self, n)[j]` while
    /// `n <= 2^160`; the current `u16` domain is therefore injective.
    pub fn rotate_affine(&self, scalar: u16) -> Result<Vec<Did>> {
        let Some(denominator) = NonZeroU32::new(u32::from(scalar)) else {
            return Err(Error::InvalidAffineScalar);
        };

        Ok((0..scalar)
            .map(|i| {
                let offset = Did::dyadic_fraction(i.into(), denominator);
                *self + offset
            })
            .collect())
    }

    /// Return `2^bit` in `Z / 2^160`.
    ///
    /// Law: `bit < 160 => result = 2^bit`.
    /// Law: `bit >= 160 => result = 0`.
    pub fn power_of_two(bit: usize) -> Self {
        if bit >= Self::BITS {
            return Self::ZERO;
        }

        let mut bytes = [0u8; Self::BYTE_LEN];
        set_ring_bit(&mut bytes, bit);
        Self::from_be_bytes(bytes)
    }

    // Type: `NonZeroU32` makes a zero denominator unrepresentable.
    // Post: result is `floor(2^160 * numerator / denominator) mod 2^160`.
    // Invariant: `remainder < denominator` before and after every bit step.
    fn dyadic_fraction(numerator: u32, denominator: NonZeroU32) -> Self {
        let denominator = u64::from(denominator.get());
        let mut remainder = u64::from(numerator) % denominator;
        let mut bytes = [0u8; Self::BYTE_LEN];

        for bit in (0..Self::BITS).rev() {
            remainder *= 2;
            if remainder >= denominator {
                set_ring_bit(&mut bytes, bit);
                remainder -= denominator;
            }
        }

        Self::from_be_bytes(bytes)
    }

    // Post: result = (self + rhs) mod 2^160.
    // Preservation: carry beyond the most-significant byte is discarded, which
    // is exactly quotienting by the 160-bit ring modulus.
    fn add_mod(self, rhs: Self) -> Self {
        let lhs = self.to_be_bytes();
        let rhs = rhs.to_be_bytes();
        let mut out = [0u8; Self::BYTE_LEN];
        let mut carry = 0u16;

        for ((dst, lhs), rhs) in out
            .iter_mut()
            .rev()
            .zip(lhs.iter().rev())
            .zip(rhs.iter().rev())
        {
            let sum = u16::from(*lhs) + u16::from(*rhs) + carry;
            let [low, _] = sum.to_le_bytes();
            *dst = low;
            carry = sum >> 8;
        }

        Self::from_be_bytes(out)
    }

    // Post: result = -self mod 2^160.
    // Preservation: two's-complement over exactly 20 bytes computes the
    // additive inverse in `Z / 2^160`; zero maps to zero.
    fn additive_inverse(self) -> Self {
        let mut out = self.to_be_bytes();
        for byte in &mut out {
            *byte = !*byte;
        }

        let mut carry = 1u16;
        for byte in out.iter_mut().rev() {
            let sum = u16::from(*byte) + carry;
            let [low, _] = sum.to_le_bytes();
            *byte = low;
            carry = sum >> 8;
        }

        Self::from_be_bytes(out)
    }
}

/// Ordering with a did reference
/// This trait defines necessary method for sorting based on did.
pub trait SortRing {
    /// Sort a impl SortRing with given did
    fn sort(&mut self, did: Did);
}

impl SortRing for Vec<Did> {
    fn sort(&mut self, did: Did) {
        self.sort_by(|a, b| {
            let (da, db) = (*a - did, *b - did);
            da.cmp(&db)
        });
    }
}

impl Deref for Did {
    type Target = H160;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Did> for H160 {
    fn from(a: Did) -> Self {
        a.0
    }
}

impl From<Did> for BigUint {
    fn from(did: Did) -> BigUint {
        BigUint::from_bytes_be(did.as_bytes())
    }
}

impl From<BigUint> for Did {
    fn from(a: BigUint) -> Self {
        let bytes = a.to_bytes_be();
        let mut out = [0u8; Self::BYTE_LEN];

        // Post: taking the least-significant 20 bytes is reduction modulo
        // `2^160`; right-aligning keeps the conversion total and panic-free.
        for (dst, src) in out
            .iter_mut()
            .rev()
            .zip(bytes.iter().rev().take(Self::BYTE_LEN))
        {
            *dst = *src;
        }

        Self::from_be_bytes(out)
    }
}

impl From<H160> for Did {
    fn from(addr: H160) -> Self {
        Self(addr)
    }
}

impl FromStr for Did {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(Self(H160::from_str(s).map_err(|_| Error::BadCHexInCache)?))
    }
}

impl Zero for Did {
    fn zero() -> Self {
        Self::ZERO
    }

    fn is_zero(&self) -> bool {
        *self == Self::ZERO
    }
}

impl AbelianGroup for Did {}

impl Neg for Did {
    type Output = Self;
    fn neg(self) -> Self {
        self.additive_inverse()
    }
}

impl Neg for &Did {
    type Output = Did;

    fn neg(self) -> Self::Output {
        (*self).neg()
    }
}

impl Add for Did {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        self.add_mod(rhs)
    }
}

impl Sub for Did {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        self + (-rhs)
    }
}

// Pre: `bytes` encodes a 160-bit big-endian ring element.
// Post: if `bit < 160`, the corresponding bit is set; otherwise `bytes` is
// unchanged.
fn set_ring_bit(bytes: &mut [u8; Did::BYTE_LEN], bit: usize) {
    let Some(byte) = (Did::BYTE_LEN - 1).checked_sub(bit / 8) else {
        return;
    };

    if let Some(slot) = bytes.get_mut(byte) {
        *slot |= 1u8 << (bit % 8);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::str::FromStr;

    use super::*;
    use crate::algebra::assert_abelian_group_laws;

    fn ring_size() -> BigUint {
        BigUint::from(1u8) << 160usize
    }

    fn samples() -> Vec<Did> {
        vec![
            Did::zero(),
            Did::from(1u32),
            Did::from(10u32),
            Did::from(ring_size() - BigUint::from(1u8)),
            Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap(),
        ]
    }

    #[test]
    fn did_abelian_group_laws_hold_on_representative_set() {
        assert_abelian_group_laws(&samples());
    }

    #[test]
    fn did_addition_matches_biguint_ring_oracle() {
        for lhs in samples() {
            for rhs in samples() {
                let expected = Did::from((BigUint::from(lhs) + BigUint::from(rhs)) % ring_size());
                assert_eq!(lhs + rhs, expected);
            }
        }
    }

    #[test]
    fn did_dyadic_fraction_matches_biguint_oracle() {
        for denominator in [1u32, 2, 3, 7, 17, 360, 361, u16::MAX.into()] {
            let Some(nonzero_denominator) = NonZeroU32::new(denominator) else {
                continue;
            };

            for numerator in [
                0,
                1,
                denominator / 2,
                denominator.saturating_sub(1),
                denominator,
                denominator.saturating_add(1),
                denominator.saturating_mul(2).saturating_add(1),
            ] {
                let expected =
                    Did::from(ring_size() * BigUint::from(numerator) / BigUint::from(denominator));
                assert_eq!(
                    Did::dyadic_fraction(numerator, nonzero_denominator),
                    expected
                );
            }
        }
    }

    #[test]
    fn test_did() {
        let a = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0x999999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xc0ffee254729296a45a3885639AC7E10F9d54979").unwrap();
        assert!(c > b && b > a);
    }

    #[test]
    fn test_finate_ring_neg() {
        let zero = Did::from_str("0x0000000000000000000000000000000000000000").unwrap();
        let a = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        assert_eq!(-a + a, zero);
        assert_eq!(-(-a), a);
    }

    #[test]
    fn test_sort() {
        let a = Did::from_str("0xaaE807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0xbb9999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xccffee254729296a45a3885639AC7E10F9d54979").unwrap();
        let d = Did::from_str("0xdddfee254729296a45a3885639AC7E10F9d54979").unwrap();
        let mut v = vec![c, b, a, d];
        v.sort(a);
        assert_eq!(v, vec![a, b, c, d]);
        v.sort(b);
        assert_eq!(v, vec![b, c, d, a]);
        v.sort(c);
        assert_eq!(v, vec![c, d, a, b]);
        v.sort(d);
        assert_eq!(v, vec![d, a, b, c]);
    }

    #[test]
    fn rotate_transformation() {
        assert_eq!(Did::from(0u32), Did::from(BigUint::from(2u16).pow(160)));
        let did = Did::from(10u32);
        let result = did.rotate(360);
        assert_eq!(result, did);
    }

    #[test]
    fn right_shift() {
        let did = Did::from(10u32);
        let ret: Did = did.rotate(180);
        assert_eq!(ret, did + Did::from(BigUint::from(2u16).pow(159)));
    }

    #[test]
    fn did_fixed_width_arithmetic_matches_biguint_ring_oracle() -> Result<()> {
        let zero = Did::from(0u32);
        let one = Did::from(1u32);
        let max = Did::from(ring_size() - BigUint::from(1u8));
        let sample = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0")?;

        assert_eq!(max + one, zero);
        assert_eq!(zero - one, max);
        assert_eq!(-zero, zero);
        assert_eq!(-sample + sample, zero);

        for (lhs, rhs) in [(zero, one), (one, max), (sample, max), (sample, sample)] {
            let expected = Did::from((BigUint::from(lhs) + BigUint::from(rhs)) % ring_size());
            assert_eq!(lhs + rhs, expected);
        }
        Ok(())
    }

    #[test]
    fn did_rotate_matches_biguint_dyadic_offset_oracle() {
        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();

        for angle in [0u16, 1, 90, 180, 359, 360, 361, u16::MAX] {
            let expected_offset =
                Did::from(ring_size() * BigUint::from(angle) / BigUint::from(360u32));
            assert_eq!(did.rotate(angle), did + expected_offset);
        }
    }

    #[test]
    fn did_power_of_two_matches_biguint_oracle() {
        for bit in [0usize, 1, 8, 31, 32, 63, 64, 127, 128, 159, 160, 255] {
            let expected = Did::from(BigUint::from(1u8) << bit);
            assert_eq!(Did::power_of_two(bit), expected);
        }
    }

    #[test]
    fn test_did_affine() -> Result<()> {
        let did = Did::from(10u32);
        let affine_dids = did.rotate_affine(4)?;
        assert_eq!(affine_dids.len(), 4);
        assert_eq!(affine_dids, vec![
            did.rotate(0),
            did.rotate(90),
            did.rotate(180),
            did.rotate(270)
        ]);
        Ok(())
    }

    #[test]
    fn rotate_affine_rejects_zero_scalar() {
        let did = Did::from(10u32);

        assert!(matches!(
            did.rotate_affine(0),
            Err(Error::InvalidAffineScalar)
        ));
    }

    #[test]
    fn rotate_affine_supports_non_degree_divisors() -> Result<()> {
        let did = Did::from(10u32);
        let affine_dids = did.rotate_affine(7)?;
        let unique_dids = affine_dids.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(affine_dids.len(), 7);
        assert_eq!(unique_dids.len(), 7);
        assert_eq!(affine_dids.first(), Some(&did));
        Ok(())
    }

    #[test]
    fn rotate_affine_supports_more_than_360_replicas() -> Result<()> {
        let did = Did::from(10u32);
        let affine_dids = did.rotate_affine(361)?;
        let unique_dids = affine_dids.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(affine_dids.len(), 361);
        assert_eq!(unique_dids.len(), 361);
        assert_eq!(affine_dids.first(), Some(&did));
        Ok(())
    }

    #[test]
    fn test_dump_and_load() {
        // The length must be 40.
        assert!(Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab").is_err());
        assert!(Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab00").is_err());

        // Allow omit 0x prefix
        assert_eq!(
            Did::from_str("11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap(),
            Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap(),
        );

        // from_str then to_string
        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        assert_eq!(
            did.to_string(),
            "0x11e807fcc88dd319270493fb2e822e388fe36ab0"
        );

        // Serialize
        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        assert_eq!(
            serde_json::to_string(&did).unwrap(),
            "\"0x11e807fcc88dd319270493fb2e822e388fe36ab0\""
        );

        // Deserialize
        let did =
            serde_json::from_str::<Did>("\"0x11e807fcc88dd319270493fb2e822e388fe36ab0\"").unwrap();
        assert_eq!(
            did,
            Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap()
        );

        // Debug and Display
        let did = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        assert_eq!(
            format!("{did}"),
            "0x11e807fcc88dd319270493fb2e822e388fe36ab0"
        );
        assert_eq!(
            format!("{did:?}"),
            "Did(0x11e807fcc88dd319270493fb2e822e388fe36ab0)"
        );
    }
}
