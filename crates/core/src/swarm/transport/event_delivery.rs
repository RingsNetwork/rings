use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_lock::Mutex as AsyncMutex;
use async_lock::MutexGuard as AsyncMutexGuard;

use crate::dht::Did;

pub(super) type SwarmEventDeliveryLock = Arc<AsyncMutex<()>>;
pub(super) type PeerOperationLock = SwarmEventDeliveryLock;

#[derive(Default)]
pub(super) struct PeerOperationLocks {
    locks: Mutex<BTreeMap<Did, SwarmEventDeliveryLock>>,
}

pub(super) type SwarmEventDeliveryLocks = PeerOperationLocks;

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

    fn lock_map(&self) -> MutexGuard<'_, BTreeMap<Did, SwarmEventDeliveryLock>> {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn lock(&self, peer: Did) -> SwarmEventDeliveryLock {
        self.lock_map()
            .entry(peer)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

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

    pub(super) fn prune(
        &self,
        peer: Did,
        delivery: &SwarmEventDeliveryLock,
        connection_epoch_exists: bool,
    ) {
        if connection_epoch_exists {
            return;
        }
        self.prune_idle(peer, delivery);
    }

    pub(super) fn prune_idle(&self, peer: Did, delivery: &PeerOperationLock) {
        let mut locks = self.lock_map();
        if locks.get(&peer).is_some_and(|current| {
            Arc::ptr_eq(current, delivery) && Arc::strong_count(current) <= 2
        }) {
            locks.remove(&peer);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock_map().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    #[test]
    fn last_operation_lease_removes_the_peer_lock() {
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
