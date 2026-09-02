pub mod bip137;
pub mod bls;
pub mod ed25519;
pub mod eip191;
pub mod secp256k1;
pub mod secp256r1;

use elliptic_curve::generic_array::ArrayLength;
use elliptic_curve::scalar::IsHigh;
use elliptic_curve::CurveArithmetic;
use elliptic_curve::PrimeCurve;

use crate::error::Error;
use crate::error::Result;

pub(crate) fn ecdsa_signature_s_is_high<C>(signature: &ecdsa::Signature<C>) -> bool
where
    C: PrimeCurve + CurveArithmetic,
    ecdsa::SignatureSize<C>: ArrayLength<u8>,
{
    bool::from(signature.s().is_high())
}

pub(crate) fn recovery_id_from_v(v: u8, base: u8) -> Result<u8> {
    let Some(max) = base.checked_add(3) else {
        return Err(Error::InvalidRecoverId(v));
    };
    if (base..=max).contains(&v) {
        Ok(v - base)
    } else {
        Err(Error::InvalidRecoverId(v))
    }
}
