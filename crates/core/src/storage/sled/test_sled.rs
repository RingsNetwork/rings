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
