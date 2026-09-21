use std::str::FromStr;

use super::SessionSk;
use super::SessionSkBuilder;
use crate::dht::Did;
use crate::ecc::keys::SigningSecretKey;
use crate::ecc::keys::VerificationPublicKey;
use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;

#[test]
pub fn test_session_verify() {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();
    let session = sm.session();
    assert!(session.verify_self().is_ok());
}

#[test]
pub fn test_session_sk_clone_preserves_authority_identity() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let cloned = session_sk.clone();

    assert_eq!(cloned.account_did(), session_sk.account_did());
    assert_eq!(cloned.session(), session_sk.session());
    assert_eq!(cloned.session_public_key(), session_sk.session_public_key());
}

#[test]
pub fn test_account_pubkey() {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();
    let session = sm.session();
    let pubkey = session.account_pubkey().unwrap();
    assert_eq!(key.pubkey(), pubkey);
}

#[test]
pub fn test_session_verify_secp256r1_account_key() {
    let account_entity = "17a6afd392fcbe4ac9270a599a9c5732c4f838ce35ea2234d389d8f0c367f3f5dcab906352e27289002c7f2c96039ddce7c1b5aad8b87ba94984d4c8b4f95702";
    let account_key =
        VerificationPublicKey::Secp256r1(PublicKey::<33>::from_hex_string(account_entity).unwrap());
    let signing_key =
        SecretKey::try_from("2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038")
            .unwrap();
    let mut builder = SessionSkBuilder::new(account_entity.to_string(), "secp256r1".to_string());
    let proof = builder.unsigned_proof();
    let sig = signers::secp256r1::sign(&signing_key, &signers::secp256r1::hash(proof.as_bytes()))
        .unwrap();
    builder = builder.set_session_sig(sig.to_vec());

    let session = builder.build().unwrap().session();
    assert_eq!(session.account_verification_pubkey().unwrap(), account_key);
    assert_eq!(session.account_did(), account_key.did());
    assert!(session.verify_self().is_ok());
    assert!(session.account_pubkey().is_err());
}

#[test]
pub fn test_session_rejects_invalid_secp256r1_account_key() {
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
    let account_entity = hex::encode(invalid_key.expect("at least one invalid P-256 x"));
    let builder = SessionSkBuilder::new(account_entity, "secp256r1".to_string())
        .set_session_sig(vec![0u8; 64]);

    assert!(!builder.validate_account());
    assert!(builder.build().is_err());
}

#[test]
pub fn test_session_verify_bls12381_account_key() {
    let signing_key = SigningSecretKey::random_bls12381().unwrap();
    let account_key = signing_key.public_key().unwrap();
    let VerificationPublicKey::Bls12381(raw_account_key) = account_key else {
        unreachable!("random_bls12381 returns a BLS verification key");
    };
    let account_entity = base58_monero::encode_check(&raw_account_key.0).unwrap();
    let mut builder = SessionSkBuilder::new(account_entity, "bls12-381".to_string());
    let proof = builder.unsigned_proof();
    builder = builder.set_session_sig(signing_key.sign_raw(proof.as_bytes()).unwrap());

    let session = builder.build().unwrap().session();
    assert_eq!(
        session.account_verification_pubkey().unwrap(),
        VerificationPublicKey::Bls12381(raw_account_key)
    );
    assert_eq!(session.account_did(), account_key.did());
    assert!(session.verify_self().is_ok());
    assert!(session.account_pubkey().is_err());
}

#[test]
pub fn test_session_verify_ed25519_account_key() {
    let signing_key = SigningSecretKey::random_ed25519();
    let account_key = signing_key.public_key().unwrap();
    let VerificationPublicKey::Ed25519(raw_account_key) = account_key else {
        unreachable!("random_ed25519 returns an Ed25519 verification key");
    };
    let account_entity = raw_account_key.to_base58_string().unwrap();
    let mut builder = SessionSkBuilder::new(account_entity, "ed25519".to_string());
    let proof = builder.unsigned_proof();
    builder = builder.set_session_sig(signing_key.sign_raw(proof.as_bytes()).unwrap());

    let session = builder.build().unwrap().session();
    assert_eq!(
        session.account_verification_pubkey().unwrap(),
        VerificationPublicKey::Ed25519(raw_account_key)
    );
    assert_eq!(session.account_did(), account_key.did());
    assert!(session.verify_self().is_ok());
    assert!(session.account_pubkey().is_err());
}

#[test]
pub fn test_dump_restore() {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();
    let dump = sm.dump().unwrap();
    let sm2 = SessionSk::from_str(&dump).unwrap();
    assert_eq!(sm, sm2);
}

/// The content address of a fixed delegation: the trailing twenty bytes of keccak256 over its
/// postcard encoding. Frozen so a change to the encoding, the hash, or the cut is a failure
/// here before it is a wire incompatibility.
#[test]
fn test_session_digest_golden_vector() {
    let account = SecretKey::from_bytes([7u8; 32]).expect("fixed scalar must be a valid key");
    let account_did: Did = account.address().into();
    let session_key = SecretKey::from_bytes([9u8; 32]).expect("fixed scalar must be a valid key");
    let session = super::model::Session {
        session_id: session_key.address().into(),
        account: super::account::Account::try_from((
            account_did.to_string(),
            "secp256k1".to_string(),
        ))
        .expect("fixed account must parse"),
        ttl_ms: 600_000,
        ts_ms: 1_700_000_000_000,
        sig: vec![0x11; 65],
    };
    let digest = session.digest().expect("fixed session must digest");
    assert_eq!(
        hex::encode(digest.into_bytes()),
        "c76a74b6e1c1f58a80eef20406d66157c64aad28"
    );
}
