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
async fn test_replay_truncates_a_torn_trailing_record_and_keeps_later_appends() {
    for tail in [
        "{\"Put\":[\"b\",",
        "{\"Put\":[\"b\",\n",
        "{\"Put\":[\"b\",2]}",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut journal, _map) = open(dir.path()).await;
        journal.put(&"a".to_owned(), &1u64).await.unwrap();
        let path = dir.path().join("journal.jsonl");
        let valid = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{valid}{tail}")).unwrap();

        let (mut journal, map) = open(dir.path()).await;
        assert_eq!(map, BTreeMap::from([("a".to_owned(), 1)]), "tail={tail:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            valid,
            "torn tail must be truncated on open: tail={tail:?}"
        );

        journal.put(&"c".to_owned(), &3u64).await.unwrap();
        let (_journal, map) = open(dir.path()).await;
        assert_eq!(
            map,
            BTreeMap::from([("a".to_owned(), 1), ("c".to_owned(), 3)]),
            "subsequent appends must survive reopen: tail={tail:?}"
        );
    }
}

#[tokio::test]
async fn test_replay_truncates_a_torn_first_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    std::fs::write(&path, "{\"Put\":[\"a\",").unwrap();
    let (mut journal, map) = open(dir.path()).await;
    assert!(map.is_empty());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    journal.put(&"b".to_owned(), &2u64).await.unwrap();
    let (_journal, map) = open(dir.path()).await;
    assert_eq!(map, BTreeMap::from([("b".to_owned(), 2)]));
}

#[tokio::test]
async fn test_replay_defers_truncation_when_read_only_on_open() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let (mut journal, _map) = open(dir.path()).await;
    journal.put(&"a".to_owned(), &1u64).await.unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"Put\":[\"b\",");
    std::fs::write(&path, text).unwrap();

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let enforce_ro = std::fs::OpenOptions::new().write(true).open(&path).is_err();
    let (mut journal, map) = open(dir.path()).await;
    assert_eq!(map, BTreeMap::from([("a".to_owned(), 1)]));
    if enforce_ro {
        assert!(journal.pending_truncate.is_some());
    }

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    journal.put(&"c".to_owned(), &3u64).await.unwrap();
    let (_journal, map) = open(dir.path()).await;
    assert_eq!(
        map,
        BTreeMap::from([("a".to_owned(), 1), ("c".to_owned(), 3)])
    );
}

#[tokio::test]
async fn test_replay_rejects_corrupt_interior_lines() {
    for (contents, expected_line) in [
        (
            "{\"Put\":[\"a\",1]}\n{\"Put\":[\"corrupt\"\n{\"Put\":[\"c\",3]}\n",
            2usize,
        ),
        ("{\"Put\":[\"corrupt\"\n{\"Put\":[\"b\",", 1usize),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        std::fs::write(&path, contents).unwrap();
        let opened = Journal::open::<String, u64>(&path).await;
        assert!(
            matches!(opened, Err(StateError::Decode { line, .. }) if line == expected_line),
            "expected interior decode error at line {expected_line}, got: {}",
            opened
                .err()
                .map_or_else(|| "Ok".to_owned(), |e| e.to_string())
        );
    }
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
