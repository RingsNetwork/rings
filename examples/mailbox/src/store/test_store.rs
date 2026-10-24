use super::*;

/// The largest message the stores of these cases keep.
const MAX: usize = 1024;

/// A UID.
fn uid(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

#[tokio::test]
async fn test_a_new_store_holds_the_default_mailboxes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), MAX).await.unwrap();
    let names = store
        .list()
        .await
        .into_iter()
        .map(|listed| listed.name)
        .collect::<Vec<_>>();
    assert_eq!(names, [
        "INBOX", "Sent", "Drafts", "Trash", "Junk", "Archive"
    ]);
    let (sent, _) = store.mailbox("Sent").await.unwrap();
    assert_eq!(sent.special_use, Some(SpecialUse::Sent));
    assert!(
        store.mailbox("inbox").await.is_ok(),
        "the inbox's name ignores case"
    );
}

#[tokio::test]
async fn test_appended_messages_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), MAX).await.unwrap();
    let (validity, first) = store
        .append(INBOX, b"one", BTreeSet::new(), 10)
        .await
        .unwrap();
    store
        .append(INBOX, b"two", BTreeSet::from([Flag::Seen]), 20)
        .await
        .unwrap();
    drop(store);
    let store = Store::open(dir.path(), MAX).await.unwrap();
    let (listed, inbox) = store.mailbox(INBOX).await.unwrap();
    assert_eq!(inbox.uid_validity, validity);
    assert_eq!(inbox.messages.len(), 2);
    assert_eq!(store.read(listed.id, first).await.unwrap(), b"one");
    assert_eq!(
        inbox.message(uid(2)).map(|m| m.flags.clone()),
        Some(BTreeSet::from([Flag::Seen]))
    );
}

#[tokio::test]
async fn test_a_message_above_the_limit_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), 4).await.unwrap();
    assert!(matches!(
        store.append(INBOX, b"too big", BTreeSet::new(), 0).await,
        Err(StoreError::TooLarge(7))
    ));
    assert!(store.mailbox(INBOX).await.unwrap().1.messages.is_empty());
}

#[tokio::test]
async fn test_copy_keeps_flags_and_expunge_removes_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), MAX).await.unwrap();
    store
        .append(INBOX, b"keep", BTreeSet::from([Flag::Flagged]), 5)
        .await
        .unwrap();
    let (inbox, _) = store.mailbox(INBOX).await.unwrap();
    let copied = store
        .copy(inbox.id, &BTreeSet::from([uid(1)]), "Archive")
        .await
        .unwrap();
    assert_eq!(copied.uids, vec![(uid(1), uid(1))]);
    let (archive, archived) = store.mailbox("Archive").await.unwrap();
    assert_eq!(
        archived.messages.first().map(|m| m.flags.clone()),
        Some(BTreeSet::from([Flag::Flagged]))
    );
    assert_eq!(store.read(archive.id, uid(1)).await.unwrap(), b"keep");

    store
        .change_flags(
            inbox.id,
            &BTreeSet::from([uid(1)]),
            FlagChange::Add,
            &BTreeSet::from([Flag::Deleted]),
        )
        .await
        .unwrap();
    assert_eq!(store.expunge(inbox.id, None).await.unwrap(), vec![uid(1)]);
    assert!(matches!(
        store.read(inbox.id, uid(1)).await,
        Err(StoreError::NoMessage(_))
    ));
}

#[tokio::test]
async fn test_mailboxes_are_created_renamed_and_deleted_but_not_the_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), MAX).await.unwrap();
    let id = store.create("Projects").await.unwrap();
    assert!(matches!(
        store.create("Projects").await,
        Err(StoreError::MailboxExists(_))
    ));
    store.rename("Projects", "Work").await.unwrap();
    let (work, _) = store.mailbox("Work").await.unwrap();
    assert_eq!(work.id, id, "a rename keeps the mailbox's identity");
    store.delete("Work").await.unwrap();
    assert!(matches!(
        store.mailbox("Work").await,
        Err(StoreError::NoMailbox(_))
    ));
    assert!(matches!(
        store.delete("inbox").await,
        Err(StoreError::Inbox)
    ));
    assert!(matches!(
        store.rename(INBOX, "Old").await,
        Err(StoreError::Inbox)
    ));
}

#[tokio::test]
async fn test_every_commit_is_announced_by_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), MAX).await.unwrap();
    let mut changes = store.watch();
    store.append(INBOX, b"x", BTreeSet::new(), 0).await.unwrap();
    let (inbox, _) = store.mailbox(INBOX).await.unwrap();
    assert_eq!(changes.recv().await.unwrap(), inbox.id);
}

#[test]
fn test_a_validity_is_the_later_of_the_least_and_the_time() {
    assert_eq!(uid_validity(5, 100).get(), 100);
    assert_eq!(uid_validity(500, 100).get(), 500);
    assert_eq!(uid_validity(0, -1).get(), 1, "never zero");
}

#[tokio::test]
async fn test_a_mailbox_made_again_at_once_gets_a_new_validity_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut seen = Vec::new();
    for _ in 0..3 {
        let store = Store::open(dir.path(), MAX).await.unwrap();
        store.create("Projects").await.unwrap();
        let (_, mailbox) = store.mailbox("Projects").await.unwrap();
        seen.push(mailbox.uid_validity);
        store.delete("Projects").await.unwrap();
    }
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{seen:?}");
}
