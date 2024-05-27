//! signer of bls
//! based on [bls_signatures](https://docs.rs/bls-signatures/latest/bls_signatures/)
//! This module implement transform of [libsecp256k1::Secretkey] and [bls_signature::PrivateKey]

use bls12_381::Scalar;
use bls_signatures  as bls;
use libsecp256k1;
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
