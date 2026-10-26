//! The release of a node's stores (#1085): the witnesses a close awaits.
//!
//! A store is released when its last holder drops it, which unlocks its directory (#1039;
//! the release law of `rings_core::storage::file`). A node keeps the release witness of every
//! file store it was built over, taken at open before the store is boxed behind its trait, and
//! [`StoresReleased`] resolves once each of them has.
//!
//! ```text
//!   StoresReleased(w₁ … wₙ) resolves  ⟺  ∀ i. wᵢ resolved   (n = 0 resolves at once)
//! ```
//!
//! Browser stores have no directory to unlock: a browser node holds no witness.

use std::fmt;
use std::future::IntoFuture;

#[cfg(rings_native)]
use futures::future::join_all;
use futures::future::BoxFuture;
#[cfg(rings_native)]
use futures::FutureExt;
#[cfg(rings_native)]
use rings_core::storage::file::StoreRelease;

/// The release witnesses of a node's stores: it resolves once every store it names is released.
///
/// Cloning shares the witnesses.
#[derive(Clone, Default)]
pub struct StoresReleased {
    /// One witness per file store.
    #[cfg(rings_native)]
    stores: Vec<StoreRelease>,
}

impl StoresReleased {
    /// The witnesses `stores`, one per file store.
    #[cfg(rings_native)]
    pub fn of(stores: impl IntoIterator<Item = StoreRelease>) -> Self {
        Self {
            stores: stores.into_iter().collect(),
        }
    }

    /// How many stores these witnesses name.
    pub fn len(&self) -> usize {
        #[cfg(rings_native)]
        {
            self.stores.len()
        }
        #[cfg(not(rings_native))]
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
        #[cfg(rings_native)]
        {
            join_all(self.stores).map(drop).boxed()
        }
        #[cfg(not(rings_native))]
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
