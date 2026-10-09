use serde::Deserialize;
use serde::Serialize;

use super::test_root::TempRoot;
use super::*;

#[derive(Debug, Serialize, Deserialize)]
struct TestStorageStruct {
    content: String,
}

/// Put, get, count, get_all, and clear agree on the stored records.
#[tokio::test]
async fn test_kv_storage_put_get_count_and_clear() {
    let root = temp_root("put-get");
    let storage = FileStorage::new_with_cap_and_path(4096, &root)
        .await
        .expect("store opens");
    let records = [
        ("test1".to_owned(), "first".to_owned()),
        ("test2".to_owned(), "second".to_owned()),
    ];
    for (key, content) in &records {
        storage
            .put(key, &TestStorageStruct {
                content: content.clone(),
            })
            .await
            .expect("record stores");
    }

    let count = <FileStorage as KvStorageInterface<TestStorageStruct>>::count(&storage)
        .await
        .expect("count reads");
    assert_eq!(count, 2);
    let first: TestStorageStruct = storage
        .get("test1")
        .await
        .expect("record reads")
        .expect("record present");
    assert_eq!(first.content, "first");
    let mut all: Vec<(String, String)> =
        <FileStorage as KvStorageInterface<TestStorageStruct>>::get_all(&storage)
            .await
            .expect("records read")
            .into_iter()
            .map(|(key, value)| (key, value.content))
            .collect();
    all.sort();
    assert_eq!(all, records);

    <FileStorage as KvStorageInterface<TestStorageStruct>>::clear(&storage)
        .await
        .expect("store clears");
    let count = <FileStorage as KvStorageInterface<TestStorageStruct>>::count(&storage)
        .await
        .expect("count reads");
    assert_eq!(count, 0);
    drop(storage);
}

/// A fresh store root for one test, removed on drop.
fn temp_root(label: &str) -> TempRoot {
    TempRoot::new(&format!("file-kv-{label}"))
}

fn record_len(key: &str, value: &str) -> u32 {
    rings_codec::serialize(&(key, value))
        .expect("record serializes")
        .len() as u32
}

async fn stored_keys(storage: &FileStorage) -> Vec<String> {
    let mut keys = <FileStorage as KvStorageInterface<String>>::get_all(storage)
        .await
        .expect("get_all")
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

/// Budget law: a new key beyond the byte budget retires the least recently written keys until
/// it fits, and a rewrite of a stored key does not compete with itself.
#[tokio::test]
async fn test_put_beyond_budget_retires_least_recently_written_keys() {
    let root = temp_root("budget");
    let one = record_len("a", "v");
    let storage = FileStorage::new_with_cap_and_path(one * 2, &root)
        .await
        .expect("open");

    storage.put("a", &"v".to_string()).await.expect("put a");
    storage.put("b", &"v".to_string()).await.expect("put b");
    storage.put("a", &"w".to_string()).await.expect("rewrite a");
    assert_eq!(stored_keys(&storage).await, ["a", "b"]);

    storage.put("c", &"v".to_string()).await.expect("put c");
    assert_eq!(stored_keys(&storage).await, ["a", "c"]);
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "b")
            .await
            .expect("get b"),
        None
    );
}

/// Budget law: a value larger than the whole budget is rejected and nothing is retired.
#[tokio::test]
async fn test_value_larger_than_budget_is_rejected_without_change() {
    let root = temp_root("oversize");
    let one = record_len("a", "v");
    let storage = FileStorage::new_with_cap_and_path(one, &root)
        .await
        .expect("open");
    storage.put("a", &"v".to_string()).await.expect("put a");

    let oversize = "x".repeat(one as usize);
    assert!(matches!(
        storage.put("b", &oversize).await,
        Err(Error::StorageValueExceedsCapacity { .. })
    ));
    assert_eq!(stored_keys(&storage).await, ["a"]);
}

