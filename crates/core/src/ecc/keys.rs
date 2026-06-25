//! Account-signing keys and DID derivation.
//!
//! This module is deliberately scoped to account identity and session
//! verification:
//!
//! - [`SigningSecretKey`] owns account-signing secret material.
//! - [`VerificationPublicKey`] owns the explicit public key needed by
//!   non-recoverable signature schemes.
//! - [`AccountVerifier`] is the session-facing verifier. Recoverable schemes
//!   can use a DID plus algorithm; non-recoverable schemes carry a public key.
//!
//! ElGamal encryption keys live in [`crate::ecc::elgamal`], where they can be
//! parameterized by any finite cyclic group. Keeping encryption keys out of this
//! module avoids implying that account identity keys and message-encryption
//! keys are the same cryptographic object.
//!
//! DID derivation uses domain-separated public-key transcripts:
//! `algorithm || 0x00 || raw_public_key`. This prevents equal raw bytes under
//! different algorithms from resolving to the same account DID.

use std::cell::RefCell;

use rand::RngCore;
use rand::SeedableRng;
use rand_hc::Hc128Rng;
use serde::Deserialize;
use serde::Serialize;

use super::keccak256;
use super::signers;
use super::PublicKey;
use super::PublicKeyAddress;
use super::SecretKey;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

thread_local! {
    static KEY_RNG: RefCell<Hc128Rng> = RefCell::new(Hc128Rng::from_entropy());
}

fn public_key_transcript(algorithm: &str, raw_bytes: &[u8]) -> Vec<u8> {
    let mut out = algorithm.as_bytes().to_vec();
    out.push(0);
    out.extend_from_slice(raw_bytes);
    out
}

fn domain_separated_address(algorithm: &str, raw_bytes: &[u8]) -> PublicKeyAddress {
    PublicKeyAddress::from_slice(&keccak256(&public_key_transcript(algorithm, raw_bytes))[12..])
}

/// Signature algorithm used by an account signing key.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, Eq, PartialEq)]
pub enum SignatureAlgorithm {
    /// secp256k1 ECDSA.
    Secp256k1,
    /// Ethereum EIP-191 personal-sign over secp256k1.
    Eip191,
    /// Bitcoin BIP-137 personal-sign over secp256k1.
    Bip137,
    /// secp256r1 ECDSA.
    Secp256r1,
    /// Ed25519.
    Ed25519,
    /// BLS12-381 signatures with G1 public keys and G2 signatures.
    Bls12381,
}

/// Public key used for signature verification and account addressing.
#[derive(Deserialize, Serialize, Debug, Clone, Eq, PartialEq)]
pub enum VerificationPublicKey {
    /// secp256k1 ECDSA public key.
    Secp256k1(PublicKey<33>),
    /// Ethereum EIP-191 recoverable secp256k1 public key.
    Eip191(PublicKey<33>),
    /// Bitcoin BIP-137 recoverable secp256k1 public key.
    Bip137(PublicKey<33>),
    /// secp256r1 ECDSA public key.
    Secp256r1(PublicKey<33>),
    /// Ed25519 public key.
    Ed25519(PublicKey<33>),
    /// BLS12-381 G1 public key.
    Bls12381(PublicKey<48>),
}

/// Public verifier for an account signature.
///
/// Recoverable signature schemes can use DID/address plus algorithm, while
/// non-recoverable schemes must carry the explicit public key.
#[derive(Deserialize, Serialize, Debug, Clone, Eq, PartialEq)]
pub enum AccountVerifier {
    /// Recoverable signature schemes can identify the account by DID/address.
    Recoverable {
        /// Recoverable signature algorithm.
        algorithm: SignatureAlgorithm,
        /// Account DID.
        did: Did,
    },
    /// Non-recoverable schemes must carry the public key.
    PublicKey(VerificationPublicKey),
}

/// Ed25519 signing seed.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, Eq, PartialEq)]
pub struct Ed25519SecretKey([u8; 32]);

/// Secret key used for account signatures.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, Eq, PartialEq)]
pub enum SigningSecretKey {
    /// secp256k1 ECDSA signing key.
    Secp256k1(SecretKey),
    /// EIP-191 signing key.
    Eip191(SecretKey),
    /// BIP-137 signing key.
    Bip137(SecretKey),
    /// secp256r1 signing key.
    Secp256r1(SecretKey),
    /// Ed25519 signing key.
    Ed25519(Ed25519SecretKey),
    /// BLS12-381 signing key.
    Bls12381(SecretKey),
}

