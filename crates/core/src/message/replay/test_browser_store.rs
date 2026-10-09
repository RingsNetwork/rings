//! Fail closed per stream in the browser (IndexedDB) replay store (#910).

use super::ReplayRecord;
use super::SequenceVerdict;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Error;
use crate::message::MessageCategory;
use crate::storage::KvStorageInterface;

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
    let open = || crate::storage::idb::IdbStorage::new_with_cap_and_name(4, STORAGE_NAME);
    let storage = open().await.expect("IndexedDB opens");
    <crate::storage::idb::IdbStorage as KvStorageInterface<ReplayRecord>>::clear(&storage)
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
    <crate::storage::idb::IdbStorage as KvStorageInterface<ReplayRecord>>::remove(
        &reopened,
        corrupt_key.as_str(),
    )
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
