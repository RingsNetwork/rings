use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_lock::Mutex as AsyncMutex;

use crate::dht::Did;

pub(super) type SwarmEventDeliveryLock = Arc<AsyncMutex<()>>;

#[derive(Default)]
pub(super) struct SwarmEventDeliveryLocks {
    locks: Mutex<BTreeMap<Did, SwarmEventDeliveryLock>>,
}

impl SwarmEventDeliveryLocks {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn lock(&self, peer: Did) -> SwarmEventDeliveryLock {
        match self.locks.lock() {
            Ok(mut locks) => locks
                .entry(peer)
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone(),
            Err(_) => {
                tracing::warn!("Failed to lock swarm event delivery map for peer {peer}");
                Arc::new(AsyncMutex::new(()))
            }
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
        let Ok(mut locks) = self.locks.lock() else {
            tracing::warn!("Failed to prune swarm event delivery lock for peer {peer}");
            return;
        };
        if locks.get(&peer).is_some_and(|current| {
            Arc::ptr_eq(current, delivery) && Arc::strong_count(current) <= 2
        }) {
            locks.remove(&peer);
        }
    }
}
