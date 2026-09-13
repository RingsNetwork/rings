use super::*;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::message::HopBudget;
use crate::message::MessageRelay;
use crate::session::SessionSk;

fn signed_receipt(network_id: u32) -> Result<(ProvisionalServiceReceiptV1, SessionSk, SessionSk)> {
    let provider = SessionSk::new_with_seckey(&SecretKey::random())?;
    let beneficiary_key = SecretKey::random();
    let beneficiary = SessionSk::new_with_seckey(&beneficiary_key)?;
    let rotated_beneficiary = SessionSk::new_with_seckey(&beneficiary_key)?;
    let now_seconds = u64::try_from(crate::utils::get_epoch_ms() / 1_000)
        .map_err(|_| Error::ServiceReceipt(ServiceReceiptError::ObservationTimeOverflow))?;
    let claim = ProvisionalServiceClaimV1::probe(
        network_id,
        provider.account_did(),
        beneficiary.account_did(),
        ProvisionalEpochV1::from_unix_seconds(now_seconds),
        [3; 32],
        [4; 32],
        [5; 32],
    );
    let provider_attestation = claim.sign_provider(MessageSigner::new(&provider, network_id))?;
    let beneficiary_attestation =
        claim.sign_beneficiary(MessageSigner::new(&rotated_beneficiary, network_id))?;
    Ok((
        ProvisionalServiceReceiptV1::new(claim, provider_attestation, beneficiary_attestation)?,
        provider,
        beneficiary,
    ))
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
    let wrong = SessionSk::new_with_seckey(&SecretKey::random())?;
    assert!(matches!(
        receipt
            .claim
            .sign_beneficiary(MessageSigner::new(&wrong, 7)),
        Err(Error::ServiceReceipt(
            ServiceReceiptError::SignerRoleMismatch { .. }
        ))
    ));

    let swapped = ProvisionalServiceReceiptV1::new(
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
fn live_admission_checks_proof_liveness_epoch_and_time_overflow() -> Result<()> {
    let (receipt, _, _) = signed_receipt(7)?;
    let observed_at = crate::utils::get_epoch_ms();
    receipt.verify_live_at(7, observed_at)?;

    let mut stale = receipt.clone();
    stale.claim.epoch.slot = stale.claim.epoch.slot.saturating_sub(3);
    assert!(stale.verify_live_at(7, observed_at).is_err());
    assert_eq!(
        receipt.verify_live_at(7, u128::MAX),
        Err(ServiceReceiptError::ObservationTimeOverflow)
    );
    Ok(())
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn canonical_claim_golden_vector_is_stable() -> Result<()> {
    let claim = ProvisionalServiceClaimV1::probe(
        7,
        Did::from(1_u32),
        Did::from(2_u32),
        ProvisionalEpochV1 { slot: 3 },
        [4; 32],
        [5; 32],
        [6; 32],
    );
    let bytes = claim.canonical_bytes()?;
    assert_eq!(
        hex::encode(&bytes),
        "52494e47532d50524f564953494f4e414c2d534552564943452d434c41494d2d56310007002a3078303030303030303030303030303030303030303030303030303030303030303030303030303030312a3078303030303030303030303030303030303030303030303030303030303030303030303030303030320304040404040404040404040404040404040404040404040404040404040404040105050505050505050505050505050505050505050505050505050505050505050606060606060606060606060606060606060606060606060606060606060606"
    );
    assert_eq!(
        hex::encode(claim.digest()?.into_bytes()),
        "72698f171a530ebd5fbb21edc4fffc8206cb96e1c01d3d2700ef431d85dd113a"
    );
    assert_eq!(
        ProvisionalServiceClaimV1::from_canonical_bytes(&bytes)?,
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
    epoch: ProvisionalEpochV1,
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
        epoch: ProvisionalEpochV1 { slot: 3 },
        nonce: [4; 32],
        units,
        request_digest: [5; 32],
        completion_digest: [6; 32],
    })
}

#[test]
fn zero_units_and_unknown_service_kind_fail_decoding() -> Result<()> {
    assert!(ProvisionalServiceClaimV1::from_canonical_bytes(&raw_claim(0, 0)?).is_err());
    assert!(ProvisionalServiceClaimV1::from_canonical_bytes(&raw_claim(1, 1)?).is_err());
    Ok(())
}

#[test]
fn probe_offer_verifies_the_exact_signed_request_and_completion() -> Result<()> {
    let network_id = 7;
    let provider = SessionSk::new_with_seckey(&SecretKey::random())?;
    let beneficiary = SessionSk::new_with_seckey(&SecretKey::random())?;
    let provider_signer = MessageSigner::new(&provider, network_id);
    let beneficiary_signer = MessageSigner::new(&beneficiary, network_id);
    let provider_did = provider.account_did();
    let beneficiary_did = beneficiary.account_did();
    let request = ProbeRequestV1 {
        epoch: ProvisionalEpochV1::from_unix_seconds(
            u64::try_from(crate::utils::get_epoch_ms() / 1_000).unwrap_or(0),
        ),
        nonce: [8; 32],
    };
    let tx_id = uuid::Uuid::new_v4();
    let request_transaction = Transaction::new(
        provider_did,
        tx_id,
        1,
        Message::ProbeRequestV1(request),
        beneficiary_signer,
    )?;
    let request_digest = request_transaction.digest()?.into_bytes();
    let completion = Transaction::new(
        beneficiary_did,
        tx_id,
        1,
        ProbeCompletionV1 {
            request_digest,
            nonce: request.nonce,
        },
        provider_signer,
    )?;
    let claim = ProvisionalServiceClaimV1::probe(
        network_id,
        provider_did,
        beneficiary_did,
        request.epoch,
        request.nonce,
        request_digest,
        completion.digest()?.into_bytes(),
    );
    let offer = ProbeOfferV1 {
        request: request_transaction,
        completion,
        provider_attestation: claim.sign_provider(provider_signer)?,
        claim,
    };
    let outer_transaction = Transaction::new(
        beneficiary_did,
        tx_id,
        2,
        Message::ProbeOfferV1(Box::new(offer.clone())),
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
