//! The batch of the file store (#916): every record of a batch is stored, marked present and
//! read back after a restart; a later pair of one key supersedes an earlier one; a refused
//! budget changes nothing.

use super::test_root::TempRoot;
use super::*;

/// The `(key, value)` records of a test batch: `count` keys `k0, k1, …`, each holding `value`.
fn records(count: usize, value: &str) -> Vec<(String, String)> {
    (0..count)
        .map(|index| (format!("k{index}"), value.to_owned()))
        .collect()
}

/// Open the authoritative store rooted at `root` under a budget of `capacity` bytes.
async fn authoritative(root: &TempRoot, capacity: u32) -> FileStorage {
    FileStorage::new_with_cap_path_and_authority(capacity, root, RecordAuthority::Authoritative)
        .await
        .expect("store opens")
}

/// Every record of a store, by key, sorted.
async fn stored(storage: &FileStorage) -> Vec<(String, String)> {
    let mut all = <FileStorage as KvStorageInterface<String>>::get_all(storage)
        .await
        .expect("records read");
    all.sort();
    all
}

/// A batch stores every record, marks each present, leaves no temporary file, and a restart
/// reads every record back: the batch law on an authoritative store.
#[tokio::test]
async fn test_an_authoritative_batch_stores_and_marks_every_record() {
    let root = TempRoot::new("batch-stores");
    let batch = records(16, "v");
    {
        let storage = authoritative(&root, 1 << 16).await;
        storage.put_batch(&batch).await.expect("batch stores");
    }
    let storage = authoritative(&root, 1 << 16).await;
    let mut expected = batch.clone();
    expected.sort();
    assert_eq!(stored(&storage).await, expected);
    for (key, _) in &batch {
        let name = file_name_for(key);
        assert!(root.join(presence::marker_name(&name)).exists(), "{key}");
        assert!(!root.join(&name).with_extension("tmp").exists(), "{key}");
    }
}

/// A later pair of one key supersedes an earlier one in the same batch, and a batch over
/// stored records replaces them.
#[tokio::test]
async fn test_a_later_pair_of_one_key_supersedes_an_earlier_one() {
    let root = TempRoot::new("batch-supersedes");
    let storage = authoritative(&root, 1 << 16).await;
    storage
        .put_batch(&records(2, "old"))
        .await
        .expect("first batch stores");
    let rewrite = [
        ("k0".to_owned(), "first".to_owned()),
        ("k0".to_owned(), "last".to_owned()),
        ("k1".to_owned(), "new".to_owned()),
    ];
    storage
        .put_batch(&rewrite)
        .await
        .expect("second batch stores");
    assert_eq!(stored(&storage).await, [
        ("k0".to_owned(), "last".to_owned()),
        ("k1".to_owned(), "new".to_owned()),
    ]);
}

/// The budget law of a store that never evicts: a batch that does not fit is refused whole,
/// before its first write, and changes nothing; one that fits beside the records it replaces
/// is stored.
#[tokio::test]
async fn test_an_authoritative_batch_over_the_budget_changes_nothing() {
    let root = TempRoot::new("batch-budget");
    let one = u32::try_from(
        rings_codec::serialize(&("k0", "v"))
            .expect("record encodes")
            .len(),
    )
    .expect("small record");
    let storage = authoritative(&root, 2 * one).await;
    storage
        .put_batch(&records(2, "v"))
        .await
        .expect("two records fit");
    assert!(matches!(
        storage.put_batch(&records(3, "v")).await,
        Err(Error::StorageBudgetExhausted { .. })
    ));
    assert_eq!(stored(&storage).await, records(2, "v"));
    assert!(!root.join(file_name_for("k2")).exists());
    assert!(!root
        .join(file_name_for("k2"))
        .with_extension("tmp")
        .exists());
    storage
        .put_batch(&records(2, "w"))
        .await
        .expect("a rewrite fits beside nothing it does not replace");
    assert_eq!(stored(&storage).await, records(2, "w"));
}

/// The batch removal law on an authoritative store: every record of the batch and its presence
/// marker are gone, the others stay, a key that holds no record is skipped, and after a restart
/// a scan reports no record missing.
#[tokio::test]
async fn test_a_batch_removal_removes_every_record_and_its_marker() {
    let root = TempRoot::new("batch-removes");
    {
        let storage = authoritative(&root, 1 << 16).await;
        storage
            .put_batch(&records(4, "v"))
            .await
            .expect("batch stores");
        let removed = ["k0".to_owned(), "k2".to_owned(), "absent".to_owned()];
        <FileStorage as KvStorageScan<String>>::remove_batch(&storage, &removed)
            .await
            .expect("batch removes");
        for key in ["k0", "k2"] {
            let name = file_name_for(key);
            assert!(!root.join(&name).exists(), "{key}");
            assert!(!root.join(presence::marker_name(&name)).exists(), "{key}");
        }
    }
    let storage = authoritative(&root, 1 << 16).await;
    assert_eq!(stored(&storage).await, [
        ("k1".to_owned(), "v".to_owned()),
        ("k3".to_owned(), "v".to_owned()),
    ]);
    let scanned = <FileStorage as KvStorageScan<String>>::scan(&storage)
        .await
        .expect("store scans");
    assert!(scanned
        .iter()
        .all(|record| matches!(record, ScannedRecord::Filed { .. })));
}