/// Index law under a refused removal: a record the file system will not remove (its path is
/// occupied by a directory here) stays indexed and counted, the write that needed its bytes
/// fails without changing the store, and the same write succeeds once the file is back.
#[tokio::test]
async fn test_refused_retirement_keeps_the_record_indexed() {
    let root = temp_root("refused");
    let one = record_len("a", "v");
    let storage = FileStorage::new_with_cap_and_path(one, &root)
        .await
        .expect("open");
    storage.put("a", &"v".to_string()).await.expect("put a");
    let path = storage.store.root.join(file_name_for("a"));
    let record = std::fs::read(&path).expect("read record a");
    std::fs::remove_file(&path).expect("remove record a");
    std::fs::create_dir(&path).expect("occupy record a's path");

    assert!(matches!(
        storage.put("b", &"v".to_string()).await,
        Err(Error::ServiceIOError(_))
    ));
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::count(&storage)
            .await
            .expect("count"),
        1
    );
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "b")
            .await
            .expect("get b"),
        None
    );
    let path_b = storage.store.root.join(file_name_for("b"));
    assert!(!path_b.exists());
    assert!(!path_b.with_extension("tmp").exists());

    std::fs::remove_dir(&path).expect("release record a's path");
    std::fs::write(&path, record).expect("restore record a");
    storage.put("b", &"v".to_string()).await.expect("put b");
    assert_eq!(stored_keys(&storage).await, ["b"]);
}

/// Decode law: a record the current schema cannot read is reported absent and retired on the
/// read that discovers it, but only while the file still holds the bytes that read observed;
/// a record rewritten meanwhile stays.
#[tokio::test]
async fn test_undecodable_record_is_retired_only_while_unchanged() {
    let root = temp_root("undecodable");
    std::fs::create_dir_all(&root).expect("root");
    let name = file_name_for("a");
    let garbage = b"not a record".to_vec();
    std::fs::write(root.join(&name), &garbage).expect("write garbage");
    let storage = FileStorage::new_with_cap_and_path(4096, &root)
        .await
        .expect("open");
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::count(&storage)
            .await
            .expect("count"),
        1
    );

    // The bytes on disk are not the ones the read observed: the record is the writer's.
    storage
        .store
        .retire_observed(&name, b"what an earlier read saw")
        .expect("retire nothing");
    assert!(root.join(&name).exists());
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::count(&storage)
            .await
            .expect("count"),
        1
    );

    // A read that observes the garbage retires it.
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "a")
            .await
            .expect("get a"),
        None
    );
    assert!(!root.join(&name).exists());
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::count(&storage)
            .await
            .expect("count"),
        0
    );
}

/// Budget law across restarts: reopening rebuilds the index from the directory in modification
/// order and restores a lowered budget by retiring the oldest files.
#[tokio::test]
async fn test_reopen_restores_budget_in_write_order() {
    let root = temp_root("reopen");
    let one = record_len("a", "v");
    {
        let storage = FileStorage::new_with_cap_and_path(one * 3, &root)
            .await
            .expect("open");
        for (index, key) in ["a", "b", "c"].into_iter().enumerate() {
            storage.put(key, &"v".to_string()).await.expect("put");
            let modified = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000 + index as u64);
            std::fs::File::open(storage.store.root.join(file_name_for(key)))
                .expect("open file")
                .set_modified(modified)
                .expect("set modified");
        }
    }

    let reopened = FileStorage::new_with_cap_and_path(one * 2, &root)
        .await
        .expect("reopen");
    assert_eq!(stored_keys(&reopened).await, ["b", "c"]);
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::count(&reopened)
            .await
            .expect("count"),
        2
    );
}

/// Write `bytes` as the record file of `key` under `root`, as a crash would have left it.
fn plant_record(root: &std::path::Path, key: &str, bytes: &[u8]) -> String {
    std::fs::create_dir_all(root).expect("root");
    let name = file_name_for(key);
    std::fs::write(root.join(&name), bytes).expect("plant record");
    name
}

