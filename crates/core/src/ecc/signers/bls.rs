//! signer of bls
//! A module for signing messages using BLS (Boneh-Lynn-Shacham) and interfacing with secp256k1 cryptographic libraries.
//!
//! This module provides functionality for generating private and public keys, signing messages,
//! and verifying signatures using both BLS and secp256k1 cryptographic standards.
//! It integrates the use of random number generation for key creation and provides conversions
//! between different key types.

use ark_bls12_381::fr::Fr;
use ark_bls12_381::g2::Config as G2Config;
use ark_bls12_381::Bls12_381;
use ark_bls12_381::G1Projective;
use ark_bls12_381::G2Projective;
use ark_ec::hashing::curve_maps::wb::WBMap;
use ark_ec::hashing::map_to_curve_hasher::MapToCurveBasedHasher;
use ark_ec::hashing::HashToCurve;
use ark_ec::pairing::Pairing;
use ark_ec::Group;
use ark_ff::fields::field_hashers::DefaultFieldHasher;
use ark_serialize::CanonicalDeserialize;
use ark_serialize::CanonicalSerialize;
use ark_std::UniformRand;
use libsecp256k1;
use rand::SeedableRng;
use rand_hc::Hc128Rng;

use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

/// Represents a BLS signature, stored as a 96-byte array.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signature(pub [u8; 96]);

/// this function is used to generate a random secret key
pub fn random_sk() -> Result<SecretKey> {
    let mut rng = Hc128Rng::from_entropy();
    Fr::rand(&mut rng).try_into()
}

fn from_compressed<T: CanonicalDeserialize, const S: usize>(a: &[u8; S]) -> Result<T> {
    T::deserialize_compressed(&a[..]).map_err(|_| Error::EccDeserializeFailed)
}

fn to_compressed<T: CanonicalSerialize, const S: usize>(s: &T) -> Result<[u8; S]> {
    let mut data: Vec<u8> = vec![];
    s.serialize_compressed(&mut data)
        .map_err(|_| Error::EccSerializeFailed)?;
    assert_eq!(s.compressed_size(), S);
    assert_eq!(data.len(), S);
    let ret: [u8; S] = data.try_into().map_err(|_| Error::EccSerializeFailed)?;
    Ok(ret)
}

impl TryFrom<SecretKey> for Fr {
    type Error = Error;
    fn try_from(sk: SecretKey) -> Result<Fr> {
        let data: [u8; 32] = sk.0.serialize();
        let ret: Fr = from_compressed(&data)?;
        Ok(ret)
    }
}

impl TryFrom<Fr> for SecretKey {
    type Error = Error;
    fn try_from(sk: Fr) -> Result<SecretKey> {
        let data: [u8; 32] = to_compressed(&sk)?;
        let sk = libsecp256k1::SecretKey::parse(&data)?;
        Ok(SecretKey(sk))
    }
}

impl TryFrom<Signature> for G2Projective {
    type Error = Error;
    fn try_from(s: Signature) -> Result<Self> {
        from_compressed(&s.0)
    }
}

impl TryFrom<G2Projective> for Signature {
    type Error = Error;
    fn try_from(s: G2Projective) -> Result<Self> {
        Ok(Signature(to_compressed(&s)?))
    }
}

impl TryFrom<G1Projective> for PublicKey {
    type Error = Error;
    fn try_from(p: G1Projective) -> Result<Self> {
        Ok(PublicKey(to_compressed::<G1Projective, 48>(&p)?.to_vec()))
    }
}

impl TryFrom<PublicKey> for G1Projective {
    type Error = Error;
    fn try_from(pk: PublicKey) -> Result<Self> {
        let data: [u8; 48] = pk.0.try_into().map_err(|_| Error::PublicKeyBadFormat)?;
        let ret: Self = from_compressed(&data)?;
        Ok(ret)
    }
}

/// Hashes a message to a 96-byte array using BLS
// https://datatracker.ietf.org/doc/draft-irtf-cfrg-hash-to-curve/
/// TODO:
/// https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-hash-to-curve-12#appendix-K.1
/// Xmd is not implemented here
pub fn hash_to_curve(msg: &[u8]) -> Result<[u8; 96]> {
    // let swu_map: WBMap<G1Config> = WBMap::new().unwrap();
    let hasher = MapToCurveBasedHasher::<
        G2Projective,
        DefaultFieldHasher<sha2::Sha256, 128>,
        WBMap<G2Config>,
    >::new(&[])
    .map_err(|_| Error::CurveHasherInitFailed)?;
    let hashed = hasher.hash(msg).map_err(|_| Error::CurveHasherFailed)?;
    let ret: [u8; 96] = to_compressed(&hashed)?;
    Ok(ret)
}

/// Sign message with bls privatekey
/// reimplemented via https://docs.rs/bls-signatures/latest/src/bls_signatures/key.rs.html#103
/// signature = hash_into_g2(message) * sk
pub fn sign(sk: SecretKey, hashed_msg: &[u8; 96]) -> Result<Signature> {
    let sk: Fr = sk.try_into()?;
    let msg: G2Projective = from_compressed(hashed_msg).unwrap();
    Ok(Signature(to_compressed(&(msg * sk))?))
}

/// Verifies that the signature is the actual aggregated signature of hashes - pubkeys. Calculated by
/// e(g1, signature) == \prod_{i = 0}^n e(pk_i, hash_i).
pub fn verify(hashes: &[[u8; 96]], sig: &Signature, pks: &[PublicKey]) -> Result<bool> {
    let sig: G2Projective = sig.clone().try_into()?;
    let g1 = G1Projective::generator();
    let e1 = Bls12_381::pairing(g1, sig);

    let hashes: Vec<G2Projective> = hashes
        .iter()
        .map(from_compressed)
        .collect::<Result<Vec<G2Projective>>>()?;

    let pks: Vec<G1Projective> = pks
        .iter()
        .map(|pk| pk.clone().try_into())
        .collect::<Result<Vec<G1Projective>>>()?;

    let mm_out = Bls12_381::multi_miller_loop(pks, hashes);
    if let Some(e2) = Bls12_381::final_exponentiation(mm_out) {
        Ok(e1 == e2)
    } else {
        Ok(false)
    }
}

/// Converts a BLS private key to a BLS public key.
/// Get the public key for this private key. Calculated by pk = g1 * sk.
pub fn public_key(key: &SecretKey) -> Result<PublicKey> {
    let sk: Fr = (*key).try_into()?;
    let g1 = G1Projective::generator();
    (g1 * sk).try_into()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_sign_and_verify() {
        let key = random_sk().unwrap();
        let msg = "hello world";
        let pk = public_key(&key).unwrap();
        let h = hash_to_curve(msg.as_bytes()).unwrap();
        let sig = sign(key, &h).unwrap();
        assert!(super::verify(vec![h].as_slice(), &sig, vec![pk].as_slice()).unwrap());
    }
}
