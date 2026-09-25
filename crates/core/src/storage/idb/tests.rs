//! Browser witnesses for the access-clock law, stored row shape, atomic touches, errors,
//! schema migration, and LRU eviction. No test waits on or reads wall-clock time.

use rexie::TransactionMode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde_json::Value as JsonValue;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::storage::idb::clock_store_name;
use crate::storage::idb::restamp;
use crate::storage::idb::AccessClock;
use crate::storage::idb::AccessStamp;
use crate::storage::idb::IdbStorage;
use crate::storage::idb::LegacyRow;
use crate::storage::idb::ACCESS_STAMP_INDEX;
use crate::storage::idb::CLOCK_LIMIT;
use crate::storage::idb::SCHEMA_VERSION;
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

/// Read a string value with the trait payload type fixed for concise browser assertions.
async fn get_string(instance: &IdbStorage, key: &str) -> crate::error::Result<Option<String>> {
    <IdbStorage as KvStorageInterface<String>>::get(instance, key).await
}

/// Raw stored row `key` as JSON, read outside the adapter's decoding.
async fn raw_row(instance: &IdbStorage, key: &str) -> JsonValue {
    let scope = instance.scope(TransactionMode::ReadOnly).unwrap();
    let row = scope.rows.get(&JsValue::from(key)).await.unwrap();
    serde_wasm_bindgen::from_value(row).unwrap()
}

/// Access stamp currently stored on row `key`.
async fn stamp_of(instance: &IdbStorage, key: &str) -> u64 {
    raw_row(instance, key).await["access_stamp"]
        .as_u64()
        .unwrap()
}

/// Value of the store-wide access clock.
async fn clock_of(instance: &IdbStorage) -> u64 {
    let scope = instance.scope(TransactionMode::ReadOnly).unwrap();
    scope.clock_record().await.unwrap().unwrap().0
}