/// Decode law of an authoritative store: a torn record (its tail lost) is reported by `get`
/// and `get_all` with its file and its intact key, and never deleted.
#[tokio::test]
async fn test_authoritative_store_reports_a_torn_record_and_keeps_it() {
    let root = temp_root("torn");
    let whole = rings_codec::serialize(&("stream", "window")).expect("record serializes");
    let torn = whole.get(..whole.len() - 3).expect("torn prefix");
    let name = plant_record(&root, "stream", torn);
    let storage =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("open");
    let expected = UndecodableRecord {
        name: name.clone(),
        key: Some("stream".to_owned()),
    };

    assert!(matches!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "stream").await,
        Err(Error::StorageRecordUndecodable(ref record)) if *record == expected
    ));
    assert!(matches!(
        <FileStorage as KvStorageInterface<String>>::get_all(&storage).await,
        Err(Error::StorageRecordUndecodable(ref record)) if *record == expected
    ));
    assert_eq!(std::fs::read(root.join(&name)).expect("record kept"), torn);
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::count(&storage)
            .await
            .expect("count"),
        1
    );
}

/// Decode law of an authoritative store: a record truncated to nothing, which a crash between
/// an unflushed write and its rename leaves behind, is reported by its file alone (its key is
/// lost with its bytes) and never deleted, even across a reopen.
#[tokio::test]
async fn test_authoritative_store_reports_an_empty_record_by_its_file() {
    let root = temp_root("empty");
    let name = plant_record(&root, "stream", &[]);
    let expected = UndecodableRecord {
        name: name.clone(),
        key: None,
    };
    for _ in 0..2 {
        let storage = FileStorage::new_with_cap_path_and_authority(
            4096,
            &root,
            RecordAuthority::Authoritative,
        )
        .await
        .expect("open");
        assert!(matches!(
            <FileStorage as KvStorageInterface<String>>::get_all(&storage).await,
            Err(Error::StorageRecordUndecodable(ref record)) if *record == expected
        ));
        assert!(root.join(&name).exists());
    }
}

/// Damage is never taken for a move: a record that does not decode whole, whose key prefix
/// decodes but hashes to another file, is undecodable with no key, not misfiled.
#[tokio::test]
async fn test_a_foreign_key_prefix_is_not_a_misfiled_record() {
    let root = temp_root("misfiled");
    let other = rings_codec::serialize(&"other").expect("key serializes");
    let name = plant_record(&root, "stream", &other);
    let storage =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("open");

    assert!(matches!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "stream").await,
        Err(Error::StorageRecordUndecodable(UndecodableRecord {
            name: ref reported,
            key: None,
        }))
            if *reported == name
    ));
}

/// Durability law, observed through its effect: an authoritative store's writes, rewrites and
/// removals round-trip through a reopen and leave no temporary file behind.
#[tokio::test]
async fn test_authoritative_writes_round_trip_through_a_reopen() {
    let root = temp_root("authoritative");
    {
        let storage = FileStorage::new_with_cap_path_and_authority(
            4096,
            &root,
            RecordAuthority::Authoritative,
        )
        .await
        .expect("open");
        storage.put("a", &"v".to_string()).await.expect("put a");
        storage.put("b", &"v".to_string()).await.expect("put b");
        storage.put("a", &"w".to_string()).await.expect("rewrite a");
        <FileStorage as KvStorageInterface<String>>::remove(&storage, "b")
            .await
            .expect("remove b");
    }
    let reopened =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("reopen");
    assert_eq!(stored_keys(&reopened).await, ["a"]);
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::get(&reopened, "a")
            .await
            .expect("get a"),
        Some("w".to_owned())
    );
    let temporaries = std::fs::read_dir(&root)
        .expect("list root")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp"))
        .count();
    assert_eq!(temporaries, 0);
}

