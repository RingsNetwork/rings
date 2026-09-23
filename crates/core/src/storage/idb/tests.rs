//! Browser witnesses for stored row shape, atomic touches, errors, and LRU eviction.

use rexie::TransactionMode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde_json::Value as JsonValue;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::storage::idb::next_visit_time_after;
use crate::storage::idb::IdbStorage;
use crate::storage::KvStorageInterface;

#[derive(Serialize, Deserialize, Debug)]
struct TestDataStruct {
    content: String,
}

/// Value used to make adapter serialization fail before any IndexedDB transaction starts.
#[derive(Deserialize)]
struct FailingSerialize;

impl Serialize for FailingSerialize {
    /// Return a deliberate serialization error for the write-rollback regression case.
    fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
    where S: Serializer {
        Err(serde::ser::Error::custom(
            "deliberate serialization failure",
        ))
    }
}

async fn create_db_instance(cap: u32) -> IdbStorage {
    // Every browser test owns a fresh database; never clear a user database.
    let name = format!("rings-idb-test-{}", uuid::Uuid::new_v4());
    let instance = IdbStorage::new_with_cap_and_name(cap, &name).await.unwrap();
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
fn test_next_visit_time_uses_wall_clock_when_it_advances() {
    assert_eq!(next_visit_time_after(10, 15), 15);
}

#[wasm_bindgen_test]
fn test_next_visit_time_advances_when_wall_clock_stalls_or_rewinds() {
    assert_eq!(next_visit_time_after(10, 10), 11);
    assert_eq!(next_visit_time_after(10, 5), 11);
}

#[wasm_bindgen_test]
fn test_next_visit_time_saturates_at_i64_max() {
    assert_eq!(next_visit_time_after(i64::MAX, i64::MIN), i64::MAX);
}

#[wasm_bindgen_test]
async fn test_create_put_data() {
    let instance = create_db_instance(4).await;

    let key = "1".to_string();
    let value = TestDataStruct {
        content: "content1".to_string(),
    };
    instance.put(&key, &value).await.unwrap();

    let (_tx, store) = instance.transaction(TransactionMode::ReadOnly).unwrap();
    assert!(store.count(None).await.unwrap() == 1, "indexedDB is empty");

    let real_value_1 = store.get(&JsValue::from(&key)).await.unwrap();
    let real_value_1: JsonValue = serde_wasm_bindgen::from_value(real_value_1).unwrap();
    let last_visit_1 = real_value_1
        .get("last_visit_time")
        .unwrap()
        .as_i64()
        .unwrap();

    assert!(real_value_1.get("visit_count").is_none());
    assert!(real_value_1.get("created_time").is_none());
    assert!(store.index("visit_count").is_err());
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

    let (_tx, store) = instance.transaction(TransactionMode::ReadOnly).unwrap();
    assert!(store.count(None).await.unwrap() == 1, "indexedDB is empty");
    let real_value_2 = store.get(&JsValue::from(&key)).await.unwrap();
    let real_value_2: JsonValue = serde_wasm_bindgen::from_value(real_value_2).unwrap();
    let last_visit_2 = real_value_2
        .get("last_visit_time")
        .unwrap()
        .as_i64()
        .unwrap();

    assert!(
        last_visit_1 < last_visit_2,
        "last_visit_1 and last_visit_2 is same, {last_visit_1}"
    );
    assert!(real_value_2.get("visit_count").is_none());
    assert!(real_value_2.get("created_time").is_none());
    let real_value_data_2: TestDataStruct =
        serde_json::from_value(real_value_2.get("data").unwrap().to_owned()).unwrap();
    assert!(
        real_value_data_2.content.eq(&value.content),
        "2. Data content in store not same: expect {}, got {}",
        value.content,
        real_value_data_2.content
    );

    instance.clear().await.unwrap();
    let (_tx, store) = instance.transaction(TransactionMode::ReadOnly).unwrap();
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
    tracing_wasm::set_as_global_default();
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

/// Reopening does not upgrade or erase an existing database; touches remove unused fields.
#[wasm_bindgen_test]
async fn reopen_existing_database_preserves_data_and_lru() {
    // Unique scope models the previous schema without opening any application database.
    let name = format!("rings-idb-old-schema-test-{}", uuid::Uuid::new_v4());
    // The old schema had one unused index in addition to the eviction index.
    let old = rexie::Rexie::builder(&name)
        .add_object_store(
            rexie::ObjectStore::new(&name)
                .key_path("key")
                .add_index(rexie::Index::new("last_visit_time", "last_visit_time"))
                .add_index(rexie::Index::new("visit_count", "visit_count")),
        )
        .build()
        .await
        .unwrap();
    // Seed a committed row carrying fields that the new reader does not need.
    let transaction = old
        .transaction(&[&name], TransactionMode::ReadWrite)
        .unwrap();
    let store = transaction.store(&name).unwrap();
    let old_row = serde_json::json!({
        "key": "retained", "data": "payload", "last_visit_time": 1,
        "visit_count": u32::MAX, "created_time": 0
    });
    store
        .put(&crate::utils::js_value::serialize(&old_row).unwrap(), None)
        .await
        .unwrap();
    transaction.done().await.unwrap();
    old.close();

    // Opening at the existing version must not run a destructive upgrade.
    let reopened = IdbStorage::new_with_cap_and_name(2, &name).await.unwrap();
    let value: Option<String> = reopened.get("retained").await.unwrap();
    assert_eq!(value.as_deref(), Some("payload"));
    let (transaction, store) = reopened.transaction(TransactionMode::ReadOnly).unwrap();
    assert!(
        store.index("visit_count").is_ok(),
        "same-version open retains the unused index"
    );
    let row: JsonValue =
        crate::utils::js_value::deserialize(store.get(&"retained".into()).await.unwrap()).unwrap();
    assert!(row.get("visit_count").is_none());
    assert!(row.get("created_time").is_none());
    assert!(row["last_visit_time"].as_i64().unwrap() > 1);
    transaction.done().await.unwrap();

    // A cold row is older than the touched row, regardless of browser clock resolution.
    let (transaction, store) = reopened.transaction(TransactionMode::ReadWrite).unwrap();
    let cold_row = serde_json::json!({"key": "cold", "data": "cold", "last_visit_time": 0});
    store
        .put(&crate::utils::js_value::serialize(&cold_row).unwrap(), None)
        .await
        .unwrap();
    transaction.done().await.unwrap();
    // Owned payload matches the adapter's DeserializeOwned contract.
    let replacement = String::from("new");
    reopened.put("new", &replacement).await.unwrap();
    let entries: Vec<(String, String)> = reopened.get_all().await.unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|(key, _)| key == "retained"));
    assert!(!entries.iter().any(|(key, _)| key == "cold"));
}