/// Law: k ticks from any clock c yield the k stamps c, c + 1, …, c + k − 1 and the clock c + k.
#[wasm_bindgen_test]
fn access_clock_ticks_are_consecutive_and_strictly_increasing() {
    const TICKS: u64 = 1024;
    for origin in [AccessClock::ORIGIN, AccessClock(CLOCK_LIMIT - TICKS)] {
        let (stamps, clock) = (0..TICKS).fold((Vec::new(), origin), |(mut stamps, clock), _| {
            let (stamp, successor) = clock.tick().unwrap();
            stamps.push(stamp);
            (stamps, successor)
        });
        let expected = (origin.0..origin.0 + TICKS)
            .map(AccessStamp)
            .collect::<Vec<_>>();
        assert_eq!(stamps, expected);
        assert!(stamps.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(clock, AccessClock(origin.0 + TICKS));
    }
}

/// The clock stops at the exact-integer limit instead of repeating or rounding a stamp.
#[wasm_bindgen_test]
fn access_clock_refuses_to_leave_the_exact_integer_range() {
    let (stamp, last) = AccessClock(CLOCK_LIMIT - 1).tick().unwrap();
    assert_eq!(stamp, AccessStamp(CLOCK_LIMIT - 1));
    assert_eq!(last, AccessClock(CLOCK_LIMIT));
    assert!(matches!(
        last.tick(),
        Err(crate::error::Error::IdbAccessClockExhausted(CLOCK_LIMIT))
    ));
    assert!(AccessClock(u64::MAX).tick().is_err());
}

/// Legacy rows are stamped 0‥n by (last_visit_time, key); ties and missing times are ordered.
#[wasm_bindgen_test]
fn restamp_preserves_and_strictifies_the_legacy_eviction_order() {
    let legacy = |key: &str, last_visit_time: Option<i64>| LegacyRow {
        key: key.to_owned(),
        last_visit_time,
        data: JsValue::from(key),
    };
    let rows = vec![
        legacy("late", Some(9)),
        legacy("tie-b", Some(5)),
        legacy("unstamped", None),
        legacy("tie-a", Some(5)),
    ];
    let (restamped, clock) = restamp(rows).unwrap();
    let order = restamped
        .iter()
        .map(|row| (row.key.as_str(), row.access_stamp.0, row.data.as_string()))
        .collect::<Vec<_>>();
    assert_eq!(order, vec![
        ("unstamped", 0, Some("unstamped".to_owned())),
        ("tie-a", 1, Some("tie-a".to_owned())),
        ("tie-b", 2, Some("tie-b".to_owned())),
        ("late", 3, Some("late".to_owned())),
    ]);
    assert_eq!(clock, AccessClock(4));
}

/// A row is stored as `{key, access_stamp, data}`, and a hit restamps it with the next tick.
#[wasm_bindgen_test]
async fn test_create_put_data() {
    let instance = create_db_instance(4).await;

    let key = "1".to_string();
    let value = TestDataStruct {
        content: "content1".to_string(),
    };
    instance.put(&key, &value).await.unwrap();
    assert_eq!(instance.count().await.unwrap(), 1);

    let row = raw_row(&instance, &key).await;
    assert_eq!(row["key"], key);
    assert_eq!(row["access_stamp"], 0);
    assert_eq!(row["data"]["content"], value.content);
    assert!(row.get("last_visit_time").is_none());
    assert!(row.get("visit_count").is_none());
    assert!(row.get("created_time").is_none());
    let scope = instance.scope(TransactionMode::ReadOnly).unwrap();
    assert_eq!(
        scope.rows.index_names(),
        vec![ACCESS_STAMP_INDEX.to_owned()]
    );
    drop(scope);

    let read: TestDataStruct = instance.get(&key).await.unwrap().unwrap();
    assert_eq!(read.content, value.content);
    assert_eq!(stamp_of(&instance, &key).await, 1);
    assert_eq!(clock_of(&instance).await, 2);

    instance.clear().await.unwrap();
    assert_eq!(instance.count().await.unwrap(), 0);
}

/// Law, observed through IndexedDB: back-to-back accesses, which share one millisecond on
/// coarse browser timers, receive consecutive stamps in access order; `clear` keeps the clock.
#[wasm_bindgen_test]
async fn accesses_within_one_millisecond_receive_strictly_increasing_stamps() {
    const KEYS: usize = 32;
    let instance = create_db_instance(u32::try_from(KEYS).unwrap()).await;
    let keys = (0..KEYS)
        .map(|index| format!("k{index}"))
        .collect::<Vec<_>>();
    for key in keys.iter() {
        instance.put(key, key).await.unwrap();
    }
    // Touch in reverse order: the last written key becomes the least recent.
    for key in keys.iter().rev() {
        assert_eq!(
            get_string(&instance, key).await.unwrap().as_ref(),
            Some(key)
        );
    }
    let mut stamps = Vec::with_capacity(KEYS);
    for key in keys.iter().rev() {
        stamps.push(stamp_of(&instance, key).await);
    }
    let expected = (KEYS as u64..2 * KEYS as u64).collect::<Vec<_>>();
    assert_eq!(stamps, expected);
    assert_eq!(clock_of(&instance).await, 2 * KEYS as u64);

    // A miss is not an access; clearing rows does not rewind the clock.
    assert_eq!(get_string(&instance, "missing").await.unwrap(), None);
    instance.clear().await.unwrap();
    instance.put("after-clear", &"x".to_owned()).await.unwrap();
    assert_eq!(stamp_of(&instance, "after-clear").await, 2 * KEYS as u64);
}

/// Eviction follows the access order exactly for a burst of operations with no pause.
#[wasm_bindgen_test]
async fn burst_eviction_retires_exactly_the_least_recent_rows() {
    let instance = create_db_instance(8).await;
    let keys = (0..8).map(|index| format!("k{index}")).collect::<Vec<_>>();
    for key in keys.iter() {
        instance.put(key, key).await.unwrap();
    }
    // Recency after this loop, oldest first: k7, k6, …, k0.
    for key in keys.iter().rev() {
        get_string(&instance, key).await.unwrap();
    }
    for index in 0..4 {
        let key = format!("n{index}");
        instance.put(&key, &key).await.unwrap();
    }
    let mut survivors = <IdbStorage as KvStorageInterface<String>>::get_all(&instance)
        .await
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    survivors.sort();
    assert_eq!(survivors, vec![
        "k0", "k1", "k2", "k3", "n0", "n1", "n2", "n3"
    ]);
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

/// Seed a database in a pre-clock layout with `rows` and close it.
///
/// `version = None` models a version-1 database (opened without a version, wall-clock indexes).
/// `Some(SCHEMA_VERSION)` models a migration interrupted after the upgrade committed: the clock
/// store and index exist, but the clock record and the restamped rows do not.
async fn seed_legacy_database(name: &str, version: Option<u32>, rows: &[JsonValue]) {
    let builder = rexie::Rexie::builder(name);
    let builder = match version {
        None => builder.add_object_store(
            rexie::ObjectStore::new(name)
                .key_path("key")
                .add_index(rexie::Index::new("last_visit_time", "last_visit_time"))
                .add_index(rexie::Index::new("visit_count", "visit_count")),
        ),
        Some(version) => builder
            .version(version)
            .add_object_store(
                rexie::ObjectStore::new(name)
                    .key_path("key")
                    .add_index(rexie::Index::new(ACCESS_STAMP_INDEX, ACCESS_STAMP_INDEX)),
            )
            .add_object_store(rexie::ObjectStore::new(&clock_store_name(name))),
    };
    let database = builder.build().await.unwrap();
    let transaction = database
        .transaction(&[name], TransactionMode::ReadWrite)
        .unwrap();
    let store = transaction.store(name).unwrap();
    for row in rows {
        store
            .put(&crate::utils::js_value::serialize(row).unwrap(), None)
            .await
            .unwrap();
    }
    transaction.done().await.unwrap();
    database.close();
}

/// Rows as written by schema version 1, including a same-millisecond tie and surplus fields.
fn legacy_rows() -> Vec<JsonValue> {
    vec![
        serde_json::json!({"key": "warm", "data": "warm", "last_visit_time": 1_700_000_000_005_i64}),
        serde_json::json!({"key": "tie-b", "data": "tie-b", "last_visit_time": 1_700_000_000_001_i64,
            "visit_count": u32::MAX, "created_time": 0}),
        serde_json::json!({"key": "tie-a", "data": {"nested": [1, 2]}, "last_visit_time": 1_700_000_000_001_i64}),
    ]
}

/// Assert the migrated layout: rows restamped in legacy order, clock `n`, only the new index.
async fn assert_migrated(reopened: &IdbStorage) {
    assert_eq!(reopened.db.version(), f64::from(SCHEMA_VERSION));
    assert_eq!(stamp_of(reopened, "tie-a").await, 0);
    assert_eq!(stamp_of(reopened, "tie-b").await, 1);
    assert_eq!(stamp_of(reopened, "warm").await, 2);
    assert_eq!(clock_of(reopened).await, 3);
    let tie_b = raw_row(reopened, "tie-b").await;
    assert!(tie_b.get("last_visit_time").is_none());
    assert!(tie_b.get("visit_count").is_none());
    assert!(tie_b.get("created_time").is_none());
    assert_eq!(
        raw_row(reopened, "tie-a").await["data"],
        serde_json::json!({"nested": [1, 2]})
    );
    let scope = reopened.scope(TransactionMode::ReadOnly).unwrap();
    assert_eq!(
        scope.rows.index_names(),
        vec![ACCESS_STAMP_INDEX.to_owned()]
    );
}

/// A version-1 database is upgraded in place: no row is lost, the legacy eviction order
/// (ties broken by key) becomes strict stamps, and eviction continues from it.
#[wasm_bindgen_test]
async fn opening_a_version_one_database_migrates_rows_in_lru_order() {
    let name = format!("rings-idb-legacy-test-{}", uuid::Uuid::new_v4());
    seed_legacy_database(&name, None, &legacy_rows()).await;

    let reopened = IdbStorage::new_with_cap_and_name(3, &name).await.unwrap();
    assert_migrated(&reopened).await;
    drop(reopened);

    // Reopening at the current schema is a no-op for stamps and clock.
    let reopened = IdbStorage::new_with_cap_and_name(3, &name).await.unwrap();
    assert_migrated(&reopened).await;

    // The legacy-oldest row is the first eviction candidate.
    reopened.put("new", &"new".to_owned()).await.unwrap();
    assert_eq!(get_string(&reopened, "tie-a").await.unwrap(), None);
    assert_eq!(stamp_of(&reopened, "new").await, 3);
    let tie_b = get_string(&reopened, "tie-b").await.unwrap();
    assert_eq!(tie_b.as_deref(), Some("tie-b"));
}

/// A migration interrupted after the upgrade reruns from the untouched legacy rows.
#[wasm_bindgen_test]
async fn interrupted_migration_reruns_on_next_open() {
    let name = format!("rings-idb-interrupted-test-{}", uuid::Uuid::new_v4());
    seed_legacy_database(&name, Some(SCHEMA_VERSION), &legacy_rows()).await;

    let reopened = IdbStorage::new_with_cap_and_name(3, &name).await.unwrap();
    assert_migrated(&reopened).await;
}

/// Migration completes before the row bound applies: a smaller capacity retires legacy-oldest rows.
#[wasm_bindgen_test]
async fn migrating_under_a_smaller_capacity_retires_legacy_oldest_rows() {
    let name = format!("rings-idb-legacy-cap-test-{}", uuid::Uuid::new_v4());
    seed_legacy_database(&name, None, &legacy_rows()).await;

    let reopened = IdbStorage::new_with_cap_and_name(1, &name).await.unwrap();
    assert_eq!(reopened.count().await.unwrap(), 1);
    assert_eq!(get_string(&reopened, "tie-a").await.unwrap(), None);
    assert_eq!(get_string(&reopened, "tie-b").await.unwrap(), None);
    let warm = get_string(&reopened, "warm").await.unwrap();
    assert_eq!(warm.as_deref(), Some("warm"));
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
    let value_a = get_string(&instance, "a").await.unwrap();
    let value_b = get_string(&instance, "b").await.unwrap();
    assert_eq!(value_a.as_deref(), Some("old-a"));
    assert_eq!(value_b.as_deref(), Some("new-b"));
}

/// A new key at capacity evicts the least recently accessed row in the same transaction.
#[wasm_bindgen_test]
async fn new_key_at_capacity_evicts_lru_row() {
    // The explicit access below makes a newer than b before the insertion of c.
    let instance = create_db_instance(2).await;
    instance.put("a", &"a".to_owned()).await.unwrap();
    instance.put("b", &"b".to_owned()).await.unwrap();
    let recently_accessed = get_string(&instance, "a").await.unwrap();
    assert_eq!(recently_accessed.as_deref(), Some("a"));

    // Accessing a makes b the least-recently-accessed row.
    instance.put("c", &"c".to_owned()).await.unwrap();
    assert_eq!(instance.count().await.unwrap(), 2);
    let value_a = get_string(&instance, "a").await.unwrap();
    let value_b = get_string(&instance, "b").await.unwrap();
    let value_c = get_string(&instance, "c").await.unwrap();
    assert_eq!(value_a.as_deref(), Some("a"));
    assert_eq!(value_b, None);
    assert_eq!(value_c.as_deref(), Some("c"));
}

/// Reopening with a smaller row budget removes every excess row, not just one candidate.
#[wasm_bindgen_test]
async fn reopening_with_smaller_capacity_removes_all_excess_rows() {
    // Four back-to-back writes receive the stamps 0‥3 regardless of the browser's timer.
    let name = format!("rings-idb-lowered-cap-test-{}", uuid::Uuid::new_v4());
    let initial = IdbStorage::new_with_cap_and_name(4, &name).await.unwrap();
    for key in ["a", "b", "c", "d"] {
        initial.put(key, &key.to_owned()).await.unwrap();
    }
    drop(initial);

    // Reopening at capacity two must keep the two most recently written rows only.
    let reopened = IdbStorage::new_with_cap_and_name(2, &name).await.unwrap();
    assert_eq!(reopened.count().await.unwrap(), 2);
    let value_a = get_string(&reopened, "a").await.unwrap();
    let value_b = get_string(&reopened, "b").await.unwrap();
    let value_c = get_string(&reopened, "c").await.unwrap();
    let value_d = get_string(&reopened, "d").await.unwrap();
    assert_eq!(value_a, None);
    assert_eq!(value_b, None);
    assert_eq!(value_c.as_deref(), Some("c"));
    assert_eq!(value_d.as_deref(), Some("d"));
}

/// Concurrent puts serialize their row-budget decisions through IndexedDB transactions.
#[wasm_bindgen_test]
async fn concurrent_puts_never_exceed_row_capacity() {
    // The same isolated two-row database receives simultaneous first writes.
    let instance = create_db_instance(2).await;
    // Keep payloads alive until join finishes polling the four borrowed put futures.
    let [a_value, b_value, c_value, d_value] = [
        String::from("a"),
        String::from("b"),
        String::from("c"),
        String::from("d"),
    ];
    let (first, second, third, fourth) = futures::join!(
        instance.put("a", &a_value),
        instance.put("b", &b_value),
        instance.put("c", &c_value),
        instance.put("d", &d_value),
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
    let value_a = get_string(&instance, "a").await.unwrap();
    let value_b = get_string(&instance, "b").await.unwrap();
    assert_eq!(value_a.as_deref(), Some("a"));
    assert_eq!(value_b.as_deref(), Some("b"));
}
