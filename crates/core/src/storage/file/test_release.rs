//! The release law (#1085): a store's witness resolves once its last holder drops it, its
//! directory unlocked, and not before; an operation still running holds the store.

use std::sync::Arc;

use futures::FutureExt;

use super::test_root::TempRoot;
use super::FileStorage;
use crate::error::Error;
use crate::storage::RecordAuthority;

/// Open the store at `root`, authoritative as a replay store is.
async fn open(root: &std::path::Path) -> crate::error::Result<FileStorage> {
    FileStorage::new_with_cap_path_and_authority(4096, root, RecordAuthority::Authoritative).await
}

/// While the handle lives the witness is pending; once the handle drops it resolves, and the
/// directory opens again.
#[tokio::test]
async fn test_a_store_is_released_when_its_handle_drops() -> crate::error::Result<()> {
    let root = TempRoot::new("file-release-handle");
    let store = open(&root).await?;
    let mut released = store.released();
    assert_eq!((&mut released).now_or_never(), None);

    drop(store);

    released.await;
    open(&root).await.map(drop)
}

/// An operation still running holds the store past its handle: the witness stays pending and
/// the directory stays locked until the operation ends, and every clone of the witness resolves
/// then.
#[tokio::test]
async fn test_an_operation_in_flight_holds_the_store_past_its_handle() -> crate::error::Result<()> {
    let root = TempRoot::new("file-release-in-flight");
    let store = open(&root).await?;
    let released = store.released();
    let mut also_released = released.clone();
    // The share of the store an operation on the blocking pool holds until it ends.
    let in_flight = Arc::clone(&store.store);

    drop(store);

    assert_eq!((&mut also_released).now_or_never(), None);
    assert!(matches!(
        open(&root).await,
        Err(Error::StorageDirectoryLocked(path)) if path == *root
    ));
    drop(in_flight);
    released.await;
    also_released.await;
    open(&root).await.map(drop)
}