/// Missing keys complete cleanly; a decode error does not rewrite or remove the stored payload.
#[wasm_bindgen_test]
async fn missing_and_invalid_reads_preserve_storage() {
    // Isolated store exercises the public error boundary with incompatible value types.
    let instance = create_db_instance(2).await;
    let missing: Option<String> = instance.get("missing").await.unwrap();
    assert!(missing.is_none());
    instance.put("number", &42_u32).await.unwrap();
    let invalid: crate::error::Result<Option<TestDataStruct>> = instance.get("number").await;
    assert!(invalid.is_err());
    let retained: Option<u32> = instance.get("number").await.unwrap();
    assert_eq!(retained, Some(42));
    assert_eq!(instance.count().await.unwrap(), 1);
}

/// The named constructor rejects zero capacity before opening IndexedDB.
#[wasm_bindgen_test]
async fn zero_capacity_is_rejected() {
    // Rejection does not create this uniquely named database.
    let name = format!("rings-idb-zero-test-{}", uuid::Uuid::new_v4());
    assert!(matches!(
        IdbStorage::new_with_cap_and_name(0, &name).await,
        Err(crate::error::Error::InvalidCapacity)
    ));
}

/// Rewriting a row at capacity preserves every unrelated row and keeps the row count fixed.
#[wasm_bindgen_test]
async fn overwrite_at_capacity_preserves_other_rows() {
    // The two-row store reaches its limit before the existing key is replaced.
    let instance = create_db_instance(2).await;
    instance.put("a", &"old-a".to_owned()).await.unwrap();
    instance.put("b", &"old-b".to_owned()).await.unwrap();

    // Replacing b must not evict a, despite the store already being full.
    instance.put("b", &"new-b".to_owned()).await.unwrap();
    assert_eq!(instance.count().await.unwrap(), 2);
    assert_eq!(instance.get("a").await.unwrap().as_deref(), Some("old-a"));
    assert_eq!(instance.get("b").await.unwrap().as_deref(), Some("new-b"));
}