impl SignatureAlgorithm {
    /// Stable lower-case algorithm name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Secp256k1 => "secp256k1",
            Self::Eip191 => "eip191",
            Self::Bip137 => "bip137",
            Self::Secp256r1 => "secp256r1",
            Self::Ed25519 => "ed25519",
            Self::Bls12381 => "bls12-381",
        }
    }

    /// Whether the public key can be recovered from a message signature.
    pub fn is_recoverable(self) -> bool {
        matches!(self, Self::Secp256k1 | Self::Eip191 | Self::Bip137)
    }
}

impl VerificationPublicKey {
    /// Signature algorithm for this public key.
    pub fn algorithm(&self) -> SignatureAlgorithm {
        match self {
            Self::Secp256k1(_) => SignatureAlgorithm::Secp256k1,
            Self::Eip191(_) => SignatureAlgorithm::Eip191,
            Self::Bip137(_) => SignatureAlgorithm::Bip137,
            Self::Secp256r1(_) => SignatureAlgorithm::Secp256r1,
            Self::Ed25519(_) => SignatureAlgorithm::Ed25519,
            Self::Bls12381(_) => SignatureAlgorithm::Bls12381,
        }
    }

    /// Verify a signature for this explicit public key.
    pub fn verify(&self, msg: &[u8], sig: impl AsRef<[u8]>) -> bool {
        match self {
            Self::Secp256k1(pk) => signers::secp256k1::verify(msg, &pk.address(), sig.as_ref()),
            Self::Eip191(pk) => signers::eip191::verify(msg, &pk.address(), sig.as_ref()),
            Self::Bip137(pk) => signers::bip137::verify(msg, &pk.address(), sig.as_ref()),
            Self::Secp256r1(pk) => signers::secp256r1::verify(msg, &pk.address(), sig.as_ref(), pk),
            Self::Ed25519(pk) => signers::ed25519::verify(msg, &pk.address(), sig.as_ref(), pk),
            Self::Bls12381(pk) => {
                let Ok(sig_data) = sig.as_ref().try_into() else {
                    return false;
                };
                signers::bls::verify(&[msg], &signers::bls::Signature(sig_data), &[*pk])
                    .unwrap_or(false)
            }
        }
    }

    /// Raw public key bytes.
    pub fn raw_bytes(&self) -> &[u8] {
        match self {
            Self::Secp256k1(pk)
            | Self::Eip191(pk)
            | Self::Bip137(pk)
            | Self::Secp256r1(pk)
            | Self::Ed25519(pk) => &pk.0,
            Self::Bls12381(pk) => &pk.0,
        }
    }

    /// Domain-separated transcript bytes used to derive non-recoverable DIDs.
    pub fn transcript_bytes(&self) -> Vec<u8> {
        public_key_transcript(self.algorithm().as_str(), self.raw_bytes())
    }

    fn domain_separated_address(&self) -> PublicKeyAddress {
        domain_separated_address(self.algorithm().as_str(), self.raw_bytes())
    }

    /// DID represented by this verification key.
    pub fn did(&self) -> Did {
        match self {
            Self::Secp256k1(pk) | Self::Eip191(pk) | Self::Bip137(pk) => pk.address().into(),
            Self::Secp256r1(_) | Self::Ed25519(_) | Self::Bls12381(_) => {
                self.domain_separated_address().into()
            }
        }
    }
}

impl AccountVerifier {
    /// Parse a legacy session account pair into an account verifier.
    pub fn from_account_parts(account_entity: &str, account_type: &str) -> Result<Self> {
        match account_type {
            "secp256k1" => Ok(Self::Recoverable {
                algorithm: SignatureAlgorithm::Secp256k1,
                did: account_entity.parse()?,
            }),
            "eip191" => Ok(Self::Recoverable {
                algorithm: SignatureAlgorithm::Eip191,
                did: account_entity.parse()?,
            }),
            "bip137" => Ok(Self::Recoverable {
                algorithm: SignatureAlgorithm::Bip137,
                did: account_entity.parse()?,
            }),
            "secp256r1" => {
                let public_key = PublicKey::from_hex_string(account_entity)?;
                let verifying_key = public_key.ct_try_into_secp256r1_pubkey();
                if !bool::from(verifying_key.is_some()) || verifying_key.unwrap().is_err() {
                    return Err(Error::InvalidPublicKey);
                }
                Ok(Self::PublicKey(VerificationPublicKey::Secp256r1(
                    public_key,
                )))
            }
            "ed25519" => Ok(Self::PublicKey(VerificationPublicKey::Ed25519(
                PublicKey::try_from_b58t(account_entity)?,
            ))),
            "bls12-381" | "bls12381" => Ok(Self::PublicKey(VerificationPublicKey::Bls12381(
                public_key_from_b58m_exact(account_entity)?,
            ))),
            _ => Err(Error::UnknownAccount),
        }
    }

