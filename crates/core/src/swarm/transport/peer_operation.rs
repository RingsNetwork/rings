//! Per-peer operation locks: one async mutex per peer with a live operation, pruned once its
//! last lease is dropped.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_lock::Mutex as AsyncMutex;
use async_lock::MutexGuard as AsyncMutexGuard;

use crate::dht::Did;

/// One peer's operation lock, shared by the leases on it.
type PeerOperationLock = Arc<AsyncMutex<()>>;

/// The operation locks of the peers with a live lease. Law: a peer has an entry exactly while
/// some lease on it lives, so the map is bounded by the peers with an operation in progress.
#[derive(Default)]
pub(super) struct PeerOperationLocks {
    locks: Mutex<BTreeMap<Did, PeerOperationLock>>,
}

/// A claim on one peer's operation lock; dropping the last lease on a peer removes its lock.
pub(super) struct PeerOperationLease<'locks> {
    locks: &'locks PeerOperationLocks,
    peer: Did,
    operation: PeerOperationLock,
}

impl PeerOperationLease<'_> {
    /// Wait for the peer's operation lock.
    pub(super) async fn acquire(&self) -> AsyncMutexGuard<'_, ()> {
        self.operation.lock().await
    }
}

impl Drop for PeerOperationLease<'_> {
    fn drop(&mut self) {
        self.locks.prune(self.peer, &self.operation);
    }
}

impl PeerOperationLocks {
    /// No peer locked.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// The map; every critical section leaves it consistent, so a poisoned lock is still valid.
    fn lock_map(&self) -> MutexGuard<'_, BTreeMap<Did, PeerOperationLock>> {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A lease on `peer`'s operation lock, created if no lease on it lives.
    pub(super) fn lease(&self, peer: Did) -> PeerOperationLease<'_> {
        let operation = self
            .lock_map()
            .entry(peer)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        PeerOperationLease {
            locks: self,
            peer,
            operation,
        }
    }

    /// Remove `peer`'s lock if `operation` is it and only the map and the dropping lease hold it.
    fn prune(&self, peer: Did, operation: &PeerOperationLock) {
        let mut locks = self.lock_map();
        if locks.get(&peer).is_some_and(|current| {
            Arc::ptr_eq(current, operation) && Arc::strong_count(current) <= 2
        }) {
            locks.remove(&peer);
        }
    }

    /// How many peers hold a lock.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock_map().len()
    }
}

#[cfg(test)]
mod tests {
    use super::PeerOperationLocks;
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
