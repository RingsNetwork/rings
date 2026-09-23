use std::str::FromStr;

use super::DelegateeKey;
use super::DelegationBuilder;
use crate::dht::Did;
use crate::ecc::keys::SigningSecretKey;
use crate::ecc::keys::VerificationPublicKey;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;

#[test]
pub fn test_delegation_verify() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let delegation = delegatee_key.delegation();
    assert!(delegation.verify_delegator_authorization().is_ok());
}

#[test]
pub fn test_delegatee_key_clone_preserves_authority_identity() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let cloned = delegatee_key.clone();

    assert_eq!(cloned.delegator_did(), delegatee_key.delegator_did());
    assert_eq!(cloned.delegation(), delegatee_key.delegation());
    assert_eq!(
        cloned.delegatee_public_key(),
        delegatee_key.delegatee_public_key()
    );
}

#[test]
pub fn test_delegator_pubkey() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let delegation = delegatee_key.delegation();
    let pubkey = delegation.delegator_pubkey().unwrap();
    assert_eq!(key.pubkey(), pubkey);
}

#[test]
pub fn test_delegation_verify_secp256r1_delegator_key() {
    let delegator_entity = "17a6afd392fcbe4ac9270a599a9c5732c4f838ce35ea2234d389d8f0c367f3f5dcab906352e27289002c7f2c96039ddce7c1b5aad8b87ba94984d4c8b4f95702";
    let delegator_key = VerificationPublicKey::Secp256r1(
        PublicKey::<33>::from_hex_string(delegator_entity).unwrap(),
    );
    let signing_key =
        SecretKey::try_from("2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038")
            .unwrap();
    let mut builder = DelegationBuilder::new(delegator_entity.to_string(), "secp256r1".to_string());
    let proof = builder.unsigned_proof();
    let sig = signers::secp256r1::sign(&signing_key, &signers::secp256r1::hash(proof.as_bytes()))
        .unwrap();
    builder = builder.set_delegator_signature(sig.to_vec());

    let delegation = builder.build().unwrap().delegation();
    assert_eq!(
        delegation.delegator_verification_pubkey().unwrap(),
        delegator_key
    );
    assert_eq!(delegation.delegator_did(), delegator_key.did());
    assert!(delegation.verify_delegator_authorization().is_ok());
    assert!(delegation.delegator_pubkey().is_err());
}

#[test]
pub fn test_delegation_rejects_invalid_secp256r1_delegator_key() {
    let mut invalid_key = None;
    for i in 0u8..=u8::MAX {
        let mut key = [0u8; 33];
        key[0] = 2;
        key[32] = i;
        let public_key = PublicKey(key);
        let verifying_key = public_key.ct_try_into_secp256r1_pubkey();
        if !bool::from(verifying_key.is_some()) || verifying_key.unwrap().is_err() {
            invalid_key = Some(key);
            break;
        }
    }
    let delegator_entity = hex::encode(invalid_key.expect("at least one invalid P-256 x"));
    let builder = DelegationBuilder::new(delegator_entity, "secp256r1".to_string())
        .set_delegator_signature(vec![0u8; 64]);

    assert!(builder.build().is_err());
}

#[test]
pub fn test_delegation_verify_bls12381_delegator_key() {
    let signing_key = SigningSecretKey::random_bls12381().unwrap();
    let delegator_key = signing_key.public_key().unwrap();
    let VerificationPublicKey::Bls12381(raw_delegator_key) = delegator_key else {
        unreachable!("random_bls12381 returns a BLS verification key");
    };
    let delegator_entity = base58_monero::encode_check(&raw_delegator_key.0).unwrap();
    let mut builder = DelegationBuilder::new(delegator_entity, "bls12-381".to_string());
    let proof = builder.unsigned_proof();
    builder = builder.set_delegator_signature(signing_key.sign_raw(proof.as_bytes()).unwrap());

    let delegation = builder.build().unwrap().delegation();
    assert_eq!(
        delegation.delegator_verification_pubkey().unwrap(),
        VerificationPublicKey::Bls12381(raw_delegator_key)
    );
    assert_eq!(delegation.delegator_did(), delegator_key.did());
    assert!(delegation.verify_delegator_authorization().is_ok());
    assert!(delegation.delegator_pubkey().is_err());
}

#[test]
pub fn test_delegation_rejects_altered_bls12381_account_signature() {
    let signing_key = SigningSecretKey::random_bls12381().unwrap();
    let delegator_key = signing_key.public_key().unwrap();
    let VerificationPublicKey::Bls12381(raw_delegator_key) = delegator_key else {
        unreachable!("random_bls12381 returns a BLS verification key");
    };
    let delegator_entity = base58_monero::encode_check(&raw_delegator_key.0).unwrap();
    let mut builder = DelegationBuilder::new(delegator_entity, "bls12-381".to_string());
    let proof = builder.unsigned_proof();
    let unrelated_signer = SigningSecretKey::random_bls12381().unwrap();
    let signature = unrelated_signer.sign_raw(proof.as_bytes()).unwrap();
    builder = builder.set_delegator_signature(signature);

    assert!(builder.build().is_err());
}

#[test]
pub fn test_delegation_verify_ed25519_delegator_key() {
    let signing_key = SigningSecretKey::random_ed25519();
    let delegator_key = signing_key.public_key().unwrap();
    let VerificationPublicKey::Ed25519(raw_delegator_key) = delegator_key else {
        unreachable!("random_ed25519 returns an Ed25519 verification key");
    };
    let delegator_entity = raw_delegator_key.to_base58_string().unwrap();
    let mut builder = DelegationBuilder::new(delegator_entity, "ed25519".to_string());
    let proof = builder.unsigned_proof();
    builder = builder.set_delegator_signature(signing_key.sign_raw(proof.as_bytes()).unwrap());

    let delegation = builder.build().unwrap().delegation();
    assert_eq!(
        delegation.delegator_verification_pubkey().unwrap(),
        VerificationPublicKey::Ed25519(raw_delegator_key)
    );
    assert_eq!(delegation.delegator_did(), delegator_key.did());
    assert!(delegation.verify_delegator_authorization().is_ok());
    assert!(delegation.delegator_pubkey().is_err());
}

#[test]
pub fn test_dump_restore() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let dump = delegatee_key.dump().unwrap();
    let delegatee_key_copy = DelegateeKey::from_str(&dump).unwrap();
    assert_eq!(delegatee_key, delegatee_key_copy);
}

/// The content address of a fixed delegation: the trailing twenty bytes of keccak256 over its
/// postcard encoding. Frozen so a change to the encoding, the hash, or the cut is a failure
/// here before it is a wire incompatibility.
#[test]
fn test_delegation_digest_golden_vector() {
    let account = SecretKey::from_bytes([7u8; 32]).expect("fixed scalar must be a valid key");
    let delegator_did: Did = account.address().into();
    let delegatee_key = SecretKey::from_bytes([9u8; 32]).expect("fixed scalar must be a valid key");
    let delegation = super::model::Delegation {
        delegatee_did: delegatee_key.address().into(),
        delegator: super::account::Account::try_from((
            delegator_did.to_string(),
            "secp256k1".to_string(),
        ))
        .expect("fixed account must parse"),
        ttl_ms: 600_000,
        ts_ms: 1_700_000_000_000,
        delegator_signature: vec![0x11; 65],
    };
    let digest = delegation.digest().expect("fixed delegation must digest");
    assert_eq!(
        hex::encode(digest.into_bytes()),
        "c76a74b6e1c1f58a80eef20406d66157c64aad28"
    );
}