    /// Signature algorithm for this account verifier.
    pub fn algorithm(&self) -> SignatureAlgorithm {
        match self {
            Self::Recoverable { algorithm, .. } => *algorithm,
            Self::PublicKey(public_key) => public_key.algorithm(),
        }
    }

    /// Verify an account signature.
    pub fn verify(&self, msg: &[u8], sig: impl AsRef<[u8]>) -> bool {
        match self {
            Self::Recoverable {
                algorithm: SignatureAlgorithm::Secp256k1,
                did,
            } => signers::secp256k1::verify(msg, &(*did).into(), sig.as_ref()),
            Self::Recoverable {
                algorithm: SignatureAlgorithm::Eip191,
                did,
            } => signers::eip191::verify(msg, &(*did).into(), sig.as_ref()),
            Self::Recoverable {
                algorithm: SignatureAlgorithm::Bip137,
                did,
            } => signers::bip137::verify(msg, &(*did).into(), sig.as_ref()),
            Self::Recoverable { .. } => false,
            Self::PublicKey(public_key) => public_key.verify(msg, sig.as_ref()),
        }
    }

    /// Recover or return the explicit public verification key for this verifier.
    pub fn verification_key_from_signature(
        &self,
        msg: &[u8],
        sig: impl AsRef<[u8]>,
    ) -> Result<VerificationPublicKey> {
        match self {
            Self::Recoverable {
                algorithm: SignatureAlgorithm::Secp256k1,
                ..
            } => Ok(VerificationPublicKey::Secp256k1(
                signers::secp256k1::recover(msg, sig.as_ref())?,
            )),
            Self::Recoverable {
                algorithm: SignatureAlgorithm::Eip191,
                ..
            } => Ok(VerificationPublicKey::Eip191(signers::eip191::recover(
                msg,
                sig.as_ref(),
            )?)),
            Self::Recoverable {
                algorithm: SignatureAlgorithm::Bip137,
                ..
            } => Ok(VerificationPublicKey::Bip137(signers::bip137::recover(
                msg,
                sig.as_ref(),
            )?)),
            Self::Recoverable { .. } => Err(Error::UnknownAccount),
            Self::PublicKey(public_key) => Ok(public_key.clone()),
        }
    }
}

impl AccountVerifier {
    /// DID represented by this verifier.
    pub fn did(&self) -> Did {
        match self {
            Self::Recoverable { did, .. } => *did,
            Self::PublicKey(public_key) => public_key.did(),
        }
    }
}

impl Ed25519SecretKey {
    /// Generate a random Ed25519 signing seed from an explicit RNG.
    pub fn random_with_rng(rng: &mut impl RngCore) -> Self {
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        Self(seed)
    }

    /// Generate a random Ed25519 signing seed.
    pub fn random() -> Self {
        with_key_rng(Self::random_with_rng)
    }

    /// Build an Ed25519 signing seed from exact bytes.
    pub fn from_bytes(seed: [u8; 32]) -> Self {
        Self(seed)
    }

    /// Return the raw Ed25519 signing seed.
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Borrow the raw Ed25519 signing seed.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Derive the Ed25519 public verification key.
    pub fn public_key(&self) -> Result<PublicKey<33>> {
        signers::ed25519::public_key(&self.0)
    }

    /// Sign raw message bytes with this Ed25519 seed.
    pub fn sign_raw(&self, msg: &[u8]) -> Result<[u8; 64]> {
        signers::ed25519::sign(&self.0, msg)
    }
}

