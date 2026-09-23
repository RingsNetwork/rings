use super::*;
use crate::delegation::DelegateeKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::message::HopBudget;
use crate::message::MessageRelay;

fn signed_receipt(
    network_id: u32,
) -> Result<(ProvisionalServiceReceipt, DelegateeKey, DelegateeKey)> {
    let provider = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let beneficiary_key = SecretKey::random();
    let beneficiary = DelegateeKey::new_with_seckey(&beneficiary_key)?;
    let rotated_beneficiary = DelegateeKey::new_with_seckey(&beneficiary_key)?;
    let now_seconds = u64::try_from(crate::utils::get_epoch_ms() / 1_000)
        .map_err(|_| Error::ServiceReceipt(ServiceReceiptError::ObservationTimeOverflow))?;
    let claim = ProvisionalServiceClaim::probe(
        network_id,
        provider.delegator_did(),
        beneficiary.delegator_did(),
        ProvisionalEpoch::from_unix_seconds(now_seconds),
        [3; 32],
        [4; 32],
        [5; 32],
    );
    let provider_attestation = claim.sign_provider(MessageSigner::new(&provider, network_id))?;
    let beneficiary_attestation =
        claim.sign_beneficiary(MessageSigner::new(&rotated_beneficiary, network_id))?;
    Ok((
        ProvisionalServiceReceipt::new(claim, provider_attestation, beneficiary_attestation)?,
        provider,
        beneficiary,
    ))
}

fn receipt_signed_at(
    network_id: u32,
    epoch: ProvisionalEpoch,
    provider_ts_ms: u128,
    beneficiary_ts_ms: u128,
) -> Result<ProvisionalServiceReceipt> {
    let provider_account = SecretKey::random();
    let beneficiary_account = SecretKey::random();
    let provider = DelegateeKey::from_test_keys(
        &provider_account,
        SecretKey::random(),
        provider_ts_ms,
        crate::consts::DEFAULT_DELEGATION_TTL_MS,
    )?;
    let beneficiary = DelegateeKey::from_test_keys(
        &beneficiary_account,
        SecretKey::random(),
        beneficiary_ts_ms,
        crate::consts::DEFAULT_DELEGATION_TTL_MS,
    )?;
    let claim = ProvisionalServiceClaim::probe(
        network_id,
        provider.delegator_did(),
        beneficiary.delegator_did(),
        epoch,
        [13; 32],
        [14; 32],
        [15; 32],
    );
    let bytes = claim.canonical_bytes()?;
    let provider_attestation = MessageSigner::new(&provider, network_id).sign_at(
        PROVIDER_DOMAIN,
        &bytes,
        provider_ts_ms,
    )?;
    let beneficiary_attestation = MessageSigner::new(&beneficiary, network_id).sign_at(
        BENEFICIARY_DOMAIN,
        &bytes,
        beneficiary_ts_ms,
    )?;
    ProvisionalServiceReceipt::new(claim, provider_attestation, beneficiary_attestation)
        .map_err(Error::from)
}

#[test]
fn session_rotation_preserves_account_roles() -> Result<()> {
    let (receipt, _, _) = signed_receipt(7)?;
    receipt.verify_crypto(7)?;
    Ok(())
}

#[test]
fn mismatched_delegated_account_and_role_swaps_fail_closed() -> Result<()> {
    let (receipt, provider, beneficiary) = signed_receipt(7)?;
    let wrong = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    assert!(matches!(
        receipt
            .claim
            .sign_beneficiary(MessageSigner::new(&wrong, 7)),
        Err(Error::ServiceReceipt(
            ServiceReceiptError::SignerRoleMismatch { .. }
        ))
    ));

    let swapped = ProvisionalServiceReceipt::new(
        receipt.claim.clone(),
        receipt
            .claim
            .sign_beneficiary(MessageSigner::new(&beneficiary, 7))?,
        receipt
            .claim
            .sign_provider(MessageSigner::new(&provider, 7))?,
    )?;
    assert!(matches!(
        swapped.verify_crypto(7),
        Err(ServiceReceiptError::SignerRoleMismatch { .. })
    ));
    Ok(())
}

