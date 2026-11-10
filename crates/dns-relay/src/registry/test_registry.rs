use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use ipnet::Ipv6Net;

use super::*;
use crate::address::ADDRESS_KEY_LEN;
use crate::owner::test_owner::TestOwner;

/// The prefix of every case.
fn prefix() -> Ipv6Net {
    "2001:db8:1::/64".parse().unwrap()
}

/// The registry in `dir`, keeping bindings for `retention`, at most `capacity` of them.
async fn open(dir: &Path, retention: Duration, capacity: usize) -> AddressRegistry<TestOwner> {
    AddressRegistry::open(prefix(), dir, retention, capacity)
        .await
        .unwrap()
}

#[tokio::test]
async fn test_open_creates_a_private_key_that_reopening_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let first = open(dir.path(), Duration::from_secs(60), 8).await;
    let address = first.address_for(TestOwner(7), 1_000).await.unwrap();
    let key = std::fs::metadata(dir.path().join(KEY_FILE)).unwrap();
    assert_eq!(key.len(), u64::try_from(ADDRESS_KEY_LEN).unwrap());
    assert_eq!(key.permissions().mode() & 0o777, 0o600);
    drop(first);
    let reopened = open(dir.path(), Duration::from_secs(60), 8).await;
    assert_eq!(reopened.owner_at(&address).await, Some(TestOwner(7)));
    assert_eq!(
        reopened.address_for(TestOwner(7), 2_000).await.unwrap(),
        address
    );
}

#[tokio::test]
async fn test_a_key_of_the_wrong_length_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(KEY_FILE), [0u8; 3]).unwrap();
    let opened = AddressRegistry::<TestOwner>::open(prefix(), dir.path(), Duration::ZERO, 8).await;
    assert!(matches!(opened, Err(StateError::KeyLength(_))));
}

#[tokio::test]
async fn test_replay_drops_bindings_the_registry_could_not_have_made() {
    let dir = tempfile::tempdir().unwrap();
    let registry = open(dir.path(), Duration::from_secs(60), 8).await;
    let address = registry.address_for(TestOwner(7), 1_000).await.unwrap();
    drop(registry);
    // Rebind the address to an owner it is not the address of, and to no owner at all.
    let journal = format!(
        "{{\"Put\":[\"{address}\",{{\"owner\":\"o8\",\"answered_at_ms\":1000}}]}}\n\
         {{\"Put\":[\"{address}\",{{\"owner\":\"not an owner\",\"answered_at_ms\":1000}}]}}\n"
    );
    std::fs::write(dir.path().join(BOOK_FILE), journal).unwrap();
    let reopened = open(dir.path(), Duration::from_secs(60), 8).await;
    assert_eq!(reopened.owner_at(&address).await, None);
}

#[tokio::test]
async fn test_a_full_registry_forgets_the_least_recently_answered_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let registry = open(dir.path(), Duration::from_secs(600), 2).await;
    let first = registry.address_for(TestOwner(1), 1_000).await.unwrap();
    let second = registry.address_for(TestOwner(2), 1_050).await.unwrap();
    let third = registry.address_for(TestOwner(3), 1_100).await.unwrap();
    assert_eq!(registry.owner_at(&first).await, None);
    drop(registry);
    let reopened = open(dir.path(), Duration::from_secs(600), 2).await;
    assert_eq!(reopened.owner_at(&first).await, None);
    assert_eq!(reopened.owner_at(&second).await, Some(TestOwner(2)));
    assert_eq!(reopened.owner_at(&third).await, Some(TestOwner(3)));
}

