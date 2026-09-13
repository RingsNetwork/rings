use std::num::NonZeroUsize;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::measure::Measure;
use rings_core::message::MessageSigner;
use rings_core::message::ProvisionalEpochV1;
use rings_core::message::ProvisionalServiceClaimV1;
use rings_core::message::ProvisionalServiceReceiptV1;
use rings_core::session::SessionSk;
use rings_core::storage::idb::IdbStorage;
use rings_measure::EvidenceAccountPair;
use rings_measure::EvidenceDigest;
use rings_measure::EvidenceFreshnessKey;
use rings_measure::ProvisionalEvidenceRecord;
use rings_measure::UnixTime;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::measure::EvidenceCollectorIdentity;
use crate::measure::EvidenceStorage;
use crate::measure::MeasureStorage;
use crate::measure::PeriodicMeasure;

fn evidence_fixture() -> (ProvisionalEvidenceRecord<Did>, EvidenceCollectorIdentity) {
    let provider = SessionSk::new_with_seckey(&SecretKey::random()).unwrap();
    let beneficiary = SessionSk::new_with_seckey(&SecretKey::random()).unwrap();
    let observed_at_seconds = u64::try_from(rings_core::utils::get_epoch_ms() / 1_000).unwrap();
    let claim = ProvisionalServiceClaimV1::probe(
        9,
        provider.account_did(),
        beneficiary.account_did(),
        ProvisionalEpochV1::from_unix_seconds(observed_at_seconds),
        [5; 32],
        [6; 32],
        [7; 32],
    );
    let provider_attestation = claim
        .sign_provider(MessageSigner::new(&provider, 9))
        .unwrap();
    let beneficiary_attestation = claim
        .sign_beneficiary(MessageSigner::new(&beneficiary, 9))
        .unwrap();
    let receipt = ProvisionalServiceReceiptV1::new(
        claim.clone(),
        provider_attestation,
        beneficiary_attestation,
    )
    .unwrap();
    let record = ProvisionalEvidenceRecord::new(
        EvidenceAccountPair::new(claim.provider_account, claim.beneficiary_account),
        EvidenceFreshnessKey::new(
            claim.network_id,
            claim.beneficiary_account,
            claim.epoch.slot,
            claim.nonce,
        ),
        EvidenceDigest::new(receipt.digest().unwrap().into_bytes()),
        receipt.canonical_bytes().unwrap(),
        UnixTime::from_secs(observed_at_seconds),
        claim.epoch.slot.saturating_sub(1),
    );
    let collector = EvidenceCollectorIdentity::new(claim.network_id, claim.provider_account);
    (record, collector)
}

async fn storages(name: &str) -> (MeasureStorage, EvidenceStorage) {
    let measure_name = format!("{name}/measure");
    let evidence_name = format!("{name}/evidence");
    let measure = IdbStorage::new_with_cap_and_name(2, &measure_name)
        .await
        .unwrap();
    let evidence = IdbStorage::new_with_cap_and_name(2, &evidence_name)
        .await
        .unwrap();
    (
        Box::new(measure) as MeasureStorage,
        Box::new(evidence) as EvidenceStorage,
    )
}

#[wasm_bindgen_test]
async fn successful_admission_survives_indexed_db_restart_without_flush() {
    let name = format!("rings-evidence-test-{}", uuid::Uuid::new_v4());
    let (measure_storage, evidence_storage) = storages(&name).await;
    measure_storage.clear().await.unwrap();
    evidence_storage.clear().await.unwrap();
    let (record, collector) = evidence_fixture();
    let measure =
        PeriodicMeasure::new_with_evidence_storage(measure_storage, evidence_storage, collector)
            .await
            .unwrap();
    measure.admit_provisional_evidence(record).await.unwrap();
    drop(measure);

    let (measure_storage, evidence_storage) = storages(&name).await;
    let restored =
        PeriodicMeasure::new_with_evidence_storage(measure_storage, evidence_storage, collector)
            .await
            .unwrap();
    let page = restored
        .provisional_evidence_page(None, NonZeroUsize::new(4).unwrap())
        .await
        .unwrap();
    assert_eq!(page.records().len(), 1);
}