impl SigningSecretKey {
    /// Sign raw message bytes using this key's algorithm.
    pub fn sign_raw(&self, msg: &[u8]) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Secp256k1(sk) => signers::secp256k1::sign_raw(*sk, msg).to_vec(),
            Self::Eip191(sk) => signers::eip191::sign_raw(*sk, msg).to_vec(),
            Self::Bip137(sk) => {
                let signature = sk.sign_hash(&signers::bip137::magic_hash(msg));
                let mut out = Vec::with_capacity(65);
                out.push(signature[64] + 27);
                out.extend_from_slice(&signature[..64]);
                out
            }
            Self::Secp256r1(sk) => {
                signers::secp256r1::sign(*sk, &signers::secp256r1::hash(msg))?.to_vec()
            }
            Self::Ed25519(sk) => sk.sign_raw(msg)?.to_vec(),
            Self::Bls12381(sk) => signers::bls::sign(*sk, msg)?.0.to_vec(),
        })
    }

    /// Generate an Ed25519 signing secret key.
    pub fn random_ed25519_with_rng(rng: &mut impl RngCore) -> Self {
        Self::Ed25519(Ed25519SecretKey::random_with_rng(rng))
    }

    /// Generate an Ed25519 signing secret key.
    pub fn random_ed25519() -> Self {
        Self::Ed25519(Ed25519SecretKey::random())
    }

    /// Generate a BLS12-381 signing secret key.
    pub fn random_bls12381() -> Result<Self> {
        signers::bls::random_sk().map(Self::Bls12381)
    }

    /// Build a BLS12-381 signing key after validating that the scalar is usable.
    pub fn try_bls12381(secret_key: SecretKey) -> Result<Self> {
        signers::bls::public_key(&secret_key)?;
        Ok(Self::Bls12381(secret_key))
    }
}

impl SigningSecretKey {
    /// Stable lower-case algorithm name.
    pub fn algorithm(&self) -> &'static str {
        match self {
            Self::Secp256k1(_) => SignatureAlgorithm::Secp256k1.as_str(),
            Self::Eip191(_) => SignatureAlgorithm::Eip191.as_str(),
            Self::Bip137(_) => SignatureAlgorithm::Bip137.as_str(),
            Self::Secp256r1(_) => SignatureAlgorithm::Secp256r1.as_str(),
            Self::Ed25519(_) => SignatureAlgorithm::Ed25519.as_str(),
            Self::Bls12381(_) => SignatureAlgorithm::Bls12381.as_str(),
        }
    }

    /// Public verification key corresponding to this signing secret.
    pub fn public_key(&self) -> Result<VerificationPublicKey> {
        Ok(match self {
            Self::Secp256k1(sk) => VerificationPublicKey::Secp256k1(sk.pubkey()),
            Self::Eip191(sk) => VerificationPublicKey::Eip191(sk.pubkey()),
            Self::Bip137(sk) => VerificationPublicKey::Bip137(sk.pubkey()),
            Self::Secp256r1(sk) => VerificationPublicKey::Secp256r1(secp256r1_public_key(*sk)?),
            Self::Ed25519(sk) => VerificationPublicKey::Ed25519(sk.public_key()?),
            Self::Bls12381(sk) => VerificationPublicKey::Bls12381(signers::bls::public_key(sk)?),
        })
    }
}

fn secp256r1_public_key(secret_key: SecretKey) -> Result<PublicKey<33>> {
    let sk_bytes: elliptic_curve::FieldBytes<p256::NistP256> = secret_key.into();
    let signing_key = ecdsa::SigningKey::<p256::NistP256>::from_bytes(&sk_bytes)?;
    let encoded = signing_key.verifying_key().to_encoded_point(false);
    let uncompressed = encoded
        .as_bytes()
        .get(1..)
        .ok_or(Error::PublicKeyBadFormat)?;
    PublicKey::from_u8(uncompressed)
}

fn with_key_rng<R>(f: impl FnOnce(&mut Hc128Rng) -> R) -> R {
    KEY_RNG.with(|rng| {
        let mut rng = rng.borrow_mut();
        f(&mut rng)
    })
}

