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
