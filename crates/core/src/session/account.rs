use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::ecc::keys::AccountVerifier;
use crate::ecc::keys::SignatureAlgorithm;
use crate::ecc::keys::VerificationPublicKey;
use crate::ecc::PublicKey;
use crate::error::Error;
use crate::error::Result;

/// An external account authorized to create a Rings session.
///
/// Rings supports recoverable signatures identified by DID and non-recoverable signatures
/// identified by their verification public key.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum Account {
    /// secp256k1 recoverable account.
    Secp256k1(Did),
    /// secp256r1 account used by Web Crypto implementations.
    Secp256r1(PublicKey<33>),
    /// EIP-191 account.
    EIP191(Did),
    /// Bitcoin BIP-137 account.
    BIP137(Did),
    /// Ed25519 account.
    Ed25519(PublicKey<33>),
    /// BLS12-381 account.
    Bls12381(PublicKey<48>),
}

impl TryFrom<(String, String)> for Account {
    type Error = Error;

    fn try_from((account_entity, account_type): (String, String)) -> Result<Self> {
        match AccountVerifier::from_account_parts(&account_entity, &account_type)? {
            AccountVerifier::Recoverable {
                algorithm: SignatureAlgorithm::Secp256k1,
                did,
            } => Ok(Account::Secp256k1(did)),
            AccountVerifier::Recoverable {
                algorithm: SignatureAlgorithm::Eip191,
                did,
            } => Ok(Account::EIP191(did)),
            AccountVerifier::Recoverable {
                algorithm: SignatureAlgorithm::Bip137,
                did,
            } => Ok(Account::BIP137(did)),
            AccountVerifier::PublicKey(VerificationPublicKey::Secp256r1(pk)) => {
                Ok(Account::Secp256r1(pk))
            }
            AccountVerifier::PublicKey(VerificationPublicKey::Ed25519(pk)) => {
                Ok(Account::Ed25519(pk))
            }
            AccountVerifier::PublicKey(VerificationPublicKey::Bls12381(pk)) => {
                Ok(Account::Bls12381(pk))
            }
            _ => Err(Error::UnknownAccount),
        }
    }
}

impl Account {
    pub(super) fn account_verifier(&self) -> AccountVerifier {
        match self {
            Self::Secp256k1(did) => AccountVerifier::Recoverable {
                algorithm: SignatureAlgorithm::Secp256k1,
                did: *did,
            },
            Self::EIP191(did) => AccountVerifier::Recoverable {
                algorithm: SignatureAlgorithm::Eip191,
                did: *did,
            },
            Self::BIP137(did) => AccountVerifier::Recoverable {
                algorithm: SignatureAlgorithm::Bip137,
                did: *did,
            },
            Self::Secp256r1(pk) => {
                AccountVerifier::PublicKey(VerificationPublicKey::Secp256r1(*pk))
            }
            Self::Ed25519(pk) => AccountVerifier::PublicKey(VerificationPublicKey::Ed25519(*pk)),
            Self::Bls12381(pk) => AccountVerifier::PublicKey(VerificationPublicKey::Bls12381(*pk)),
        }
    }
}