/// Regression (#1209): a replacement batch that grows some records and shrinks others while
/// fitting in the aggregate budget succeeds regardless of key/filename order across all
/// non-evicting authorities, and an oversized replacement batch changes nothing.
#[tokio::test]
async fn test_fitting_replacement_batch_with_mixed_growth_and_shrinkage_succeeds() {
    let encoded_bytes = |rows: &[(String, String)]| -> u32 {
        rows.iter()
            .map(|row| rings_codec::serialize(row).expect("record encodes").len() as u32)
            .sum()
    };
    let initial = vec![
        ("c".to_owned(), "x".repeat(10)),
        ("a".to_owned(), "x".repeat(10)),
        ("b".to_owned(), "x".repeat(150)),
    ];
    let replacement = vec![
        ("c".to_owned(), "y".repeat(15)),
        ("a".to_owned(), "y".repeat(100)),
        ("b".to_owned(), "y".repeat(10)),
    ];
    let capacity = encoded_bytes(&initial) + 10;
    assert!(encoded_bytes(&replacement) <= capacity);

    for authority in [RecordAuthority::Authoritative, RecordAuthority::Retained] {
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [
            2, 1, 0,
        ]] {
            let root = TempRoot::new("batch-grow-shrink");
            let storage = FileStorage::new_with_cap_path_and_authority(capacity, &root, authority)
                .await
                .expect("store opens");
            storage
                .put_batch(&initial)
                .await
                .expect("initial batch fits");
            let ordered = order
                .into_iter()
                .map(|index| replacement[index].clone())
                .collect::<Vec<_>>();
            storage
                .put_batch(&ordered)
                .await
                .expect("fitting replacement batch must not fail intermediate checks");
            let mut expected = replacement.clone();
            expected.sort();
            assert_eq!(stored(&storage).await, expected);

            let oversized = vec![
                ("c".to_owned(), "z".repeat(10)),
                ("a".to_owned(), "z".repeat(100)),
                ("b".to_owned(), "z".repeat(100)),
            ];
            assert!(matches!(
                storage.put_batch(&oversized).await,
                Err(Error::StorageBudgetExhausted { .. })
            ));
            assert_eq!(stored(&storage).await, expected);
        }
    }
}

/// Regression (#1209): a two-record grow/shrink exchange under an exact full budget succeeds.
#[tokio::test]
async fn test_two_record_grow_shrink_exchange_at_exact_budget() {
    let initial = [
        ("k0".to_owned(), "x".repeat(10)),
        ("k1".to_owned(), "x".repeat(80)),
    ];
    let swapped = [
        ("k0".to_owned(), "y".repeat(80)),
        ("k1".to_owned(), "y".repeat(10)),
    ];
    let capacity = initial
        .iter()
        .map(|row| rings_codec::serialize(row).expect("record encodes").len() as u32)
        .sum::<u32>();

    for authority in [
        RecordAuthority::Disposable,
        RecordAuthority::DurableCache,
        RecordAuthority::Retained,
        RecordAuthority::Authoritative,
    ] {
        let root = TempRoot::new("batch-two-swap");
        let storage = FileStorage::new_with_cap_path_and_authority(capacity, &root, authority)
            .await
            .expect("store opens");
        storage.put_batch(&initial).await.expect("initial stores");
        storage.put_batch(&swapped).await.expect("swap stores");
        assert_eq!(stored(&storage).await, swapped);
    }
}

/// Regression (#1225): a disposable batch preserves caller last-write recency rather than SHA-1
/// filename order, never evicts an earlier record of the same batch, and rejects a deduplicated
/// batch whose total byte size exceeds the capacity without mutating existing records.
#[tokio::test]
async fn test_disposable_batch_preserves_caller_order_and_never_evicts_same_batch() {
    let one = u32::try_from(
        rings_codec::serialize(&("k0", "v"))
            .expect("record encodes")
            .len(),
    )
    .expect("small record");

    for (first, second) in [("k0", "k1"), ("k1", "k0")] {
        let root = TempRoot::new("batch-disposable-order");
        let storage = FileStorage::new_with_cap_and_path(2 * one, &root)
            .await
            .expect("store opens");
        storage
            .put("old", &"v".to_owned())
            .await
            .expect("seed record stores");
        storage
            .put_batch(&[
                (second.to_owned(), "stale".to_owned()),
                (first.to_owned(), "v".to_owned()),
                (second.to_owned(), "v".to_owned()),
            ])
            .await
            .expect("batch stores both records after retiring seed");

        let mut expected = vec![
            (first.to_owned(), "v".to_owned()),
            (second.to_owned(), "v".to_owned()),
        ];
        expected.sort();
        assert_eq!(stored(&storage).await, expected);

        assert!(matches!(
            storage.put_batch(&records(3, "v")).await,
            Err(Error::StorageValueExceedsCapacity { .. })
        ));
        assert_eq!(stored(&storage).await, expected);

        storage
            .put("k2", &"v".to_owned())
            .await
            .expect("next single put retires the earliest key of the batch");
        let mut after_evict = vec![
            (second.to_owned(), "v".to_owned()),
            ("k2".to_owned(), "v".to_owned()),
        ];
        after_evict.sort();
        assert_eq!(stored(&storage).await, after_evict);
    }
}

/// Law (batch): an earlier oversized value for a key superseded by a later fitting value for the
/// same key in one batch succeeds.
#[tokio::test]
async fn test_superseded_oversized_value_for_same_key_in_batch_succeeds() {
    let root = TempRoot::new("batch-superseded-oversized");
    let one = u32::try_from(
        rings_codec::serialize(&("k", "v"))
            .expect("record encodes")
            .len(),
    )
    .expect("small record");
    let storage = authoritative(&root, one).await;
    let batch = [
        ("k".to_owned(), "x".repeat(256)),
        ("k".to_owned(), "v".to_owned()),
    ];
    storage
        .put_batch(&batch)
        .await
        .expect("later fitting value supersedes earlier oversized value");
    assert_eq!(stored(&storage).await, [("k".to_owned(), "v".to_owned())]);
}
