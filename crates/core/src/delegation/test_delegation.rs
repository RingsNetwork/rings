use std::str::FromStr;

use super::Account;
use super::DelegateeKey;
use super::Delegation;
use super::DelegationBuilder;
use super::DelegatorAuthorization;
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

/// The widest account of the variant after `account`'s in a fixed chain through every variant
/// (the first variant's for `None`), or `None` after the last.
///
/// The match is exhaustive, so an account variant added later fails to compile here until it is
/// given an arm. The arm decides whether the witness covers it: the new variant must also be
/// constructed by its predecessor's arm, since an arm `Some(Account::New(_)) => None` alone
/// compiles and leaves it out. Rust cannot enumerate an enum's variants, so the chain is the
/// list, kept beside the match that forces each variant to be considered.
fn next_widest_account(account: Option<&Account>) -> Option<Account> {
    let did = Did::from(u32::MAX);
    match account {
        None => Some(Account::Secp256k1(did)),
        Some(Account::Secp256k1(_)) => Some(Account::EIP191(did)),
        Some(Account::EIP191(_)) => Some(Account::BIP137(did)),
        Some(Account::BIP137(_)) => Some(Account::Secp256r1(PublicKey([u8::MAX; 33]))),
        Some(Account::Secp256r1(_)) => Some(Account::Ed25519(PublicKey([u8::MAX; 33]))),
        Some(Account::Ed25519(_)) => Some(Account::Bls12381(PublicKey([u8::MAX; 48]))),
        Some(Account::Bls12381(_)) => None,
    }
}

/// Witness of [`Delegation::widest_for_test`]: every account at its widest, signing its
/// algorithm's signature length, encodes no wider than it, so an account or algorithm added
/// later with a wider key or signature fails here instead of leaving the envelope reserves
/// silently too small.
#[test]
fn test_the_widest_delegation_bounds_every_account() {
    let widest = Delegation::widest_for_test();
    let encoded_len = |delegation: &Delegation| rings_codec::serialize(delegation).unwrap().len();
    let accounts = std::iter::successors(next_widest_account(None), |account| {
        next_widest_account(Some(account))
    });
    for account in accounts {
        let signature_len = account.account_verifier().algorithm().signature_len();
        let delegation = Delegation {
            delegator: account,
            delegator_signature: vec![u8::MAX; signature_len],
            ..widest.clone()
        };
        assert!(encoded_len(&delegation) <= encoded_len(&widest));
    }
}

/// `DelegatorAuthorization::Established` skips the delegator's signature alone: a delegation
/// whose delegator signature is forged fails under `Verify` and passes under `Established`, as a
/// caller that verified it earlier would see; the delegatee's signature and the expiry are
/// checked under both.
#[test]
fn test_an_established_authorization_still_checks_expiry_and_the_delegatee_signature() {
    let delegatee_key = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
    let msg = b"message";
    let sig = delegatee_key.sign(msg).unwrap();
    let mut forged = delegatee_key.delegation();
    if let Some(byte) = forged.delegator_signature.first_mut() {
        *byte ^= 1;
    }
    let at_ms = forged.created_at_ms();

    assert!(forged
        .verify_under(msg, &sig, at_ms, DelegatorAuthorization::Verify)
        .is_err());
    assert!(forged
        .verify_under(msg, &sig, at_ms, DelegatorAuthorization::Established)
        .is_ok());
    assert!(forged
        .verify_under(b"other", &sig, at_ms, DelegatorAuthorization::Established)
        .is_err());
    let expired = forged.expires_at_ms() + 1;
    assert!(matches!(
        forged.verify_under(msg, &sig, expired, DelegatorAuthorization::Established),
        Err(crate::error::Error::DelegationExpired)
    ));
}

#[test]
fn test_delegatee_key_rejects_mismatched_private_key_and_preserves_bound_historical_decryption() {
    let delegator = SecretKey::random();
    let valid = DelegateeKey::new_with_seckey(&delegator).unwrap();
    let unrelated_secret = SecretKey::random();
    assert_ne!(
        Did::from(unrelated_secret.address()),
        valid.delegation().delegatee_did()
    );

    let mismatched_json = serde_json::to_string(&serde_json::json!({
        "delegation": valid.delegation(),
        "delegatee_secret_key": unrelated_secret,
    }))
    .unwrap();
    assert!(serde_json::from_str::<DelegateeKey>(&mismatched_json).is_err());

    let mismatched_dump = base58_monero::encode_check(mismatched_json.as_bytes()).unwrap();
    assert!(DelegateeKey::from_str(&mismatched_dump).is_err());
    assert!(matches!(
        DelegateeKey::try_from_parts(valid.delegation(), unrelated_secret),
        Err(crate::error::Error::DelegateeKeyMismatch { .. })
    ));

    // A genuinely bound historical key whose TTL has elapsed still restores for decryption,
    // while `verify_at` past expiry rejects it for signing.
    let historical_ts: u128 = 1_700_000_000_000;
    let historical_ttl: u64 = 60_000;
    let historical =
        DelegateeKey::new_with_seckey_at(&delegator, historical_ttl, historical_ts).unwrap();
    let historical_dump = historical.dump().unwrap();
    let restored = DelegateeKey::from_str(&historical_dump).unwrap();
    assert_eq!(restored, historical);
    assert!(restored.verify_binding().is_ok());
    assert!(matches!(
        restored.verify_at(historical_ts + u128::from(historical_ttl) + 1),
        Err(crate::error::Error::DelegationExpired)
    ));
}

