//! signer of bls
//! based on [bls_signatures](https://docs.rs/bls-signatures/latest/bls_signatures/)
//! This module implement transform of [libsecp256k1::Secretkey] and [bls_signature::PrivateKey]

use bls12_381::G1Affine;
use bls12_381::G1Projective;
use bls12_381::G2Affine;
use bls12_381::G2Projective;
use bls12_381::Scalar;
use bls_signatures as bls;
use libsecp256k1;

use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;

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

pub fn hash(msg: &[u8]) -> [u8; 96] {
    let hash: G2Affine = bls::hash(msg).into();
    hash.to_compressed()
}

/// Sign message with bls privatekey
/// reimplemented via https://docs.rs/bls-signatures/latest/src/bls_signatures/key.rs.html#103
/// signature = hash_into_g2(message) * sk
pub fn sign(sec: SecretKey, hash: &[u8; 96]) -> Result<[u8; 96]> {
    let sk: bls::PrivateKey = sec.try_into()?;
    let affine: Option<G2Affine> = G2Affine::from_compressed(hash).into();
    if let Some(af) = affine {
        let mut proj: G2Projective = af.into();
        proj *= Into::<Scalar>::into(sk);
        Ok(G2Affine::from(proj).to_compressed())
    } else {
        Err(Error::PublicKeyBadFormat)
    }
}

/// Sign message with bls privatekey and message
pub fn sign_raw(sec: SecretKey, msg: &[u8]) -> Result<[u8; 96]> {
    let sk: bls::PrivateKey = sec.try_into()?;
    let sig = sk.sign(hash(msg));
    Ok(G2Affine::from(sig).to_compressed())
}

/// Verify message
pub fn verify(hashes: &[[u8; 96]], sig: &[u8; 96], public_keys: &[PublicKey]) -> Result<bool> {
    let sig_affine: Option<G2Affine> = G2Affine::from_compressed(sig).into();
    if let Some(affine) = sig_affine {
        let signature = bls::Signature::from(affine);
        let hash: Option<Vec<G2Projective>> = hashes
            .iter()
            .map(|h| Into::<Option<G2Affine>>::into(G2Affine::from_compressed(h)))
            .map(|a| a.map(|b| b.into()))
            .collect::<Option<Vec<G2Projective>>>();
        let pk: Result<Vec<bls::PublicKey>> =
            public_keys.iter().map(|pk| pk.clone().try_into()).collect();
        if let (Some(h), Ok(p)) = (hash, pk) {
            Ok(bls::verify(&signature, &h, &p))
        } else {
            Err(Error::BlsAffineDecodeFailed)
        }
    } else {
        Err(Error::PublicKeyBadFormat)
    }
}

/// Verify message
pub fn verify_msg(msgs: &[&[u8]], sig: &[u8; 96], public_keys: &[PublicKey]) -> Result<bool> {
    let sig_affine: Option<G2Affine> = G2Affine::from_compressed(sig).into();
    if let Some(affine) = sig_affine {
        let signature = bls::Signature::from(affine);
        let pk: Result<Vec<bls::PublicKey>> =
            public_keys.iter().map(|pk| pk.clone().try_into()).collect();
        if let Ok(p) = pk {
            Ok(bls::verify_messages(&signature, msgs, &p))
        } else {
            Err(Error::BlsAffineDecodeFailed)
        }
    } else {
        Err(Error::PublicKeyBadFormat)
    }
}

/// Cast bls publickey from bls secretkey
pub fn to_bls_pk(sk: SecretKey) -> Result<PublicKey> {
    let seckey: bls::PrivateKey = sk.try_into()?;
    let pk = seckey.public_key();
    Ok(pk.into())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ecc::SecretKey;

    #[test]
    fn test_sign_and_verify() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let msg = "hello";
        let sig = sign_raw(key, msg.as_bytes()).unwrap();
        let pk = to_bls_pk(key).unwrap();
        let hash = hash(msg.as_bytes());
        assert!(verify_msg(
            vec![msg.as_bytes()].as_slice(),
            &sig,
            vec![pk.clone()].as_slice()
        )
        .unwrap());
        assert!(verify(vec![hash].as_slice(), &sig, vec![pk].as_slice()).unwrap());
    }
}