#[tokio::test]
async fn test_a_failed_journal_write_leaves_in_memory_bindings_unchanged_and_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let book_path = dir.path().join(BOOK_FILE);
    let registry = open(dir.path(), Duration::from_millis(800), 1).await;

    let first = registry.address_for(TestOwner(1), 1_000).await.unwrap();
    assert_eq!(registry.owner_at(&first).await, Some(TestOwner(1)));

    // Make the journal path unwritable (`EISDIR`) before binding a second owner at capacity 1.
    let saved_journal = std::fs::read(&book_path).unwrap();
    std::fs::remove_file(&book_path).unwrap();
    std::fs::create_dir(&book_path).unwrap();

    let err = registry.address_for(TestOwner(2), 1_050).await.unwrap_err();
    assert!(matches!(err, AddressError::State(_)));
    assert_eq!(
        registry.owner_at(&first).await,
        Some(TestOwner(1)),
        "LRU victim must remain bound when journal persistence fails"
    );

    // Restore journal IO and retry at the same timestamp: the binding must persist and survive
    // reopening.
    std::fs::remove_dir(&book_path).unwrap();
    std::fs::write(&book_path, &saved_journal).unwrap();

    let second = registry.address_for(TestOwner(2), 1_050).await.unwrap();
    assert_eq!(registry.owner_at(&first).await, None);
    assert_eq!(registry.owner_at(&second).await, Some(TestOwner(2)));

    // Also verify a failed refresh past `refresh_ms` (100ms) does not advance `persisted_at_ms`.
    let saved_after_second = std::fs::read(&book_path).unwrap();
    std::fs::remove_file(&book_path).unwrap();
    std::fs::create_dir(&book_path).unwrap();
    assert!(matches!(
        registry.address_for(TestOwner(2), 1_200).await,
        Err(AddressError::State(_))
    ));
    std::fs::remove_dir(&book_path).unwrap();
    std::fs::write(&book_path, &saved_after_second).unwrap();

    // Retry the refresh at 1_200, then bind a check after reopen past 1_050 + 800 to confirm
    // `answered_at_ms = 1_200` reached the journal.
    assert_eq!(
        registry.address_for(TestOwner(2), 1_200).await.unwrap(),
        second
    );
    drop(registry);

    let reopened = open(dir.path(), Duration::from_millis(800), 1).await;
    assert_eq!(reopened.owner_at(&first).await, None);
    assert_eq!(reopened.owner_at(&second).await, Some(TestOwner(2)));
    let journal_text = std::fs::read_to_string(&book_path).unwrap();
    assert!(
        journal_text.contains("\"answered_at_ms\":1200"),
        "refreshed timestamp must be persisted on retry: {journal_text}"
    );
}

#[tokio::test]
async fn test_cancelled_binding_does_not_forget_a_later_successful_answer_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let book_path = dir.path().join(BOOK_FILE);
    let registry = open(dir.path(), Duration::from_secs(600), 1).await;

    // Step 1: bind owner 1 at t=1000.
    let first = registry.address_for(TestOwner(1), 1_000).await.unwrap();
    let len_after_first = std::fs::metadata(&book_path).unwrap().len();
    assert!(len_after_first > 0);

    // Step 2: start binding owner 2 at t=1050 (which plans `[Unbind(first), Bind(second)]`) and
    // poll until its journal write lands on disk, then cancel before `book.commit(planned)` runs.
    {
        let mut fut = Box::pin(registry.address_for(TestOwner(2), 1_050));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        loop {
            match std::future::Future::poll(fut.as_mut(), &mut cx) {
                std::task::Poll::Ready(_) => {
                    break;
                }
                std::task::Poll::Pending => {
                    let current_len = std::fs::metadata(&book_path).unwrap().len();
                    if current_len > len_after_first {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "timed out waiting for journal write to reach disk before commit"
                    );
                    tokio::task::yield_now().await;
                }
            }
        }
        drop(fut);
    }

    // Step 3: answer owner 1 again within its refresh window (producing `planned.changes() == []`).
    assert_eq!(
        registry.address_for(TestOwner(1), 1_100).await.unwrap(),
        first
    );
    assert_eq!(registry.owner_at(&first).await, Some(TestOwner(1)));
    drop(registry);

    // Step 4: reopen the registry from disk; owner 1 must still be bound.
    let reopened = open(dir.path(), Duration::from_secs(600), 1).await;
    assert_eq!(reopened.owner_at(&first).await, Some(TestOwner(1)));
}
