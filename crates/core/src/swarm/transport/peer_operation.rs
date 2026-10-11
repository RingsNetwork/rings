//! Per-peer operation locks: one async mutex per peer with a live operation, pruned once its
//! last lease is dropped.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_lock::Mutex as AsyncMutex;
use async_lock::MutexGuard as AsyncMutexGuard;

use crate::dht::Did;

pub(super) type PeerOperationLock = Arc<AsyncMutex<()>>;

struct PeerArcRegistry<T> {
    entries: Mutex<BTreeMap<Did, Arc<T>>>,
}

impl<T> Default for PeerArcRegistry<T> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<T> PeerArcRegistry<T> {
    fn lock_map(&self) -> MutexGuard<'_, BTreeMap<Did, Arc<T>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn get_or_insert_with(&self, peer: Did, create: impl FnOnce() -> T) -> Arc<T> {
        self.lock_map()
            .entry(peer)
            .or_insert_with(|| Arc::new(create()))
            .clone()
    }

    fn prune(&self, peer: Did, value: &Arc<T>) {
        let mut entries = self.lock_map();
        if entries
            .get(&peer)
            .is_some_and(|current| Arc::ptr_eq(current, value) && Arc::strong_count(current) <= 2)
        {
            entries.remove(&peer);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock_map().len()
    }
}

#[derive(Default)]
pub(super) struct PeerOperationLocks {
    locks: PeerArcRegistry<AsyncMutex<()>>,
}

pub(super) struct PeerOperationLease<'locks> {
    locks: &'locks PeerOperationLocks,
    peer: Did,
    operation: PeerOperationLock,
}

impl PeerOperationLease<'_> {
    pub(super) async fn acquire(&self) -> AsyncMutexGuard<'_, ()> {
        self.operation.lock().await
    }
}

impl Drop for PeerOperationLease<'_> {
    fn drop(&mut self) {
        self.locks.prune_idle(self.peer, &self.operation);
    }
}

impl PeerOperationLocks {
    pub(super) fn new() -> Self {
        Self::default()
    }

    #[cfg(all(
        test,
        feature = "dummy",
        not(all(feature = "wasm", target_family = "wasm"))
    ))]
    pub(super) fn lock(&self, peer: Did) -> PeerOperationLock {
        self.locks.get_or_insert_with(peer, || AsyncMutex::new(()))
    }

    pub(super) fn lease(&self, peer: Did) -> PeerOperationLease<'_> {
        let operation = self.locks.get_or_insert_with(peer, || AsyncMutex::new(()));
        PeerOperationLease {
            locks: self,
            peer,
            operation,
        }
    }

    pub(super) fn prune_idle(&self, peer: Did, delivery: &PeerOperationLock) {
        self.locks.prune(peer, delivery);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.locks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    #[test]
    fn test_last_operation_lease_removes_the_peer_lock() {
        let locks = PeerOperationLocks::new();
        let peer = SecretKey::random().address().into();
        let first = locks.lease(peer);
        let second = locks.lease(peer);
        assert_eq!(locks.len(), 1);

        drop(first);
        assert_eq!(locks.len(), 1);
        drop(second);
        assert_eq!(locks.len(), 0);
    }
}