/// Scan law: a scan reports each record decoded or undecodable (named by its file and its intact
/// key) and deletes nothing, even in a disposable store whose reads would retire the record.
#[tokio::test]
async fn test_scan_reports_undecodable_records_and_deletes_nothing() {
    let root = temp_root("scan");
    let whole = rings_codec::serialize(&("torn", "window")).expect("record serializes");
    let name = plant_record(&root, "torn", whole.get(..whole.len() - 1).expect("prefix"));
    let storage = FileStorage::new_with_cap_and_path(4096, &root)
        .await
        .expect("open");
    storage.put("whole", &"v".to_string()).await.expect("put");

    let mut scanned = <FileStorage as KvStorageScan<String>>::scan(&storage)
        .await
        .expect("scan");
    scanned.sort_by_key(|record| matches!(record, ScannedRecord::Filed { .. }));
    assert_eq!(scanned, [
        ScannedRecord::Undecodable(UndecodableRecord {
            name: name.clone(),
            key: Some("torn".to_owned()),
        }),
        ScannedRecord::Filed {
            key: "whole".to_owned(),
            value: "v".to_owned(),
        },
    ]);
    assert!(root.join(&name).exists());
    assert_eq!(
        <FileStorage as KvStorageScan<String>>::record_name(&storage, "torn"),
        name
    );
}

/// Scan and index laws under a directory in a record's place: it is reported by its file name,
/// charges nothing against the budget, and the scan as a whole succeeds with the readable
/// records.
#[tokio::test]
async fn test_scan_reports_a_directory_in_a_record_place() {
    let root = temp_root("directory");
    let directory = file_name_for("directory");
    std::fs::create_dir_all(root.join(&directory)).expect("occupy a record name");
    // A budget of exactly one record: the directory must charge nothing, whatever size the file
    // system reports for it (4096 on ext4).
    let storage = FileStorage::new_with_cap_path_and_authority(
        record_len("whole", "v"),
        &root,
        RecordAuthority::Authoritative,
    )
    .await
    .expect("open");
    storage.put("whole", &"v".to_string()).await.expect("put");

    let mut scanned = <FileStorage as KvStorageScan<String>>::scan(&storage)
        .await
        .expect("a bad entry does not fail the scan");
    scanned.sort_by_key(|record| matches!(record, ScannedRecord::Filed { .. }));
    assert_eq!(scanned, [
        ScannedRecord::Undecodable(UndecodableRecord {
            name: directory,
            key: None,
        }),
        ScannedRecord::Filed {
            key: "whole".to_owned(),
            value: "v".to_owned(),
        },
    ]);
}

/// A symbolic link named `name` in `root` that points at itself: its metadata and its contents
/// fail to resolve (`ELOOP`) for every user, root included.
#[cfg(unix)]
fn plant_link_loop(root: &std::path::Path, name: &str) {
    std::fs::create_dir_all(root).expect("root");
    std::os::unix::fs::symlink(name, root.join(name)).expect("plant a link loop");
}

/// Scan law under an unreadable file: a record entry that cannot be read (a looping link) is
/// reported by its file name, and the scan as a whole succeeds with the readable records.
#[cfg(unix)]
#[tokio::test]
async fn test_scan_reports_a_record_it_cannot_read() {
    let root = temp_root("unreadable");
    let looping = file_name_for("looping");
    plant_link_loop(&root, &looping);
    let storage =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("open indexes the loop");
    storage.put("whole", &"v".to_string()).await.expect("put");

    let mut scanned = <FileStorage as KvStorageScan<String>>::scan(&storage)
        .await
        .expect("a bad entry does not fail the scan");
    scanned.sort_by_key(|record| matches!(record, ScannedRecord::Filed { .. }));
    assert_eq!(scanned, [
        ScannedRecord::Undecodable(UndecodableRecord {
            name: looping,
            key: None,
        }),
        ScannedRecord::Filed {
            key: "whole".to_owned(),
            value: "v".to_owned(),
        },
    ]);
}