#[test]
fn every_claim_field_is_covered_by_the_role_signatures() -> Result<()> {
    let (receipt, _, _) = signed_receipt(7)?;
    let mut tampered = Vec::new();

    let mut network = receipt.clone();
    network.claim.network_id = 8;
    tampered.push(network);
    let mut provider = receipt.clone();
    provider.claim.provider_account = Did::from(11_u32);
    tampered.push(provider);
    let mut beneficiary = receipt.clone();
    beneficiary.claim.beneficiary_account = Did::from(12_u32);
    tampered.push(beneficiary);
    let mut epoch = receipt.clone();
    epoch.claim.epoch.slot = epoch.claim.epoch.slot.saturating_add(1);
    tampered.push(epoch);
    let mut nonce = receipt.clone();
    nonce.claim.nonce = [9; 32];
    tampered.push(nonce);
    let mut units = receipt.clone();
    units.claim.units =
        NonZeroU64::new(2).ok_or(ServiceReceiptError::InvalidProbeUnits { units: 0 })?;
    tampered.push(units);
    let mut request = receipt.clone();
    request.claim.request_digest = [10; 32];
    tampered.push(request);
    let mut completion = receipt;
    completion.claim.completion_digest = [11; 32];
    tampered.push(completion);

    assert!(tampered
        .iter()
        .all(|candidate| candidate.verify_crypto(7).is_err()));
    Ok(())
}

#[test]
fn live_admission_rejects_a_validly_signed_stale_epoch() -> Result<()> {
    let observed_at = crate::utils::get_epoch_ms();
    let observed_seconds = u64::try_from(observed_at / 1_000)
        .map_err(|_| Error::ServiceReceipt(ServiceReceiptError::ObservationTimeOverflow))?;
    let observed_epoch = ProvisionalEpoch::from_unix_seconds(observed_seconds);
    let stale_epoch = ProvisionalEpoch {
        slot: observed_epoch.slot.saturating_sub(3),
    };
    let receipt = receipt_signed_at(7, stale_epoch, observed_at, observed_at)?;

    assert_eq!(
        receipt.verify_live_at(7, observed_at),
        Err(ServiceReceiptError::EpochOutsideTolerance {
            claim_slot: stale_epoch.slot,
            observed_slot: observed_epoch.slot,
        })
    );
    Ok(())
}

