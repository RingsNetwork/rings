use rexie::TransactionMode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::storage::idb::IdbStorage;
use crate::storage::KvStorageInterface;

#[derive(Serialize, Deserialize, Debug)]
struct TestDataStruct {
    content: String,
}

async fn create_db_instance(cap: u32) -> IdbStorage {
    let instance = IdbStorage::new_with_cap(cap).await.unwrap();
    instance.clear().await.unwrap();
    let count = instance.count().await.unwrap();
    assert_eq!(count, 0, "store not empty");
    instance
}

async fn create_kv_db<V>(cap: u32) -> Box<dyn KvStorageInterface<V>>
where V: DeserializeOwned + Serialize + Sized {
    Box::new(create_db_instance(cap).await)
}

#[wasm_bindgen_test]
async fn test_create_put_data() {
    let instance = create_db_instance(4).await;

    let key = "1".to_string();
    let value = TestDataStruct {
        content: "content1".to_string(),
    };
    instance.put(&key, &value).await.unwrap();

    let (_tx, store) = instance.get_tx_store(TransactionMode::ReadOnly).unwrap();
    assert!(store.count(None).await.unwrap() == 1, "indexedDB is empty");

    let real_value_1 = store.get(&JsValue::from(&key)).await.unwrap();
    let real_value_1: JsonValue = serde_wasm_bindgen::from_value(real_value_1).unwrap();
    let last_visit_1 = real_value_1
        .get("last_visit_time")
        .unwrap()
        .as_i64()
        .unwrap();

    assert!(
        real_value_1.get("visit_count").unwrap().as_i64().unwrap() == 0,
        "visit_count not zero"
    );
    let real_value_data_1: TestDataStruct =
        serde_json::from_value(real_value_1.get("data").unwrap().to_owned()).unwrap();
    assert!(
        real_value_data_1.content.eq(&value.content),
        "Data content in store not same: expect {}, got {}",
        value.content,
        real_value_data_1.content
    );

    let r: TestDataStruct = instance.get(&key).await.unwrap().unwrap();
    tracing::debug!("{:?}", r);
    assert_eq!(r.content, value.content);

    let (_tx, store) = instance.get_tx_store(TransactionMode::ReadOnly).unwrap();
    assert!(store.count(None).await.unwrap() == 1, "indexedDB is empty");
    let real_value_2 = store.get(&JsValue::from(&key)).await.unwrap();
    let real_value_2: JsonValue = serde_wasm_bindgen::from_value(real_value_2).unwrap();
    let last_visit_2 = real_value_2
        .get("last_visit_time")
        .unwrap()
        .as_i64()
        .unwrap();

    assert!(
        last_visit_1 != last_visit_2,
        "last_visit_1 and last_visit_2 is same, {last_visit_1}"
    );
    assert!(
        real_value_2.get("visit_count").unwrap().as_i64().unwrap() == 1,
        "2. visit_count not zero"
    );
    let real_value_data_2: TestDataStruct =
        serde_json::from_value(real_value_2.get("data").unwrap().to_owned()).unwrap();
    assert!(
        real_value_data_2.content.eq(&value.content),
        "2. Data content in store not same: expect {}, got {}",
        value.content,
        real_value_data_2.content
    );

    instance.clear().await.unwrap();
    let (_tx, store) = instance.get_tx_store(TransactionMode::ReadOnly).unwrap();
    assert!(
        store.count(None).await.unwrap() == 0,
        "indexedDB is not empty"
    );
}

#[wasm_bindgen_test]
async fn test_indexed_db_count() {
    let instance = create_kv_db::<JsonValue>(4).await;
    instance
        .put("1", &serde_json::json!("test1"))
        .await
        .unwrap();
    instance
        .put("2", &serde_json::json!("test2"))
        .await
        .unwrap();
    instance
        .put("3", &serde_json::json!("test3"))
        .await
        .unwrap();
    instance
        .put("4", &serde_json::json!("test4"))
        .await
        .unwrap();
    let count = instance.count().await.unwrap();
    assert!(count == 4, "count error, got: {:?}, expect: {:?}", count, 4);
    instance.clear().await.unwrap();
    let count = instance.count().await.unwrap();
    assert_eq!(count, 0, "indexedDB is not empty");
}

#[wasm_bindgen_test]
async fn test_indexed_db_remove() {
    let instance = create_kv_db::<JsonValue>(4).await;
    let key1 = "1".to_string();
    let key2 = "2".to_string();
    let key3 = "3".to_string();
    let key4 = "4".to_string();
    instance
        .put(&key1, &serde_json::json!("test1"))
        .await
        .unwrap();
    instance
        .put(&key2, &serde_json::json!("test2"))
        .await
        .unwrap();
    instance
        .put(&key3, &serde_json::json!("test3"))
        .await
        .unwrap();
    instance
        .put(&key4, &serde_json::json!("test4"))
        .await
        .unwrap();
    let count = instance.count().await.unwrap();
    assert!(count == 4, "count error, got: {:?}, expect: {:?}", count, 4);

    instance.remove(&key1).await.unwrap();
    let count = instance.count().await.unwrap();
    assert!(count == 3, "count error, got: {:?}, expect: {:?}", count, 3);

    instance.clear().await.unwrap();
    assert!(
        instance.count().await.unwrap() == 0,
        "indexedDB is not empty"
    );
}

#[wasm_bindgen_test]
async fn test_idb_prune() {
    super::setup_log();
    let instance = create_kv_db::<TestDataStruct>(4).await;
    let key1 = "1".to_string();
    let key2 = "2".to_string();
    let key3 = "3".to_string();
    let key4 = "4".to_string();
    let key5 = "5".to_string();
    instance
        .put(&key1, &TestDataStruct {
            content: "test1".to_owned(),
        })
        .await
        .unwrap();
    instance
        .put(&key2, &TestDataStruct {
            content: "test2".to_owned(),
        })
        .await
        .unwrap();
    instance
        .put(&key3, &TestDataStruct {
            content: "test3".to_owned(),
        })
        .await
        .unwrap();
    instance
        .put(&key4, &TestDataStruct {
            content: "test4".to_owned(),
        })
        .await
        .unwrap();

    let d3: TestDataStruct = instance.get(&key3).await.unwrap().unwrap();
    tracing::debug!("d3, {:?}", d3);
    let d3: TestDataStruct = instance.get(&key3).await.unwrap().unwrap();
    tracing::debug!("d3, {:?}", d3);
    let d1: TestDataStruct = instance.get(&key1).await.unwrap().unwrap();
    tracing::debug!("d1, {:?}", d1);
    let d2: TestDataStruct = instance.get(&key2).await.unwrap().unwrap();
    tracing::debug!("d2, {:?}", d2);

    instance
        .put(&key5, &TestDataStruct {
            content: "test5".to_owned(),
        })
        .await
        .unwrap();

    let entries: Vec<(String, TestDataStruct)> = instance.get_all().await.unwrap();
    assert!(
        !entries.iter().any(|(k, _v)| k.eq(&key4)),
        "key4 should be deleted"
    );

    instance.clear().await.unwrap();
    let count = instance.count().await.unwrap();
    assert_eq!(count, 0, "indexedDB is not empty");
}
