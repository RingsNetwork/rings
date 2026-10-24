use std::collections::BTreeMap;
use std::sync::Mutex as SyncMutex;

use super::*;
use crate::owner::test_owner::TestOwner;

/// A directory whose answers the test sets.
#[derive(Default)]
struct Settable(SyncMutex<BTreeMap<TestOwner, Announcement>>);

impl Settable {
    /// Make `owner` announce `announcement`, or nothing when `None`.
    fn set(&self, owner: TestOwner, announcement: Option<Announcement>) {
        let mut answers = self.0.lock().unwrap();
        match announcement {
            Some(announcement) => answers.insert(owner, announcement),
            None => answers.remove(&owner),
        };
    }
}

#[async_trait]
impl Directory for Settable {
    type Owner = TestOwner;

    async fn announcement(&self, owner: TestOwner) -> Result<Option<Announcement>, LookupFailure> {
        Ok(self.0.lock().unwrap().get(&owner).cloned())
    }
}

/// An owner receiving mail, with a TXT record.
fn mail() -> Announcement {
    Announcement::new([(25, ServiceName::new("smtp").unwrap())], [(
        SubName::new("_acme-challenge").unwrap(),
        vec![TxtValue::new("token").unwrap()],
    )])
    .unwrap()
}

#[tokio::test]
async fn test_a_vanished_owner_is_remembered_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let directory = Arc::new(Settable::default());
    let grace = Duration::from_secs(3_600);
    let remembering = Remembering::open(Arc::clone(&directory), dir.path(), grace)
        .await
        .unwrap();
    directory.set(TestOwner(1), Some(mail()));
    assert_eq!(
        remembering.announcement(TestOwner(1)).await,
        Ok(Some(mail()))
    );
    drop(remembering);

    directory.set(TestOwner(1), None);
    let restarted = Remembering::open(Arc::clone(&directory), dir.path(), grace)
        .await
        .unwrap();
    assert_eq!(restarted.announcement(TestOwner(1)).await, Ok(Some(mail())));
    assert_eq!(restarted.announcement(TestOwner(2)).await, Ok(None));
}

#[tokio::test]
async fn test_a_withdrawal_is_forgotten_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let directory = Arc::new(Settable::default());
    let grace = Duration::from_secs(3_600);
    let remembering = Remembering::open(Arc::clone(&directory), dir.path(), grace)
        .await
        .unwrap();
    directory.set(TestOwner(1), Some(mail()));
    remembering.announcement(TestOwner(1)).await.unwrap();
    directory.set(TestOwner(1), Some(Announcement::default()));
    assert_eq!(remembering.announcement(TestOwner(1)).await, Ok(None));
    drop(remembering);

    directory.set(TestOwner(1), None);
    let restarted = Remembering::open(Arc::clone(&directory), dir.path(), grace)
        .await
        .unwrap();
    assert_eq!(restarted.announcement(TestOwner(1)).await, Ok(None));
}

#[tokio::test]
async fn test_a_malformed_memory_entry_is_never_answered() {
    let dir = tempfile::tempdir().unwrap();
    let file = MemoryFile {
        owners: vec![
            MemoryEntry {
                owner: "o1".to_owned(),
                ports: vec![(0, "smtp".to_owned())],
                txt: vec![],
                seen_at_ms: now_ms(),
            },
            MemoryEntry {
                owner: "not an owner".to_owned(),
                ports: vec![(25, "smtp".to_owned())],
                txt: vec![],
                seen_at_ms: now_ms(),
            },
        ],
    };
    write_json(&dir.path().join(MEMORY_FILE), &file)
        .await
        .unwrap();
    let remembering = Remembering::open(
        Arc::new(Settable::default()),
        dir.path(),
        Duration::from_secs(3_600),
    )
    .await
    .unwrap();
    assert_eq!(remembering.announcement(TestOwner(1)).await, Ok(None));
}