#[test]
fn live_admission_distinguishes_expired_provider_and_beneficiary_proofs() -> Result<()> {
    let observed_at = crate::utils::get_epoch_ms();
    let observed_seconds = u64::try_from(observed_at / 1_000)
        .map_err(|_| Error::ServiceReceipt(ServiceReceiptError::ObservationTimeOverflow))?;
    let epoch = ProvisionalEpoch::from_unix_seconds(observed_seconds);
    let expired_ts = observed_at.saturating_sub(u128::from(crate::consts::DEFAULT_TTL_MS) + 1);
    let expired_provider = receipt_signed_at(7, epoch, expired_ts, observed_at)?;
    let expired_beneficiary = receipt_signed_at(7, epoch, observed_at, expired_ts)?;

    expired_provider.verify_crypto(7)?;
    expired_beneficiary.verify_crypto(7)?;
    assert_eq!(
        expired_provider.verify_live_at(7, observed_at),
        Err(ServiceReceiptError::ProviderAttestationNotLive)
    );
    assert_eq!(
        expired_beneficiary.verify_live_at(7, observed_at),
        Err(ServiceReceiptError::BeneficiaryAttestationNotLive)
    );
    Ok(())
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn live_admission_rejects_proofs_signed_before_their_sessions_expired() -> Result<()> {
    const NETWORK_ID: u32 = 7;
    const SESSION_CREATED_AT_MS: u128 = 1_700_000_000_000;
    const SESSION_TTL_MS: u64 = 5;
    const SIGNED_AT_MS: u128 = SESSION_CREATED_AT_MS + 1;
    const OBSERVED_AT_MS: u128 = SESSION_CREATED_AT_MS + 6;
    const LONG_SESSION_TTL_MS: u64 = 100;

    let provider = DelegateeKey::from_test_keys(
        &SecretKey::random(),
        SecretKey::random(),
        SESSION_CREATED_AT_MS,
        SESSION_TTL_MS,
    )?;
    let beneficiary = DelegateeKey::from_test_keys(
        &SecretKey::random(),
        SecretKey::random(),
        SESSION_CREATED_AT_MS,
        LONG_SESSION_TTL_MS,
    )?;
    let claim = ProvisionalServiceClaim::probe(
        NETWORK_ID,
        provider.delegator_did(),
        beneficiary.delegator_did(),
        ProvisionalEpoch::from_unix_seconds(u64::try_from(OBSERVED_AT_MS / 1_000).unwrap_or(0)),
        [21; 32],
        [22; 32],
        [23; 32],
    );
    let bytes = claim.canonical_bytes()?;
    let provider_attestation =
        MessageSigner::new(&provider, NETWORK_ID).sign_at(PROVIDER_DOMAIN, &bytes, SIGNED_AT_MS)?;
    let beneficiary_attestation = MessageSigner::new(&beneficiary, NETWORK_ID).sign_at(
        BENEFICIARY_DOMAIN,
        &bytes,
        SIGNED_AT_MS,
    )?;
    let provider_expired = ProvisionalServiceReceipt::new(
        claim.clone(),
        provider_attestation.clone(),
        beneficiary_attestation.clone(),
    )?;

    provider_expired.verify_crypto(NETWORK_ID)?;
    assert_eq!(
        provider_expired.verify_live_at(NETWORK_ID, OBSERVED_AT_MS),
        Err(ServiceReceiptError::ProviderAttestationNotLive)
    );

    let short_beneficiary = DelegateeKey::from_test_keys(
        &SecretKey::random(),
        SecretKey::random(),
        SESSION_CREATED_AT_MS,
        SESSION_TTL_MS,
    )?;
    let long_provider = DelegateeKey::from_test_keys(
        &SecretKey::random(),
        SecretKey::random(),
        SESSION_CREATED_AT_MS,
        LONG_SESSION_TTL_MS,
    )?;
    let claim = ProvisionalServiceClaim::probe(
        NETWORK_ID,
        long_provider.delegator_did(),
        short_beneficiary.delegator_did(),
        ProvisionalEpoch::from_unix_seconds(u64::try_from(OBSERVED_AT_MS / 1_000).unwrap_or(0)),
        [24; 32],
        [25; 32],
        [26; 32],
    );
    let bytes = claim.canonical_bytes()?;
    let beneficiary_expired = ProvisionalServiceReceipt::new(
        claim,
        MessageSigner::new(&long_provider, NETWORK_ID).sign_at(
            PROVIDER_DOMAIN,
            &bytes,
            SIGNED_AT_MS,
        )?,
        MessageSigner::new(&short_beneficiary, NETWORK_ID).sign_at(
            BENEFICIARY_DOMAIN,
            &bytes,
            SIGNED_AT_MS,
        )?,
    )?;

    beneficiary_expired.verify_crypto(NETWORK_ID)?;
    assert_eq!(
        beneficiary_expired.verify_live_at(NETWORK_ID, OBSERVED_AT_MS),
        Err(ServiceReceiptError::BeneficiaryAttestationNotLive)
    );
    Ok(())
}

#[test]
fn live_admission_rejects_observation_time_overflow() -> Result<()> {
    let (receipt, _, _) = signed_receipt(7)?;

    assert_eq!(
        receipt.verify_live_at(7, u128::MAX),
        Err(ServiceReceiptError::ObservationTimeOverflow)
    );
    Ok(())
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn canonical_receipt_golden_vector_is_stable() -> Result<()> {
    const CREATED_AT_MS: u128 = 1_700_000_000_000;
    const SESSION_TTL_MS: u64 = 86_400_000;
    const SIGNED_AT_MS: u128 = CREATED_AT_MS + 123;
    let provider_account =
        SecretKey::try_from("0000000000000000000000000000000000000000000000000000000000000001")?;
    let provider_delegatee_key =
        SecretKey::try_from("0000000000000000000000000000000000000000000000000000000000000002")?;
    let beneficiary_account =
        SecretKey::try_from("0000000000000000000000000000000000000000000000000000000000000003")?;
    let beneficiary_delegatee_key =
        SecretKey::try_from("0000000000000000000000000000000000000000000000000000000000000004")?;
    let provider = DelegateeKey::from_test_keys(
        &provider_account,
        provider_delegatee_key,
        CREATED_AT_MS,
        SESSION_TTL_MS,
    )?;
    let beneficiary = DelegateeKey::from_test_keys(
        &beneficiary_account,
        beneficiary_delegatee_key,
        CREATED_AT_MS,
        SESSION_TTL_MS,
    )?;
    let claim = ProvisionalServiceClaim::probe(
        7,
        provider.delegator_did(),
        beneficiary.delegator_did(),
        ProvisionalEpoch { slot: 3 },
        [4; 32],
        [5; 32],
        [6; 32],
    );
    let bytes = claim.canonical_bytes()?;
    let receipt = ProvisionalServiceReceipt::new(
        claim,
        MessageSigner::new(&provider, 7).sign_at(PROVIDER_DOMAIN, &bytes, SIGNED_AT_MS)?,
        MessageSigner::new(&beneficiary, 7).sign_at(BENEFICIARY_DOMAIN, &bytes, SIGNED_AT_MS)?,
    )?;
    let canonical = receipt.canonical_bytes()?;

    receipt.verify_crypto(7)?;
    assert_eq!(
        hex::encode(&canonical),
        "52494e47532d50524f564953494f4e414c2d534552564943452d524543454950540007002a3078376535663435353230393161363931323564356466636237623863323635393032393339356264662a30783638313365623933363233373265656636323030663362316462633366383139363731636261363903040404040404040404040404040404040404040404040404040404040404040401050505050505050505050505050505050505050505050505050505050505050506060606060606060606060606060606060606060606060606060606060606062a307832623561643563343739356330323635313466383331376337613231356532313864636364366366002a30783765356634353532303931613639313235643564666362376238633236353930323933393562646680b8992980d095ffbc314134cac11c98d6784d088d0e0bf905f42d58c14c585cd8504f98760b32e2a0a9204da47050b5f48d4dae7087c1fdb67c97470491af0902ec15878b4363c8732fa601c0cf24fbd095ffbc31413c18de86cd757a85e2492f44fe7a33b0df600658b1c9ecd79929832be7d7f3040e2c616155b6165e4be7d9364aadac65696ce0d63314a68ef5f335153f6a326b002a307831656666343762633361313061343564346232333062356431306533373735316665366161373138002a30783638313365623933363233373265656636323030663362316462633366383139363731636261363980b8992980d095ffbc3141a0a355a84db0e7604c37aac193c5f6c4206acf103247a13a2feefa340c52c7686f52528da21a1fde9fac4199118fdd6dde83fc0b375a0cfa6e68441baef15a2e00c0cf24fbd095ffbc3141fc362d49b9bac182bdbaf599dd45d248bde7118141b0f5333fbc98c40e1172730875f9901f38d15ab10c9191ebceb4ece3d14fd208771453f6d8b97b3c0bb50201"
    );
    assert_eq!(
        hex::encode(receipt.digest()?.into_bytes()),
        "8809ac8b3658fb709c3a672d3069d1631c6b594e278739174d01c47fde07170c"
    );
    assert_eq!(
        ProvisionalServiceReceipt::from_canonical_bytes(&canonical)?,
        receipt
    );
    Ok(())
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn canonical_claim_golden_vector_is_stable() -> Result<()> {
    let claim = ProvisionalServiceClaim::probe(
        7,
        Did::from(1_u32),
        Did::from(2_u32),
        ProvisionalEpoch { slot: 3 },
        [4; 32],
        [5; 32],
        [6; 32],
    );
    let bytes = claim.canonical_bytes()?;
    assert_eq!(
        hex::encode(&bytes),
        "52494e47532d50524f564953494f4e414c2d534552564943452d434c41494d0007002a3078303030303030303030303030303030303030303030303030303030303030303030303030303030312a3078303030303030303030303030303030303030303030303030303030303030303030303030303030320304040404040404040404040404040404040404040404040404040404040404040105050505050505050505050505050505050505050505050505050505050505050606060606060606060606060606060606060606060606060606060606060606"
    );
    assert_eq!(
        hex::encode(claim.digest()?.into_bytes()),
        "eade6ef8b37f47e13d206a7720f2fd66c792e500ccca26b74ac6fac76fb84cc7"
    );
    assert_eq!(
        ProvisionalServiceClaim::from_canonical_bytes(&bytes)?,
        claim
    );
    Ok(())
}

#[derive(Serialize)]
struct RawClaim {
    network_id: u32,
    service: u32,
    provider_account: Did,
    beneficiary_account: Did,
    epoch: ProvisionalEpoch,
    nonce: [u8; 32],
    units: u64,
    request_digest: [u8; 32],
    completion_digest: [u8; 32],
}

fn raw_claim(service: u32, units: u64) -> std::result::Result<Vec<u8>, ServiceReceiptError> {
    encode_prefixed(CLAIM_WIRE_PREFIX, &RawClaim {
        network_id: 7,
        service,
        provider_account: Did::from(1_u32),
        beneficiary_account: Did::from(2_u32),
        epoch: ProvisionalEpoch { slot: 3 },
        nonce: [4; 32],
        units,
        request_digest: [5; 32],
        completion_digest: [6; 32],
    })
}

#[test]
fn zero_units_and_unknown_service_kind_fail_decoding() -> Result<()> {
    assert!(ProvisionalServiceClaim::from_canonical_bytes(&raw_claim(0, 0)?).is_err());
    assert!(ProvisionalServiceClaim::from_canonical_bytes(&raw_claim(1, 1)?).is_err());
    Ok(())
}

#[test]
fn probe_offer_verifies_the_exact_signed_request_and_completion() -> Result<()> {
    let network_id = 7;
    let provider = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let beneficiary = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let provider_signer = MessageSigner::new(&provider, network_id);
    let beneficiary_signer = MessageSigner::new(&beneficiary, network_id);
    let provider_did = provider.delegator_did();
    let beneficiary_did = beneficiary.delegator_did();
    let request = ProbeRequest {
        epoch: ProvisionalEpoch::from_unix_seconds(
            u64::try_from(crate::utils::get_epoch_ms() / 1_000).unwrap_or(0),
        ),
        nonce: [8; 32],
    };
    let tx_id = uuid::Uuid::new_v4();
    let request_transaction = Transaction::new(
        provider_did,
        tx_id,
        1,
        Message::ProbeRequest(request),
        beneficiary_signer,
    )?;
    let request_digest = request_transaction.digest()?.into_bytes();
    let completion = Transaction::new(
        beneficiary_did,
        tx_id,
        1,
        ProbeCompletion {
            request_digest,
            nonce: request.nonce,
        },
        provider_signer,
    )?;
    let claim = ProvisionalServiceClaim::probe(
        network_id,
        provider_did,
        beneficiary_did,
        request.epoch,
        request.nonce,
        request_digest,
        completion.digest()?.into_bytes(),
    );
    let offer = ProbeOffer {
        request: request_transaction,
        completion,
        provider_attestation: claim.sign_provider(provider_signer)?,
        claim,
    };
    let outer_transaction = Transaction::new(
        beneficiary_did,
        tx_id,
        2,
        Message::ProbeOffer(Box::new(offer.clone())),
        provider_signer,
    )?;
    let outer = MessagePayload::new(
        outer_transaction,
        provider_signer,
        MessageRelay::new(beneficiary_did, beneficiary_did, HopBudget::MAX),
    )?;

    assert_eq!(
        offer.verify_transcript(&outer, network_id, beneficiary_did)?,
        request
    );
    assert_eq!(
        offer.verify_live_transcript_at(
            &outer,
            network_id,
            beneficiary_did,
            crate::utils::get_epoch_ms(),
        )?,
        request
    );

    let mut tampered = offer;
    tampered.completion.sequence = tampered.completion.sequence.saturating_add(1);
    assert_eq!(
        tampered.verify_transcript(&outer, network_id, beneficiary_did),
        Err(ServiceReceiptError::InvalidCompletionTransaction)
    );
    Ok(())
}

#[test]
fn probe_offer_rejects_an_attestation_from_an_expired_provider_session() -> Result<()> {
    const NETWORK_ID: u32 = 7;
    const SESSION_TTL_MS: u64 = 5;
    let observed_at_ms = crate::utils::get_epoch_ms();
    let session_created_at_ms = observed_at_ms.saturating_sub(u128::from(SESSION_TTL_MS) + 1);
    let provider_account = SecretKey::random();
    let provider_transport = DelegateeKey::new_with_seckey(&provider_account)?;
    let provider_attestation_session = DelegateeKey::from_test_keys(
        &provider_account,
        SecretKey::random(),
        session_created_at_ms,
        SESSION_TTL_MS,
    )?;
    let beneficiary = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let provider_signer = MessageSigner::new(&provider_transport, NETWORK_ID);
    let beneficiary_signer = MessageSigner::new(&beneficiary, NETWORK_ID);
    let provider_did = provider_transport.delegator_did();
    let beneficiary_did = beneficiary.delegator_did();
    let request = ProbeRequest {
        epoch: ProvisionalEpoch::from_unix_seconds(
            u64::try_from(observed_at_ms / 1_000).unwrap_or(0),
        ),
        nonce: [27; 32],
    };
    let tx_id = uuid::Uuid::new_v4();
    let request_transaction = Transaction::new(
        provider_did,
        tx_id,
        1,
        Message::ProbeRequest(request),
        beneficiary_signer,
    )?;
    let request_digest = request_transaction.digest()?.into_bytes();
    let completion = Transaction::new(
        beneficiary_did,
        tx_id,
        1,
        ProbeCompletion {
            request_digest,
            nonce: request.nonce,
        },
        provider_signer,
    )?;
    let claim = ProvisionalServiceClaim::probe(
        NETWORK_ID,
        provider_did,
        beneficiary_did,
        request.epoch,
        request.nonce,
        request_digest,
        completion.digest()?.into_bytes(),
    );
    let offer = ProbeOffer {
        request: request_transaction,
        completion,
        provider_attestation: MessageSigner::new(&provider_attestation_session, NETWORK_ID)
            .sign_at(
                PROVIDER_DOMAIN,
                &claim.canonical_bytes()?,
                session_created_at_ms,
            )?,
        claim,
    };
    let outer_transaction = Transaction::new(
        beneficiary_did,
        tx_id,
        2,
        Message::ProbeOffer(Box::new(offer.clone())),
        provider_signer,
    )?;
    let outer = MessagePayload::new(
        outer_transaction,
        provider_signer,
        MessageRelay::new(beneficiary_did, beneficiary_did, HopBudget::MAX),
    )?;

    assert_eq!(
        offer.verify_live_transcript_at(&outer, NETWORK_ID, beneficiary_did, observed_at_ms,),
        Err(ServiceReceiptError::ProviderAttestationNotLive)
    );
    Ok(())
}

#[test]
fn probe_offer_judges_embedded_transaction_sessions_at_observation_time() -> Result<()> {
    const NETWORK_ID: u32 = 7;
    const SESSION_TTL_MS: u64 = 60_000;
    let created_at_ms = crate::utils::get_epoch_ms();
    let observed_at_ms = created_at_ms + u128::from(SESSION_TTL_MS) + 1;
    let beneficiary_account = SecretKey::random();
    let beneficiary = DelegateeKey::from_test_keys(
        &beneficiary_account,
        SecretKey::random(),
        created_at_ms,
        SESSION_TTL_MS,
    )?;
    let provider = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let provider_signer = MessageSigner::new(&provider, NETWORK_ID);
    let beneficiary_signer = MessageSigner::new(&beneficiary, NETWORK_ID);
    let provider_did = provider.delegator_did();
    let beneficiary_did = beneficiary.delegator_did();
    let request = ProbeRequest {
        epoch: ProvisionalEpoch::from_unix_seconds(
            u64::try_from(observed_at_ms / 1_000).unwrap_or(0),
        ),
        nonce: [28; 32],
    };
    let tx_id = uuid::Uuid::new_v4();
    let request_transaction = Transaction::new(
        provider_did,
        tx_id,
        1,
        Message::ProbeRequest(request),
        beneficiary_signer,
    )?;
    let request_digest = request_transaction.digest()?.into_bytes();
    let completion = Transaction::new(
        beneficiary_did,
        tx_id,
        1,
        ProbeCompletion {
            request_digest,
            nonce: request.nonce,
        },
        provider_signer,
    )?;
    let claim = ProvisionalServiceClaim::probe(
        NETWORK_ID,
        provider_did,
        beneficiary_did,
        request.epoch,
        request.nonce,
        request_digest,
        completion.digest()?.into_bytes(),
    );
    let offer = ProbeOffer {
        request: request_transaction,
        completion,
        provider_attestation: claim.sign_provider(provider_signer)?,
        claim,
    };
    let outer_transaction = Transaction::new(
        beneficiary_did,
        tx_id,
        2,
        Message::ProbeOffer(Box::new(offer.clone())),
        provider_signer,
    )?;
    let outer = MessagePayload::new(
        outer_transaction,
        provider_signer,
        MessageRelay::new(beneficiary_did, beneficiary_did, HopBudget::MAX),
    )?;

    offer.verify_transcript(&outer, NETWORK_ID, beneficiary_did)?;
    assert_eq!(
        offer.verify_live_transcript_at(&outer, NETWORK_ID, beneficiary_did, observed_at_ms),
        Err(ServiceReceiptError::InvalidRequestTransaction)
    );
    Ok(())
}