#[test]
fn test_delegation_validity_lower_bound_enforced_with_skew_under_verify_and_established() {
    use crate::consts::TS_OFFSET_TOLERANCE_MS;
    use crate::message::DomainTag;
    use crate::message::MessageSigner;
    use crate::message::SigningDomain;

    let delegator = SecretKey::random();
    let ts_ms: u128 = 1_800_000_000_000;
    let ttl_ms: u64 = 600_000;
    let delegatee_key = DelegateeKey::new_with_seckey_at(&delegator, ttl_ms, ts_ms).unwrap();
    let delegation = delegatee_key.delegation();
    let msg = b"validity lower bound witness";
    let sig = delegatee_key.sign(msg).unwrap();

    let skew_edge = ts_ms - TS_OFFSET_TOLERANCE_MS;
    let before_skew = skew_edge - 1;
    let one_day_early = ts_ms - 86_400_000;

    assert!(delegation
        .verify_delegator_authorization_at(skew_edge)
        .is_ok());
    for auth in [
        DelegatorAuthorization::Verify,
        DelegatorAuthorization::Established,
    ] {
        assert!(delegation.verify_under(msg, &sig, skew_edge, auth).is_ok());
        for early in [before_skew, one_day_early] {
            assert!(matches!(
                delegation.verify_under(msg, &sig, early, auth),
                Err(crate::error::Error::DelegationNotYetValid)
            ));
        }
    }
    assert!(matches!(
        delegation.verify_delegator_authorization_at(before_skew),
        Err(crate::error::Error::DelegationNotYetValid)
    ));

    // A message stamped at `one_day_early` under a delegation dated `ts_ms` (one day ahead) must
    // fail `MessageVerification::verify_live_at(..., one_day_early)`.
    let tag = DomainTag::new("test-delegation-lower-bound");
    let domain = SigningDomain::new(tag, 1);
    let proof = MessageSigner::new(&delegatee_key, 1)
        .sign_at(tag, msg, one_day_early)
        .unwrap();
    assert!(proof.is_live_at(one_day_early));
    assert!(!proof.verify_live_at(domain, msg, one_day_early));
}

#[test]
fn test_delegation_proof_binds_delegator_and_algorithm_against_cross_replay() {
    let delegator_a = SecretKey::random();
    let delegator_b = SecretKey::random();
    let delegatee = SecretKey::random();
    let ts_ms: u128 = 1_800_000_000_000;
    let ttl_ms: u64 = 600_000;

    let valid_a = DelegateeKey::new_with_seckey_at(&delegator_a, ttl_ms, ts_ms).unwrap();
    let delegation_a = valid_a.delegation();
    assert!(delegation_a
        .verify_delegator_authorization_at(ts_ms)
        .is_ok());

    let replayed_to_b = Delegation {
        delegatee_did: delegation_a.delegatee_did,
        delegator: Account::Secp256k1(delegator_b.address().into()),
        ttl_ms: delegation_a.ttl_ms,
        ts_ms: delegation_a.ts_ms,
        delegator_signature: delegation_a.delegator_signature.clone(),
    };
    assert!(replayed_to_b
        .verify_delegator_authorization_at(ts_ms)
        .is_err());

    let replayed_as_eip191 = Delegation {
        delegatee_did: delegation_a.delegatee_did,
        delegator: Account::EIP191(delegator_a.address().into()),
        ttl_ms: delegation_a.ttl_ms,
        ts_ms: delegation_a.ts_ms,
        delegator_signature: delegation_a.delegator_signature,
    };
    assert!(replayed_as_eip191
        .verify_delegator_authorization_at(ts_ms)
        .is_err());

    // Ensure non-recoverable delegator signature also binds delegator_did.
    let ed_key = SigningSecretKey::random_ed25519();
    let VerificationPublicKey::Ed25519(ed_pk) = ed_key.public_key().unwrap() else {
        unreachable!("random_ed25519 produces Ed25519 public key");
    };
    let mut builder =
        DelegationBuilder::new(ed_pk.to_base58_string().unwrap(), "ed25519".to_string());
    let proof = builder.unsigned_proof();
    assert!(proof.starts_with("rings-delegation-v1\ned25519\n"));
    builder = builder.set_delegator_signature(ed_key.sign_raw(proof.as_bytes()).unwrap());
    let built = builder.build().unwrap().delegation();
    assert!(built.verify_delegator_authorization().is_ok());

    let other_ed = SigningSecretKey::random_ed25519();
    let VerificationPublicKey::Ed25519(other_ed_pk) = other_ed.public_key().unwrap() else {
        unreachable!("random_ed25519 produces Ed25519 public key");
    };
    let swapped_ed = Delegation {
        delegatee_did: delegatee.address().into(),
        ..built.clone()
    };
    assert!(swapped_ed.verify_delegator_authorization().is_err());
    let swapped_ed_delegator = Delegation {
        delegator: Account::Ed25519(other_ed_pk),
        ..built
    };
    assert!(swapped_ed_delegator
        .verify_delegator_authorization()
        .is_err());
}

/// The cost of verifying one message proof's delegation, with the delegator's signature
/// recovered (`Verify`) and established (`Established`). Run with
/// `--release -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn bench_delegation_verification_by_authorization() {
    const RUNS: u32 = 2000;
    let delegatee_key = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
    let msg = b"message";
    let sig = delegatee_key.sign(msg).unwrap();
    let delegation = delegatee_key.delegation();
    let at_ms = delegation.created_at_ms();
    for authorization in [
        DelegatorAuthorization::Verify,
        DelegatorAuthorization::Established,
    ] {
        let started = std::time::Instant::now();
        for _ in 0..RUNS {
            delegation
                .verify_under(msg, &sig, at_ms, authorization)
                .unwrap();
        }
        println!(
            "{authorization:?}: {:?} per proof",
            started.elapsed() / RUNS
        );
    }
}
