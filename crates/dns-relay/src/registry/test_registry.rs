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