/// Index law under unreadable metadata: an entry whose metadata cannot be resolved (a looping
/// link) fails a disposable open, while an authoritative open indexes it and a scan reports it.
#[cfg(unix)]
#[tokio::test]
async fn test_authoritative_open_indexes_an_entry_whose_metadata_fails() {
    let root = temp_root("metadata");
    let name = file_name_for("stream");
    plant_link_loop(&root, &name);

    assert!(FileStorage::new_with_cap_and_path(4096, &root)
        .await
        .is_err());
    let storage =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("an authoritative open indexes the entry");
    assert_eq!(
        <FileStorage as KvStorageScan<String>>::scan(&storage)
            .await
            .expect("scan"),
        [ScannedRecord::Undecodable(UndecodableRecord {
            name,
            key: None,
        })]
    );
}

/// Index law under a dangling link: an entry whose target is missing is present, so a scan
/// reports it as unreadable instead of hiding it as absent.
#[cfg(unix)]
#[tokio::test]
async fn test_scan_reports_a_dangling_link_instead_of_hiding_it() {
    let root = temp_root("dangling");
    let name = file_name_for("stream");
    std::fs::create_dir_all(root.as_ref()).expect("root");
    std::os::unix::fs::symlink(root.join("missing-target"), root.join(&name))
        .expect("plant a dangling link");
    let storage =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("open");

    assert_eq!(
        <FileStorage as KvStorageScan<String>>::scan(&storage)
            .await
            .expect("scan"),
        [ScannedRecord::Undecodable(UndecodableRecord {
            name,
            key: None,
        })]
    );
}

/// Root law: an authoritative store whose root vanished while open refuses the next write
/// instead of recreating an empty store; a disposable store recreates it.
#[tokio::test]
async fn test_authoritative_store_refuses_a_write_into_a_vanished_root() {
    let root = temp_root("vanished");
    let authoritative =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("open");
    authoritative
        .put("a", &"v".to_string())
        .await
        .expect("put a");
    std::fs::remove_dir_all(root.as_ref()).expect("remove the root");

    assert!(matches!(
        authoritative.put("b", &"v".to_string()).await,
        Err(Error::StorageRootMissing(ref missing)) if missing.as_path() == root.as_ref()
    ));
    assert!(!root.exists());
    drop(authoritative);

    let disposable = FileStorage::new_with_cap_and_path(4096, &root)
        .await
        .expect("open");
    std::fs::remove_dir_all(root.as_ref()).expect("remove the root");
    disposable.put("b", &"v".to_string()).await.expect("put b");
    assert_eq!(stored_keys(&disposable).await, ["b"]);
}

/// Budget law of an authoritative store: a write that does not fit fails and evicts nothing,
/// and an open under a lowered budget fails instead of retiring records.
#[tokio::test]
async fn test_authoritative_store_evicts_nothing() {
    let root = temp_root("no-eviction");
    let one = record_len("a", "v");
    {
        let storage = FileStorage::new_with_cap_path_and_authority(
            one * 2,
            &root,
            RecordAuthority::Authoritative,
        )
        .await
        .expect("open");
        storage.put("a", &"v".to_string()).await.expect("put a");
        storage.put("b", &"v".to_string()).await.expect("put b");
        storage
            .put("a", &"w".to_string())
            .await
            .expect("rewrite a in place");
        assert!(matches!(
            storage.put("c", &"v".to_string()).await,
            Err(Error::StorageBudgetExhausted { .. })
        ));
        assert_eq!(stored_keys(&storage).await, ["a", "b"]);
        assert!(!root.join(file_name_for("c")).exists());
        assert!(!root.join(file_name_for("c")).with_extension("tmp").exists());
    }
    assert!(matches!(
        FileStorage::new_with_cap_path_and_authority(one, &root, RecordAuthority::Authoritative)
            .await,
        Err(Error::StorageBudgetExhausted { .. })
    ));
    let reopened = FileStorage::new_with_cap_path_and_authority(
        one * 2,
        &root,
        RecordAuthority::Authoritative,
    )
    .await
    .expect("reopen");
    assert_eq!(stored_keys(&reopened).await, ["a", "b"]);
}

