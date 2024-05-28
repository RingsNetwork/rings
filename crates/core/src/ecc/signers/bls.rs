//! signer of bls
//! A module for signing messages using BLS (Boneh-Lynn-Shacham) and interfacing with secp256k1 cryptographic libraries.
//!
//! This module provides functionality for generating private and public keys, signing messages,
//! and verifying signatures using both BLS and secp256k1 cryptographic standards.
//! It integrates the use of random number generation for key creation and provides conversions
//! between different key types.

use bls12_381::G1Affine;
use bls12_381::G1Projective;
use bls12_381::G2Affine;
use bls12_381::G2Projective;
use bls12_381::Scalar;
use bls_signatures as bls;
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

impl From<bls::Signature> for Signature {
    fn from(s: bls::Signature) -> Self {
        Self(G2Affine::from(s).to_compressed())
    }
}

impl TryInto<bls::Signature> for Signature {
    type Error = Error;
    fn try_into(self) -> Result<bls::Signature> {
        let affine: Option<G2Affine> = G2Affine::from_compressed(&self.0).into();
        if let Some(af) = affine {
            Ok(bls::Signature::from(af))
        } else {
            Err(Error::BlsAffineDecodeFailed)
        }
    }
}

/// Generates a random BLS private key.
pub fn random_sk() -> Result<SecretKey> {
    let mut rng = Hc128Rng::from_entropy();
    bls::PrivateKey::generate(&mut rng).try_into()
}

impl TryInto<SecretKey> for bls::PrivateKey {
    type Error = Error;
    fn try_into(self) -> Result<SecretKey> {
        let data: [u8; 32] = Into::<Scalar>::into(self).to_bytes();
        let sk = libsecp256k1::SecretKey::parse(&data)?;
        Ok(SecretKey(sk))
    }
}

impl TryInto<bls::PrivateKey> for SecretKey {
    type Error = Error;
    fn try_into(self) -> Result<bls::PrivateKey> {
        let sk = self.0.serialize();
        let scalar: Option<Scalar> = Scalar::from_bytes(&sk).into();
        if let Some(key) = scalar {
            Ok(bls::PrivateKey::from(key))
        } else {
            Err(Error::PrivateKeyBadFormat)
        }
    }
}

impl From<bls::PublicKey> for PublicKey {
    fn from(val: bls::PublicKey) -> Self {
        let data: [u8; 48] = val.as_affine().to_compressed();
        PublicKey(data.to_vec())
    }
}

impl TryInto<bls::PublicKey> for PublicKey {
    type Error = Error;
    fn try_into(self) -> Result<bls::PublicKey> {
        let data: [u8; 48] = self.0.try_into().map_err(|_| Error::PublicKeyBadFormat)?;
        let affine: Option<G1Affine> = G1Affine::from_compressed(&data).into();
        if let Some(af) = affine {
            Ok(bls::PublicKey::from(G1Projective::from(af)))
        } else {
            Err(Error::PublicKeyBadFormat)
        }
    }
}

/// Hashes a message to a 96-byte array using BLS hashing.
pub fn hash(msg: &[u8]) -> [u8; 96] {
    let hash: G2Affine = bls::hash(msg).into();
    hash.to_compressed()
}

/// Sign message with bls privatekey
/// reimplemented via https://docs.rs/bls-signatures/latest/src/bls_signatures/key.rs.html#103
/// signature = hash_into_g2(message) * sk
pub fn sign(sec: SecretKey, hash: &[u8; 96]) -> Result<Signature> {
    let sk: bls::PrivateKey = sec.try_into()?;
    let affine: Option<G2Affine> = G2Affine::from_compressed(hash).into();
    if let Some(af) = affine {
        let mut proj: G2Projective = af.into();
        proj *= Into::<Scalar>::into(sk);
        Ok(Signature(G2Affine::from(proj).to_compressed()))
    } else {
        Err(Error::PublicKeyBadFormat)
    }
}

/// Signs a message with a BLS private key by first hashing the message.
pub fn sign_raw(sec: SecretKey, msg: &[u8]) -> Result<Signature> {
    sign(sec, &hash(msg))
}

/// Verifies a BLS signature against a set of hashes and public keys.
pub fn verify(hashes: &[[u8; 96]], sig: &Signature, public_keys: &[PublicKey]) -> Result<bool> {
    let sig: bls::Signature = sig.clone().try_into()?;
    let hash: Vec<G2Projective> = hashes
        .iter()
        .map(|h| Into::<Option<G2Affine>>::into(G2Affine::from_compressed(h)))
        .map(|a| a.map(|b| b.into()))
        .collect::<Option<Vec<G2Projective>>>()
        .ok_or(Error::BlsAffineDecodeFailed)?;
    let pk: Vec<bls::PublicKey> = public_keys
        .iter()
        .map(|pk| pk.clone().try_into())
        .collect::<Result<Vec<bls::PublicKey>>>()?;
    Ok(bls::verify(&sig, &hash, &pk))
}

/// Verifies a BLS signature against a set of messages and public keys.
pub fn verify_msg(msgs: &[&[u8]], sig: &Signature, public_keys: &[PublicKey]) -> Result<bool> {
    let sig: bls::Signature = sig.clone().try_into()?;
    let pk: Vec<bls::PublicKey> = public_keys
        .iter()
        .map(|pk| pk.clone().try_into())
        .collect::<Result<Vec<bls::PublicKey>>>()?;
    Ok(bls::verify_messages(&sig, msgs, &pk))
}

/// Converts a BLS private key to a BLS public key.
pub fn to_bls_pk(sk: &SecretKey) -> Result<PublicKey> {
    let seckey: bls::PrivateKey = sk.clone().try_into()?;
    let pk = seckey.public_key();
    Ok(pk.into())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_types() {
        let key1: SecretKey = random_sk().unwrap();
        let bls_key: bls::PrivateKey = key1.try_into().unwrap();
        let key2: SecretKey = bls_key.try_into().unwrap();
        assert_eq!(key1, key2);

        let pk1: PublicKey = to_bls_pk(&key1).unwrap();
        let bls_pk: bls::PublicKey = bls_key.public_key();
        let pk2: PublicKey = bls_pk.try_into().unwrap();
        assert_eq!(pk1, pk2);
    }

    #[test]
    fn test_sign_and_verify() {
        let key = random_sk().unwrap();
        let msg = "hello";
        let pk = to_bls_pk(&key).unwrap();
        let sig = sign_raw(key, msg.as_bytes()).unwrap();
        let hash = hash(msg.as_bytes());

        // test sign with bls
        let bls_sk: bls::PrivateKey = key.try_into().unwrap();
        let bls_pk: bls::PublicKey = bls_sk.public_key();
        let bls_sig = bls_sk.sign(msg.as_bytes());
        assert!(bls::verify_messages(
            &bls_sig,
            vec![msg.as_bytes()].as_slice(),
            vec![bls_pk].as_slice()
        ));
        assert_eq!(Signature::from(bls_sig), sig);

        assert!(verify_msg(
            vec![msg.as_bytes()].as_slice(),
            &sig,
            vec![pk.clone()].as_slice()
        )
        .expect("panic on verify msg"));
        assert!(verify(vec![hash].as_slice(), &sig, vec![pk].as_slice()).unwrap());
    }
}