/// A new key at capacity evicts the least recently accessed row in the same transaction.
#[wasm_bindgen_test]
async fn new_key_at_capacity_evicts_lru_row() {
    // The explicit access below makes a newer than b before the insertion of c.
    let instance = create_db_instance(2).await;
    instance.put("a", &"a".to_owned()).await.unwrap();
    instance.put("b", &"b".to_owned()).await.unwrap();
    assert_eq!(instance.get("a").await.unwrap().as_deref(), Some("a"));

    // Accessing a makes b the least-recently-accessed row.
    instance.put("c", &"c".to_owned()).await.unwrap();
    assert_eq!(instance.count().await.unwrap(), 2);
    assert_eq!(instance.get("a").await.unwrap().as_deref(), Some("a"));
    assert_eq!(instance.get("b").await.unwrap(), None);
    assert_eq!(instance.get("c").await.unwrap().as_deref(), Some("c"));
}

/// Reopening with a smaller row budget removes every excess row, not just one candidate.
#[wasm_bindgen_test]
async fn reopening_with_smaller_capacity_removes_all_excess_rows() {
    // The first instance persists four rows with explicit ordering in a unique database.
    let name = format!("rings-idb-lowered-cap-test-{}", uuid::Uuid::new_v4());
    let initial = IdbStorage::new_with_cap_and_name(4, &name).await.unwrap();
    let (transaction, store) = initial.transaction(TransactionMode::ReadWrite).unwrap();
    for (key, timestamp) in [("a", 1_i64), ("b", 2), ("c", 3), ("d", 4)] {
        // Distinct timestamps make survivors independent of browser clock timing.
        let row = serde_json::json!({"key": key, "data": key, "last_visit_time": timestamp});
        store
            .put(&crate::utils::js_value::serialize(&row).unwrap(), None)
            .await
            .unwrap();
    }
    transaction.done().await.unwrap();
    drop(initial);

    // Reopening at capacity two must keep the two most recently written rows only.
    let reopened = IdbStorage::new_with_cap_and_name(2, &name).await.unwrap();
    assert_eq!(reopened.count().await.unwrap(), 2);
    assert_eq!(reopened.get("a").await.unwrap(), None);
    assert_eq!(reopened.get("b").await.unwrap(), None);
    assert_eq!(reopened.get("c").await.unwrap().as_deref(), Some("c"));
    assert_eq!(reopened.get("d").await.unwrap().as_deref(), Some("d"));
}

/// Concurrent puts serialize their row-budget decisions through IndexedDB transactions.
#[wasm_bindgen_test]
async fn concurrent_puts_never_exceed_row_capacity() {
    // The same isolated two-row database receives simultaneous first writes.
    let instance = create_db_instance(2).await;
    let (first, second, third, fourth) = futures::join!(
        instance.put("a", &"a".to_owned()),
        instance.put("b", &"b".to_owned()),
        instance.put("c", &"c".to_owned()),
        instance.put("d", &"d".to_owned()),
    );
    first.unwrap();
    second.unwrap();
    third.unwrap();
    fourth.unwrap();
    assert_eq!(instance.count().await.unwrap(), 2);
    // Name the payload type explicitly because only the vector length is otherwise observed.
    let entries: Vec<(String, String)> = instance.get_all().await.unwrap();
    assert_eq!(entries.len(), 2);
}

/// A replacement serialization failure leaves all rows untouched, including eviction targets.
#[wasm_bindgen_test]
async fn failed_serialization_does_not_evict_existing_rows() {
    // A full store provides an eviction candidate if the implementation prunes too early.
    let instance = create_db_instance(2).await;
    instance.put("a", &"a".to_owned()).await.unwrap();
    instance.put("b", &"b".to_owned()).await.unwrap();

    // A serialization error must occur before the write transaction can delete either row.
    let result = instance.put("c", &FailingSerialize).await;
    assert!(result.is_err());
    assert_eq!(instance.count().await.unwrap(), 2);
    assert_eq!(instance.get("a").await.unwrap().as_deref(), Some("a"));
    assert_eq!(instance.get("b").await.unwrap().as_deref(), Some("b"));
}