/// Decode law under a misfiled record: a whole, decodable record copied over another key's
/// file holds a key that does not hash to its file name, so it is neither key's record: a scan
/// reports it as misfiled, with the key and value it holds, an authoritative `get` of the key it
/// is filed as fails naming it, a `get` of the key it holds does not see it, and a disposable
/// `get` retires it.
#[tokio::test]
async fn test_a_misfiled_whole_record_belongs_to_neither_key() {
    let root = temp_root("misfiled-whole");
    let copied = rings_codec::serialize(&("other", "v")).expect("record serializes");
    let name = plant_record(&root, "stream", &copied);
    let storage =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("open");

    assert_eq!(
        <FileStorage as KvStorageScan<String>>::scan(&storage)
            .await
            .expect("scan"),
        [ScannedRecord::Misfiled {
            name: name.clone(),
            key: "other".to_owned(),
            value: "v".to_owned(),
        }]
    );
    assert!(matches!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "stream").await,
        Err(Error::StorageRecordMisfiled {
            ref record,
            ref key,
        }) if *record == name && key == "other"
    ));
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::get(&storage, "other")
            .await
            .expect("other is absent"),
        None
    );
    drop(storage);

    let disposable = FileStorage::new_with_cap_and_path(4096, &root)
        .await
        .expect("open");
    assert_eq!(
        <FileStorage as KvStorageInterface<String>>::get(&disposable, "stream")
            .await
            .expect("retired"),
        None
    );
    assert!(!root.join(&name).exists());
}

/// Root law: an authoritative root that is no longer a directory is reported missing, and the
/// write changes nothing.
#[tokio::test]
async fn test_authoritative_root_replaced_by_a_file_is_missing() {
    let root = temp_root("root-file");
    let storage =
        FileStorage::new_with_cap_path_and_authority(4096, &root, RecordAuthority::Authoritative)
            .await
            .expect("open");
    std::fs::remove_dir_all(root.as_ref()).expect("remove the root");
    std::fs::write(root.as_ref(), b"not a directory").expect("occupy the root");

    assert!(matches!(
        storage.put("a", &"v".to_string()).await,
        Err(Error::StorageRootMissing(_))
    ));
    std::fs::remove_file(root.as_ref()).expect("release the root");
}

/// Root law, the race after the check: a temporary write that fails because the root vanished
/// (`NotFound`) or stopped being a directory (`NotADirectory`) is reported as the root missing
/// by an authoritative store, and as itself by a disposable one. A unit test of the
/// classification `store_record` routes the temporary write's error through; each store owns its
/// own root, as the index law requires.
#[tokio::test]
async fn test_a_write_that_loses_its_root_reports_the_root_missing() {
    let authoritative_root = temp_root("write-failure-authoritative");
    let disposable_root = temp_root("write-failure-disposable");
    let authoritative = FileStorage::new_with_cap_path_and_authority(
        4096,
        &authoritative_root,
        RecordAuthority::Authoritative,
    )
    .await
    .expect("open");
    let disposable = FileStorage::new_with_cap_and_path(4096, &disposable_root)
        .await
        .expect("open");
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::NotADirectory,
    ] {
        let failed = || Error::ServiceIOError(std::io::Error::from(kind));
        assert!(matches!(
            authoritative.store.write_failure(failed()),
            Error::StorageRootMissing(_)
        ));
        assert!(matches!(
            disposable.store.write_failure(failed()),
            Error::ServiceIOError(_)
        ));
    }
}
