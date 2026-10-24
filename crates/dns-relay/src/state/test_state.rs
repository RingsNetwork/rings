use std::collections::BTreeMap;

use super::*;

/// The journal at `journal.jsonl` in `dir`, and the map it replays.
async fn open(dir: &Path) -> (Journal, BTreeMap<String, u64>) {
    Journal::open(&dir.join("journal.jsonl")).await.unwrap()
}

#[tokio::test]
async fn test_replay_folds_puts_and_removes_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (mut journal, map) = open(dir.path()).await;
    assert!(map.is_empty(), "a missing journal is an empty map");
    journal.put(&"a".to_owned(), &1u64).await.unwrap();
    journal.put(&"b".to_owned(), &2u64).await.unwrap();
    journal.put(&"a".to_owned(), &3u64).await.unwrap();
    journal.remove(&"b".to_owned()).await.unwrap();
    let (_journal, map) = open(dir.path()).await;
    assert_eq!(map, BTreeMap::from([("a".to_owned(), 3)]));
}

#[tokio::test]
async fn test_replay_stops_at_a_torn_record() {
    let dir = tempfile::tempdir().unwrap();
    let (mut journal, _map) = open(dir.path()).await;
    journal.put(&"a".to_owned(), &1u64).await.unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"Put\":[\"b\",");
    std::fs::write(&path, text).unwrap();
    let (_journal, map) = open(dir.path()).await;
    assert_eq!(map, BTreeMap::from([("a".to_owned(), 1)]));
}

#[tokio::test]
async fn test_compaction_keeps_the_map_and_sheds_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let (mut journal, _map) = open(dir.path()).await;
    for round in 0..100u64 {
        journal.put(&"a".to_owned(), &round).await.unwrap();
    }
    assert!(journal.needs_compaction(1));
    journal.compact([("a".to_owned(), 99u64)]).await.unwrap();
    assert!(!journal.needs_compaction(1));
    let (_journal, map) = open(dir.path()).await;
    assert_eq!(map, BTreeMap::from([("a".to_owned(), 99)]));
    let lines = std::fs::read_to_string(dir.path().join("journal.jsonl"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(lines, 1);
}
