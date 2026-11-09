//! The release of a node's stores (#1085): the witnesses a close awaits.
//!
//! A native store is released when its last holder drops it, which unlocks its directory (#1039;
//! the release law of `rings_core::storage::file`). A browser node's storage namespace is
//! released when its last store drops, which resolves its exclusive Web Lock (`rings-node/<name>`,
//! #1055) and settles once the browser's `LockManager` has released it. A node keeps the release
//! witnesses of the stores it was built over, taken at open before each store is boxed behind its
//! trait, and [`StoresReleased`] resolves once each of them has.
//!
//! ```text
//!   StoresReleased(w₁ … wₙ) resolves  ⟺  ∀ i. wᵢ resolved   (n = 0 resolves at once)
//! ```

use std::fmt;
use std::future::IntoFuture;

#[cfg(any(rings_native, rings_browser))]
use futures::future::join_all;
use futures::future::BoxFuture;
#[cfg(any(rings_native, rings_browser))]
use futures::FutureExt;
#[cfg(rings_native)]
use rings_core::storage::file::StoreRelease;

#[cfg(rings_browser)]
use crate::provider::browser::provider::lock::BrowserLockRelease;

/// The release witnesses of a node's stores: it resolves once every store it names is released.
///
/// Cloning shares the witnesses.
#[derive(Clone, Default)]
pub struct StoresReleased {
    /// One witness per file store.
    #[cfg(rings_native)]
    stores: Vec<StoreRelease>,
    /// The witness of the browser origin lock (`rings-node/<storage_name>`, #1055).
    #[cfg(rings_browser)]
    stores: Vec<BrowserLockRelease>,
}

impl StoresReleased {
    /// The witnesses `stores`, one per file store.
    #[cfg(rings_native)]
    pub fn of(stores: impl IntoIterator<Item = StoreRelease>) -> Self {
        Self {
            stores: stores.into_iter().collect(),
        }
    }

    /// The witnesses `stores` of a browser node's storage lock (#1055).
    #[cfg(rings_browser)]
    pub fn of(stores: impl IntoIterator<Item = BrowserLockRelease>) -> Self {
        Self {
            stores: stores.into_iter().collect(),
        }
    }

    /// How many stores these witnesses name.
    pub fn len(&self) -> usize {
        #[cfg(any(rings_native, rings_browser))]
        {
            self.stores.len()
        }
        #[cfg(not(any(rings_native, rings_browser)))]
        {
            0
        }
    }

    /// Whether these witnesses name no store, so they resolve at once.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl IntoFuture for StoresReleased {
    type Output = ();
    type IntoFuture = BoxFuture<'static, ()>;

    /// Resolves once every store is released.
    fn into_future(self) -> Self::IntoFuture {
        #[cfg(any(rings_native, rings_browser))]
        {
            join_all(self.stores).map(drop).boxed()
        }
        #[cfg(not(any(rings_native, rings_browser)))]
        {
            Box::pin(std::future::ready(()))
        }
    }
}

impl fmt::Debug for StoresReleased {
    /// How many stores the witnesses name; whether they resolved is learned only by awaiting.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoresReleased")
            .field("stores", &self.len())
            .finish()
    }
}

#[cfg(all(test, rings_native))]
mod tests {
    use std::future::IntoFuture;

    use futures::FutureExt;
    use rings_core::storage::file::FileStorage;
    use rings_core::storage::RecordAuthority;
    use uuid::Uuid;

    use super::StoresReleased;

    #[tokio::test]
    async fn test_stores_released_empty_and_partial_vs_full_resolution() {
        // Boundary `n = 0`: `default()` and `of([])` report `len() == 0`, `is_empty()`, and
        // resolve immediately on first poll.
        let empty = StoresReleased::default();
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
        assert_eq!(format!("{empty:?}"), "StoresReleased { stores: 0 }");
        assert_eq!(empty.into_future().now_or_never(), Some(()));
        assert_eq!(
            StoresReleased::of([]).into_future().now_or_never(),
            Some(())
        );

        // Partial vs. full resolution (`n = 2`): remains `Pending` while any store stays live, and
        // resolves (along with clones) once all stores drop.
        let root = std::env::temp_dir().join(format!("rings-stores-released-{}", Uuid::new_v4()));
        let first = FileStorage::new_with_cap_path_and_authority(
            64,
            root.join("first"),
            RecordAuthority::DurableCache,
        )
        .await
        .expect("open first store");
        let second = FileStorage::new_with_cap_path_and_authority(
            64,
            root.join("second"),
            RecordAuthority::DurableCache,
        )
        .await
        .expect("open second store");

        let released = StoresReleased::of([first.released(), second.released()]);
        let cloned = released.clone();
        assert_eq!(released.len(), 2);
        assert!(!released.is_empty());
        assert_eq!(format!("{released:?}"), "StoresReleased { stores: 2 }");

        let mut fut = released.into_future();
        assert!(futures::poll!(&mut fut).is_pending());

        // Dropping only 1 of 2 stores must leave `StoresReleased` pending.
        drop(first);
        assert!(futures::poll!(&mut fut).is_pending());

        // Dropping the 2nd store resolves both the in-flight future and any cloned witness.
        drop(second);
        assert_eq!(fut.now_or_never(), Some(()));
        assert_eq!(cloned.into_future().now_or_never(), Some(()));

        let _ = std::fs::remove_dir_all(root);
    }
}
