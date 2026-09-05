use serde::Deserialize;
use serde::Serialize;

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
    let _ = std::fs::remove_dir_all(root);
}

fn temp_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "rings-file-kv-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ))
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

    let _ = std::fs::remove_dir_all(root);
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

    let _ = std::fs::remove_dir_all(root);
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
            std::fs::File::open(storage.root.join(file_name_for(key)))
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

    let _ = std::fs::remove_dir_all(root);
}
