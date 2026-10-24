//! The presence law (#915): an authoritative store marks every record it made durable, reports
//! a marked record that is gone as missing, and repairs an unmarked record at open; a disposable
//! store keeps no markers. Each test drives the real file system under a fresh root.

use super::presence::marker_name;
use super::test_root::TempRoot;
use super::*;

/// A fresh store root for one test, removed on drop.
fn temp_root(label: &str) -> TempRoot {
    TempRoot::new(&format!("file-presence-{label}"))
}

/// Open the authoritative store at `root`.
async fn open_authoritative(root: &Path) -> FileStorage {
    FileStorage::new_with_cap_path_and_authority(4096, root, RecordAuthority::Authoritative)
        .await
        .expect("open")
}

/// The scan of `storage`, records as `(name, kind)` pairs sorted by name.
async fn scanned(storage: &FileStorage) -> Vec<(String, &'static str)> {
    let mut records = <FileStorage as KvStorageScan<String>>::scan(storage)
        .await
        .expect("scan")
        .into_iter()
        .map(|record| match record {
            ScannedRecord::Filed { key, .. } => (file_name_for(&key), "filed"),
            ScannedRecord::Undecodable(UndecodableRecord { name }) => (name, "undecodable"),
            ScannedRecord::Missing(UndecodableRecord { name }) => (name, "missing"),
        })
        .collect::<Vec<_>>();
    records.sort();
    records
}

/// Whether the presence marker of `key` exists under `root`.
fn marked(root: &Path, key: &str) -> bool {
    root.join(marker_name(&file_name_for(key))).exists()
}

/// A put marks its record; a marker is no record: it is not counted, and a removal takes both.
#[tokio::test]
async fn test_an_authoritative_put_marks_its_record_and_a_removal_unmarks_it() {
    let root = temp_root("marks");
    let storage = open_authoritative(&root).await;
    storage.put("a", &"v".to_owned()).await.expect("put");
    assert!(marked(&root, "a"));
    let count = <FileStorage as KvStorageInterface<String>>::count(&storage)
        .await
        .expect("count");
    assert_eq!(count, 1);

    <FileStorage as KvStorageInterface<String>>::remove(&storage, "a")
        .await
        .expect("remove");
    assert!(!marked(&root, "a"));
    assert!(!root.join(file_name_for("a")).exists());
    assert!(scanned(&storage).await.is_empty());
}

/// A record removed outside the store, while it is closed, is missing after the reopen: the
/// scan names it, and `get` refuses it instead of answering a key never written.
#[tokio::test]
async fn test_a_record_removed_outside_the_store_is_missing() {
    let root = temp_root("removed");
    let name = file_name_for("a");
    {
        let storage = open_authoritative(&root).await;
        storage.put("a", &"v".to_owned()).await.expect("put a");
        storage.put("b", &"v".to_owned()).await.expect("put b");
    }
    std::fs::remove_file(root.join(&name)).expect("remove the record outside the store");

    let reopened = open_authoritative(&root).await;
    let mut expected = vec![(name.clone(), "missing"), (file_name_for("b"), "filed")];
    expected.sort();
    assert_eq!(scanned(&reopened).await, expected);
    assert!(matches!(
        <FileStorage as KvStorageInterface<String>>::get(&reopened, "a").await,
        Err(Error::StorageRecordMissing(UndecodableRecord { name: missing })) if missing == name
    ));
    assert!(marked(&root, "a"), "a missing record's marker is kept");
}

/// A record file renamed to upper case is not a record name, so it is never indexed; its marker
/// keeps it in the scan (the presence law): missing on a case-sensitive volume, and filed on a
/// case-insensitive one, which still opens the file under its own name. Never absent.
#[tokio::test]
async fn test_an_upper_cased_record_is_never_absent_from_the_scan() {
    let root = temp_root("upper");
    let name = file_name_for("a");
    {
        let storage = open_authoritative(&root).await;
        storage.put("a", &"v".to_owned()).await.expect("put");
    }
    std::fs::rename(root.join(&name), root.join(name.to_uppercase())).expect("rename");
    let case_insensitive = root.join(&name).exists();

    let reopened = open_authoritative(&root).await;
    let expected = if case_insensitive { "filed" } else { "missing" };
    assert_eq!(scanned(&reopened).await, [(name, expected)]);
}

/// A store whose root vanishes while it runs answers every marked name with an error, never
/// with an absent value (the read agreement law).
#[tokio::test]
async fn test_a_vanished_root_makes_every_marked_record_missing() {
    let root = temp_root("vanished");
    let storage = open_authoritative(&root).await;
    storage.put("a", &"v".to_owned()).await.expect("put");
    std::fs::remove_dir_all(&root).expect("remove the root outside the store");

    assert!(matches!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "a").await,
        Err(Error::StorageRecordMissing(_))
    ));
    assert_eq!(scanned(&storage).await, [(file_name_for("a"), "missing")]);
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "never")
            .await
            .expect("an unmarked key reads"),
        None
    );
}

/// An open marks a record that has no marker (a crash between its rename and its marker, or a
/// store written before markers), so it is filed, and missing once removed.
#[tokio::test]
async fn test_an_open_marks_an_unmarked_record() {
    let root = temp_root("repair");
    {
        let storage = open_authoritative(&root).await;
        storage.put("a", &"v".to_owned()).await.expect("put");
    }
    std::fs::remove_file(root.join(marker_name(&file_name_for("a")))).expect("drop marker");

    let reopened = open_authoritative(&root).await;
    assert!(marked(&root, "a"));
    assert_eq!(scanned(&reopened).await, [(file_name_for("a"), "filed")]);
}

/// `clear` removes every record and every marker, a missing record's included; only the
/// store's lock file is left (`lock`).
#[tokio::test]
async fn test_clear_removes_the_markers_of_missing_records() {
    let root = temp_root("clear");
    {
        let storage = open_authoritative(&root).await;
        storage.put("a", &"v".to_owned()).await.expect("put a");
        storage.put("b", &"v".to_owned()).await.expect("put b");
    }
    std::fs::remove_file(root.join(file_name_for("a"))).expect("remove a outside the store");

    let reopened = open_authoritative(&root).await;
    <FileStorage as KvStorageInterface<String>>::clear(&reopened)
        .await
        .expect("clear");
    assert!(scanned(&reopened).await.is_empty());
    let left = std::fs::read_dir(&root)
        .expect("list")
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .expect("entry");
    assert_eq!(left, [super::lock::LOCK_FILE_NAME]);
}

/// A disposable store keeps no markers: a removed record is simply absent.
#[tokio::test]
async fn test_a_disposable_store_keeps_no_markers() {
    let root = temp_root("disposable");
    {
        let storage = FileStorage::new_with_cap_and_path(4096, &root)
            .await
            .expect("open");
        storage.put("a", &"v".to_owned()).await.expect("put");
        assert!(!marked(&root, "a"));
    }
    std::fs::remove_file(root.join(file_name_for("a"))).expect("remove");
    let reopened = FileStorage::new_with_cap_and_path(4096, &root)
        .await
        .expect("reopen");
    assert!(scanned(&reopened).await.is_empty());
}
