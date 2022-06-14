use std::mem::size_of_val;

use rexie::TransactionMode;
use rings_core::storage::persistence::{
    idb::IDBStorageBasic, IDBStorage, PersistenceStorageOperation, PersistenceStorageReadAndWrite,
    PersistenceStorageRemove,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

async fn create_db_instance(cap: usize) -> IDBStorage {
    //console_log::init_with_level(log::Level::Debug).unwrap();
    let instance = IDBStorage::new_with_cap(cap).await.unwrap();
    let (tx, store) = instance.get_tx_store(TransactionMode::ReadWrite).unwrap();
    store.clear().await.unwrap();
    tx.done().await.unwrap();
    let (_tx, store) = instance.get_tx_store(TransactionMode::ReadOnly).unwrap();
    assert!(store.count(None).await.unwrap() == 0, "store not empty");
    instance
}

#[derive(Serialize, Deserialize, Debug)]
struct TestDataStruct {
    content: String,
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
    let real_value_1: JsonValue = real_value_1.into_serde().unwrap();
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

    let r: TestDataStruct = instance.get(&key).await.unwrap();
    log::debug!("{:?}", r);
    assert_eq!(r.content, value.content);

    let (_tx, store) = instance.get_tx_store(TransactionMode::ReadOnly).unwrap();
    assert!(store.count(None).await.unwrap() == 1, "indexedDB is empty");
    let real_value_2 = store.get(&JsValue::from(&key)).await.unwrap();
    let real_value_2: JsonValue = real_value_2.into_serde().unwrap();
    let last_visit_2 = real_value_2
        .get("last_visit_time")
        .unwrap()
        .as_i64()
        .unwrap();

    assert!(
        last_visit_1 != last_visit_2,
        "last_visit_1 and last_visit_2 is same, {}",
        last_visit_1
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
    let instance = create_db_instance(4).await;
    instance
        .put(&"1".to_string(), &serde_json::json!("test1"))
        .await
        .unwrap();
    instance
        .put(&"2".to_string(), &serde_json::json!("test2"))
        .await
        .unwrap();
    instance
        .put(&"3".to_string(), &serde_json::json!("test3"))
        .await
        .unwrap();
    instance
        .put(&"4".to_string(), &serde_json::json!("test4"))
        .await
        .unwrap();
    let count = instance.count().await.unwrap();
    assert!(count == 4, "count error, got: {:?}, expect: {:?}", count, 4);
    instance.clear().await.unwrap();
    let (_tx, store) = instance.get_tx_store(TransactionMode::ReadOnly).unwrap();
    assert!(
        store.count(None).await.unwrap() == 0,
        "indexedDB is not empty"
    );
}

#[wasm_bindgen_test]
async fn test_indexed_db_remove() {
    let instance = create_db_instance(4).await;
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
    let instance = create_db_instance(4).await;
    let key1 = "1".to_string();
    let key2 = "2".to_string();
    let key3 = "3".to_string();
    let key4 = "4".to_string();
    let key5 = "5".to_string();
    instance
        .put(
            &key1,
            &TestDataStruct {
                content: "test1".to_owned(),
            },
        )
        .await
        .unwrap();
    instance
        .put(
            &key2,
            &TestDataStruct {
                content: "test2".to_owned(),
            },
        )
        .await
        .unwrap();
    instance
        .put(
            &key3,
            &TestDataStruct {
                content: "test3".to_owned(),
            },
        )
        .await
        .unwrap();
    instance
        .put(
            &key4,
            &TestDataStruct {
                content: "test4".to_owned(),
            },
        )
        .await
        .unwrap();

    let d3: TestDataStruct = instance.get(&key3).await.unwrap();
    log::debug!("d3, {:?}", d3);
    let d3: TestDataStruct = instance.get(&key3).await.unwrap();
    log::debug!("d3, {:?}", d3);
    let d1: TestDataStruct = instance.get(&key1).await.unwrap();
    log::debug!("d1, {:?}", d1);
    let d2: TestDataStruct = instance.get(&key2).await.unwrap();
    log::debug!("d2, {:?}", d2);

    instance
        .put(
            &key5,
            &TestDataStruct {
                content: "test5".to_owned(),
            },
        )
        .await
        .unwrap();

    let entries: Vec<(String, TestDataStruct)> = instance.get_all().await.unwrap();
    assert!(
        !entries.iter().any(|(k, _v)| k.eq(&key4)),
        "key4 should be deleted"
    );

    instance.clear().await.unwrap();
    assert!(
        instance.count().await.unwrap() == 0,
        "indexedDB is not empty"
    );
}

#[wasm_bindgen_test]
async fn test_idb_total_size() {
    let instance = create_db_instance(4).await;
    let key1 = "1".to_string();
    let value1 = TestDataStruct {
        content: "test1".to_owned(),
    };
    instance.put(&key1, &value1).await.unwrap();
    let expect_size =
        size_of_val(&JsValue::from(key1)) + size_of_val(&JsValue::from_serde(&value1).unwrap());
    let total_size = instance.total_size().await.unwrap();
    assert!(total_size > 0, "total_size should > 0");
    assert!(
        total_size == expect_size,
        "total_size got: {}, expect: {}",
        total_size,
        expect_size
    );

    instance.clear().await.unwrap();
    assert!(
        instance.count().await.unwrap() == 0,
        "indexedDB is not empty"
    );
}
