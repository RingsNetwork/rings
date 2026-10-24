//! The ownership law (#1039): one open store per store directory, enforced at open, per store
//! and not per root. Each test drives the real file system under a fresh root.

use super::lock::LOCK_FILE_NAME;
use super::test_root::TempRoot;
use super::FileStorage;
use crate::error::Error;
use crate::storage::KvStorageInterface;
use crate::storage::KvStorageScan;
use crate::storage::RecordAuthority;

/// Every authority a store opens with.
const AUTHORITIES: [RecordAuthority; 4] = [
    RecordAuthority::Disposable,
    RecordAuthority::DurableCache,
    RecordAuthority::Retained,
    RecordAuthority::Authoritative,
];

/// Open the store at `root` with `authority`.
async fn open(
    root: &std::path::Path,
    authority: RecordAuthority,
) -> crate::error::Result<FileStorage> {
    FileStorage::new_with_cap_path_and_authority(4096, root, authority).await
}

/// A second open of an open store fails at open, naming the directory, for every authority; once
/// the first store drops, the directory opens again with its records.
#[tokio::test]
async fn test_a_second_open_of_an_open_store_fails_at_open() {
    for authority in AUTHORITIES {
        let root = TempRoot::new("file-lock-second");
        let first = open(&root, authority).await.expect("first open");
        first.put("a", &"v".to_owned()).await.expect("put");

        assert!(matches!(
            open(&root, authority).await,
            Err(Error::StorageDirectoryLocked(path)) if path == *root
        ));

        drop(first);
        let reopened = open(&root, authority).await.expect("reopen after drop");
        assert_eq!(
            reopened.get("a").await.expect("get"),
            Some("v".to_owned()),
            "{authority:?}"
        );
    }
}

/// The lock is per store: stores in sibling directories of one root, the node layout, open
/// together.
#[tokio::test]
async fn test_two_stores_under_one_root_open_together() {
    let root = TempRoot::new("file-lock-siblings");
    let data = open(&root.join("data"), RecordAuthority::Disposable)
        .await
        .expect("data store");
    let replay = open(
        &root.join("transaction-replay"),
        RecordAuthority::Authoritative,
    )
    .await
    .expect("replay store");
    data.put("a", &"v".to_owned()).await.expect("put data");
    replay.put("a", &"v".to_owned()).await.expect("put replay");
}

/// The lock file is no record: an open store with it counts and scans nothing, and `clear`
/// keeps it.
#[tokio::test]
async fn test_the_lock_file_is_no_record() {
    let root = TempRoot::new("file-lock-record");
    let storage = open(&root, RecordAuthority::Authoritative)
        .await
        .expect("open");
    assert!(root.join(LOCK_FILE_NAME).exists());
    let count = <FileStorage as KvStorageInterface<String>>::count(&storage)
        .await
        .expect("count");
    assert_eq!(count, 0);
    let scanned = <FileStorage as KvStorageScan<String>>::scan(&storage)
        .await
        .expect("scan");
    assert!(scanned.is_empty());

    storage.put("a", &"v".to_owned()).await.expect("put");
    <FileStorage as KvStorageInterface<String>>::clear(&storage)
        .await
        .expect("clear");
    assert!(root.join(LOCK_FILE_NAME).exists());
}