fn public_key_from_b58m_exact<const SIZE: usize>(value: &str) -> Result<PublicKey<SIZE>> {
    let bytes = base58_monero::decode_check(value).map_err(|_| Error::PublicKeyBadFormat)?;
    PublicKey::from_exact_u8(&bytes)
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand_hc::Hc128Rng;

    use super::*;

    #[test]
    fn recoverable_account_verifier_verifies_and_recovers_key() {
        let secret =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();
        let did: Did = secret.address().into();
        let reference = AccountVerifier::Recoverable {
            algorithm: SignatureAlgorithm::Secp256k1,
            did,
        };
        let msg = b"session proof";
        let sig = secret.sign_raw(msg);

        assert_eq!(reference.did(), did);
        assert!(reference.verify(msg, sig));
        assert_eq!(
            reference.verification_key_from_signature(msg, sig).unwrap(),
            VerificationPublicKey::Secp256k1(secret.pubkey())
        );
    }

    #[test]
    fn bip137_signing_secret_signs_and_recovers_key() {
        let secret = SigningSecretKey::Bip137(
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap(),
        );
        let did = secret.public_key().unwrap().did();
        let reference = AccountVerifier::Recoverable {
            algorithm: SignatureAlgorithm::Bip137,
            did,
        };
        let msg = b"bitcoin session proof";
        let sig = secret.sign_raw(msg).unwrap();

        assert_eq!(secret.algorithm(), "bip137");
        assert!(reference.verify(msg, &sig));
        assert_eq!(
            reference
                .verification_key_from_signature(msg, &sig)
                .unwrap(),
            secret.public_key().unwrap()
        );
    }

    #[test]
    fn bls_signing_secret_signs_and_verifies_key() {
        let secret = SigningSecretKey::random_bls12381().unwrap();
        let public_key = secret.public_key().unwrap();
        let did = public_key.did();
        let reference = AccountVerifier::PublicKey(public_key.clone());
        let msg = b"bls session proof";
        let sig = secret.sign_raw(msg).unwrap();

        assert_eq!(secret.algorithm(), "bls12-381");
        assert_eq!(reference.did(), did);
        assert!(reference.verify(msg, &sig));
        assert_eq!(
            reference
                .verification_key_from_signature(msg, &sig)
                .unwrap(),
            public_key
        );

        let VerificationPublicKey::Bls12381(pk) = public_key else {
            unreachable!("random_bls12381 returns a BLS verification key");
        };
        let encoded = base58_monero::encode_check(&pk.0).unwrap();
        assert_eq!(
            AccountVerifier::from_account_parts(&encoded, "bls12-381").unwrap(),
            AccountVerifier::PublicKey(VerificationPublicKey::Bls12381(pk))
        );
    }

    #[test]
    fn ed25519_signing_secret_signs_and_verifies_key() {
        let secret = SigningSecretKey::random_ed25519();
        let public_key = secret.public_key().unwrap();
        let did = public_key.did();
        let reference = AccountVerifier::PublicKey(public_key.clone());
        let msg = b"ed25519 session proof";
        let sig = secret.sign_raw(msg).unwrap();

        assert_eq!(secret.algorithm(), "ed25519");
        assert_eq!(reference.did(), did);
        assert!(reference.verify(msg, &sig));
        assert_eq!(
            reference
                .verification_key_from_signature(msg, &sig)
                .unwrap(),
            public_key
        );

        let VerificationPublicKey::Ed25519(pk) = public_key else {
            unreachable!("random_ed25519 returns an Ed25519 verification key");
        };
        assert_eq!(
            AccountVerifier::from_account_parts(&pk.to_base58_string().unwrap(), "ed25519")
                .unwrap(),
            AccountVerifier::PublicKey(VerificationPublicKey::Ed25519(pk))
        );
    }

    #[test]
    fn ed25519_random_with_rng_is_reproducible_for_same_seed() {
        let mut rng_a = Hc128Rng::seed_from_u64(42);
        let mut rng_b = Hc128Rng::seed_from_u64(42);

        assert_eq!(
            SigningSecretKey::random_ed25519_with_rng(&mut rng_a),
            SigningSecretKey::random_ed25519_with_rng(&mut rng_b)
        );
    }

    #[test]
    fn explicit_verification_keys_domain_separate_dids() {
        let pk = SecretKey::random().pubkey();
        let secp = VerificationPublicKey::Secp256k1(pk);
        let ed = VerificationPublicKey::Ed25519(pk);

        let mut expected_transcript = b"ed25519\0".to_vec();
        expected_transcript.extend_from_slice(&pk.0);

        assert_eq!(ed.transcript_bytes(), expected_transcript);
        assert_ne!(secp.did(), ed.did());
        assert_eq!(ed.did(), AccountVerifier::PublicKey(ed.clone()).did());
    }

    #[test]
    fn secp256r1_secret_derives_p256_public_key() {
        let secret = SigningSecretKey::Secp256r1(
            SecretKey::try_from("2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038")
                .unwrap(),
        );
        let expected = PublicKey::<33>::from_hex_string(
            "17a6afd392fcbe4ac9270a599a9c5732c4f838ce35ea2234d389d8f0c367f3f5dcab906352e27289002c7f2c96039ddce7c1b5aad8b87ba94984d4c8b4f95702",
        )
        .unwrap();

        assert_eq!(
            secret.public_key().unwrap(),
            VerificationPublicKey::Secp256r1(expected)
        );
    }
}
