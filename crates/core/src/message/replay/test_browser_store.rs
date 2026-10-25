//! Fail closed per stream in the browser (IndexedDB) replay store (#910).

use super::ReplayRecord;
use super::SequenceVerdict;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Error;
use crate::message::MessageCategory;
use crate::storage::idb::IdbStorage;
use crate::storage::KvStorageInterface;
use crate::storage::RecordAuthority::Authoritative;

/// The digest of the transaction a test admits, one per `value`.
fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

/// Fail closed per stream in the browser store: a row under stream 1's receiver record key
/// that does not decode as a record refuses that stream alone, is kept, and removing it
/// before a reopen restores the stream while the others keep their windows.
#[wasm_bindgen_test::wasm_bindgen_test]
async fn test_browser_store_fails_closed_only_on_the_stream_of_an_undecodable_row() {
    /// The IndexedDB database this test owns.
    const STORAGE_NAME: &str = "rings-core/replay-store-undecodable-row";
    // Authoritative, as the browser provider opens its replay store.
    let open = || IdbStorage::new_with_cap_name_and_authority(4, STORAGE_NAME, Authoritative);
    let storage = open().await.expect("IndexedDB opens");
    <IdbStorage as KvStorageInterface<ReplayRecord>>::clear(&storage)
        .await
        .expect("IndexedDB clears");
    let corrupt = StreamKey::new(7, Did::from(1_u32), Did::from(99_u32), MessageCategory::E2e);
    let intact = StreamKey::new(7, Did::from(2_u32), Did::from(99_u32), MessageCategory::E2e);
    let corrupt_key = super::store::record_key(super::store::ReplayTable::Receiver, &corrupt)
        .expect("record key encodes");
    storage
        .put(corrupt_key.as_str(), &42_u32)
        .await
        .expect("foreign row stores");
    let replay = TransactionReplay::new_shared(Box::new(storage));

    assert!(matches!(
        replay.admit(corrupt, 0, digest(1)).await,
        Err(Error::TransactionReplayStreamUnavailable { ref record, .. })
            if *record == corrupt_key
    ));
    assert_eq!(
        replay
            .admit(intact, 0, digest(1))
            .await
            .expect("the intact stream admits"),
        SequenceVerdict::First
    );
    assert_eq!(replay.counters().unrestorable_record, 1);
    drop(replay);

    let reopened = open().await.expect("IndexedDB reopens");
    <IdbStorage as KvStorageInterface<ReplayRecord>>::remove(&reopened, corrupt_key.as_str())
        .await
        .expect("the operator removes the row");
    let restarted = TransactionReplay::new_shared(Box::new(reopened));
    assert_eq!(
        restarted
            .admit(corrupt, 0, digest(1))
            .await
            .expect("the cleared stream admits"),
        SequenceVerdict::First
    );
    assert!(matches!(
        restarted.admit(intact, 0, digest(1)).await,
        Err(Error::TransactionReplay { .. })
    ));
}

/// Group commit in the browser (#916): admissions of many streams issued at once all commit
/// through the strict IndexedDB store, sharing batches, and every one survives a reopen.
#[wasm_bindgen_test::wasm_bindgen_test]
async fn test_concurrent_browser_admissions_share_batches_and_survive_a_reopen() {
    /// The IndexedDB database this test owns.
    const STORAGE_NAME: &str = "rings-core/replay-store-group-commit";
    /// The admissions issued at once.
    const ADMISSIONS: u32 = 32;
    let open = || IdbStorage::new_with_cap_name_and_authority(64, STORAGE_NAME, Authoritative);
    let storage = open().await.expect("IndexedDB opens");
    <IdbStorage as KvStorageInterface<ReplayRecord>>::clear(&storage)
        .await
        .expect("IndexedDB clears");
    let stream = |origin: u32| {
        StreamKey::new(
            7,
            Did::from(origin),
            Did::from(99_u32),
            MessageCategory::E2e,
        )
    };
    let replay = TransactionReplay::new_shared(Box::new(storage));
    let verdicts = futures::future::join_all(
        (0..ADMISSIONS).map(|origin| replay.admit(stream(origin), 0, digest(1))),
    )
    .await;
    assert!(verdicts
        .iter()
        .all(|verdict| matches!(verdict, Ok(SequenceVerdict::First))));
    drop(replay);

    let restarted = TransactionReplay::new_shared(Box::new(open().await.expect("reopens")));
    for origin in 0..ADMISSIONS {
        assert!(matches!(
            restarted.admit(stream(origin), 0, digest(1)).await,
            Err(Error::TransactionReplay { .. })
        ));
    }
}
