use std::time::Duration;

use ipnet::Ipv6Net;

use super::*;
use crate::address::ADDRESS_KEY_LEN;
use crate::owner::test_owner::TestOwner;

/// The prefix of every case.
fn prefix() -> Ipv6Net {
    "2001:db8:1::/64".parse().unwrap()
}

/// The registry in `dir`, keeping bindings for `retention`.
async fn open(dir: &Path, retention: Duration) -> AddressRegistry<TestOwner> {
    AddressRegistry::open(prefix(), dir, retention)
        .await
        .unwrap()
}

#[tokio::test]
async fn test_open_creates_a_private_key_that_reopening_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let first = open(dir.path(), Duration::from_secs(60)).await;
    let address = first.address_for(TestOwner(7), 1_000).await.unwrap();
    let key = std::fs::metadata(dir.path().join(KEY_FILE)).unwrap();
    assert_eq!(key.len(), u64::try_from(ADDRESS_KEY_LEN).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(key.permissions().mode() & 0o777, 0o600);
    }
    drop(first);
    let reopened = open(dir.path(), Duration::from_secs(60)).await;
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
    let opened = AddressRegistry::<TestOwner>::open(prefix(), dir.path(), Duration::ZERO).await;
    assert!(matches!(opened, Err(StateError::KeyLength(_))));
}

#[tokio::test]
async fn test_book_drops_entries_the_registry_could_not_have_made() {
    let dir = tempfile::tempdir().unwrap();
    let registry = open(dir.path(), Duration::from_secs(60)).await;
    let address = registry.address_for(TestOwner(7), 1_000).await.unwrap();
    drop(registry);
    let forged = BookFile {
        bindings: vec![
            BookEntry {
                address,
                owner: "o8".to_owned(),
                answered_at_ms: 1_000,
            },
            BookEntry {
                address,
                owner: "not an owner".to_owned(),
                answered_at_ms: 1_000,
            },
        ],
    };
    std::fs::write(
        dir.path().join(BOOK_FILE),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    let reopened = open(dir.path(), Duration::from_secs(60)).await;
    assert_eq!(reopened.owner_at(&address).await, None);
}

#[tokio::test]
async fn test_a_growing_book_forgets_bindings_past_retention() {
    let dir = tempfile::tempdir().unwrap();
    let registry = open(dir.path(), Duration::from_millis(100)).await;
    let old = registry.address_for(TestOwner(1), 1_000).await.unwrap();
    let kept = registry.address_for(TestOwner(2), 1_050).await.unwrap();
    let fresh = registry.address_for(TestOwner(3), 1_120).await.unwrap();
    assert_eq!(registry.owner_at(&old).await, None);
    assert_eq!(registry.owner_at(&kept).await, Some(TestOwner(2)));
    assert_eq!(registry.owner_at(&fresh).await, Some(TestOwner(3)));
    drop(registry);
    let reopened = open(dir.path(), Duration::from_millis(100)).await;
    assert_eq!(reopened.owner_at(&old).await, None);
    assert_eq!(reopened.owner_at(&kept).await, Some(TestOwner(2)));
}
