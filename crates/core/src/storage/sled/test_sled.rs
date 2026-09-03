use serde::Deserialize;
use serde::Serialize;

use super::*;

#[derive(Debug, Serialize, Deserialize)]
struct TestStorageStruct {
    content: String,
}

#[tokio::test]
async fn test_kv_storage_put_delete() {
    let path = std::env::temp_dir().join(format!(
        "rings-file-kv-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let storage = SledStorage::new_with_cap_and_path(4096, &path)
        .await
        .unwrap();
    let key1 = "test1".to_owned();
    let data1 = TestStorageStruct {
        content: "test1".to_string(),
    };
    storage.put(&key1, &data1).await.unwrap();
    let count1 = <SledStorage as KvStorageInterface<TestStorageStruct>>::count::<'_, '_>(&storage)
        .await
        .unwrap();
    assert!(count1 == 1, "expect count1.1 is {}, got {}", 1, count1);
    let got_v1: TestStorageStruct = storage.get(&key1).await.unwrap().unwrap();
    assert!(
        got_v1.content.eq(data1.content.as_str()),
        "expect value1 is {}, got {}",
        data1.content,
        got_v1.content
    );

    let key2 = "test2".to_owned();
    let data2 = TestStorageStruct {
        content: "test2".to_string(),
    };

    storage.put(&key2, &data2).await.unwrap();

    let count_got_2 =
        <SledStorage as KvStorageInterface<TestStorageStruct>>::count::<'_, '_>(&storage)
            .await
            .unwrap();
    assert!(count_got_2 == 2, "expect count 2, got {count_got_2}");

    let all_entries: Vec<(String, TestStorageStruct)> = storage.get_all().await.unwrap();
    assert!(
        all_entries.len() == 2,
        "all_entries len expect 2, got {}",
        all_entries.len()
    );

    let keys = [key1, key2];
    let values = [data1.content, data2.content];

    assert!(
        all_entries
            .iter()
            .any(|(k, v)| { keys.contains(k) && values.contains(&v.content) }),
        "not found items"
    );
    let data3: u64 = 101;
    let key3 = "key3".to_owned();
    storage.put(&key3, &data3).await.unwrap();
    let got_d3: u64 = storage.get(&key3).await.unwrap().unwrap();
    assert!(data3 == got_d3, "expect {data3}, got {got_d3}");

    // Clear full db and check if it's count is zero now.
    <SledStorage as KvStorageInterface<TestStorageStruct>>::clear::<'_, '_>(&storage)
        .await
        .unwrap();
    let count1 = <SledStorage as KvStorageInterface<TestStorageStruct>>::count::<'_, '_>(&storage)
        .await
        .unwrap();
    assert!(count1 == 0, "expect count1 is 0, got {count1}");

    drop(storage);
    let _ = std::fs::remove_dir_all(path);
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

async fn stored_keys(storage: &SledStorage) -> Vec<String> {
    let mut keys = <SledStorage as KvStorageInterface<String>>::get_all(storage)
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
    let storage = SledStorage::new_with_cap_and_path(one * 2, &root)
        .await
        .expect("open");

    storage.put("a", &"v".to_string()).await.expect("put a");
    storage.put("b", &"v".to_string()).await.expect("put b");
    storage.put("a", &"w".to_string()).await.expect("rewrite a");
    assert_eq!(stored_keys(&storage).await, ["a", "b"]);

    storage.put("c", &"v".to_string()).await.expect("put c");
    assert_eq!(stored_keys(&storage).await, ["a", "c"]);
    assert_eq!(
        <SledStorage as KvStorageInterface<String>>::get(&storage, "b")
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
    let storage = SledStorage::new_with_cap_and_path(one, &root)
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
        let storage = SledStorage::new_with_cap_and_path(one * 3, &root)
            .await
            .expect("open");
        for (index, key) in ["a", "b", "c"].into_iter().enumerate() {
            storage.put(key, &"v".to_string()).await.expect("put");
            let modified = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000 + index as u64);
            std::fs::File::open(storage.key_path(key))
                .expect("open file")
                .set_modified(modified)
                .expect("set modified");
        }
    }

    let reopened = SledStorage::new_with_cap_and_path(one * 2, &root)
        .await
        .expect("reopen");
    assert_eq!(stored_keys(&reopened).await, ["b", "c"]);
    assert_eq!(
        <SledStorage as KvStorageInterface<String>>::count(&reopened)
            .await
            .expect("count"),
        2
    );

    let _ = std::fs::remove_dir_all(root);
}
