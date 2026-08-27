//! Fair off-gate execution for unordered relay terminal frames.

#[cfg(all(test, rings_native))]
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::dht::Did;

#[cfg(test)]
use crate::error::OnionQueueAdmissionReason;
use crate::error::OnionQueueKind;
use crate::extension::ext::Scope;
use crate::extension::transport::platform::spawn_detached;
use crate::peer_quota::PeerQuota;
use crate::sync_lock::lock;
#[cfg(all(test, rings_native))]
use crate::test_support::BlockingSendProbe;

const MAX_PENDING_RELAY_CONTROL_SENDS: usize = 64;
const MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER: usize = 4;

struct ControlSend {
    scope: Scope,
    to: Did,
    payload: Bytes,
    #[cfg(all(test, rings_native))]
    test_hook: Option<Arc<ControlSendTestHook>>,
}

async fn apply_control_send(control: ControlSend) {
    #[cfg(all(test, rings_native))]
    if let Some(hook) = control.test_hook.as_ref() {
        if let Err(error) = hook.before_send(control.to).await {
            tracing::debug!(?error, "relay control-send test hook failed");
        }
    }
    if let Err(error) = control.scope.send(control.to, control.payload).await {
        tracing::debug!(peer = %control.to, ?error, "relay terminal control send failed");
    }
    #[cfg(all(test, rings_native))]
    if let Some(hook) = control.test_hook.as_ref() {
        if let Err(error) = hook.record_completed(control.to) {
            tracing::debug!(?error, "relay control-send test hook failed");
        }
    }
}

#[cfg(all(test, rings_native))]
pub(crate) struct ControlSendTestHook {
    blocking: BlockingSendProbe<Did>,
    completed: Mutex<HashSet<Did>>,
    completion: tokio::sync::Notify,
}

#[cfg(all(test, rings_native))]
impl Default for ControlSendTestHook {
    fn default() -> Self {
        Self {
            blocking: BlockingSendProbe::default(),
            completed: Mutex::new(HashSet::new()),
            completion: tokio::sync::Notify::new(),
        }
    }
}

#[cfg(all(test, rings_native))]
impl ControlSendTestHook {
    async fn before_send(&self, peer: Did) -> crate::error::Result<()> {
        self.blocking.block_key(peer).await
    }

    fn record_completed(&self, peer: Did) -> crate::error::Result<()> {
        lock(&self.completed)?.insert(peer);
        self.completion.notify_waiters();
        Ok(())
    }

    pub(crate) async fn wait_until_blocked(&self) {
        self.blocking.wait_until_blocked().await;
    }

    pub(crate) async fn wait_until_completed(&self, peer: Did) -> crate::error::Result<()> {
        loop {
            let completion = self.completion.notified();
            if lock(&self.completed)?.contains(&peer) {
                return Ok(());
            }
            completion.await;
        }
    }

    pub(crate) fn release(&self) {
        self.blocking.release();
    }
}

struct ControlPermit {
    budget: Arc<Mutex<PeerQuota>>,
    peer: Did,
}

impl ControlPermit {
    fn acquire(budget: Arc<Mutex<PeerQuota>>, peer: Did) -> crate::error::Result<Self> {
        lock(budget.as_ref())?
            .reserve(peer)
            .map_err(|reason| OnionQueueKind::RelayControl.admission(peer, reason))?;
        Ok(Self { budget, peer })
    }
}

impl Drop for ControlPermit {
    fn drop(&mut self) {
        let released = lock(self.budget.as_ref())
            .map(|mut budget| budget.release(self.peer))
            .unwrap_or(false);
        if !released {
            tracing::debug!(peer = %self.peer, "relay control-send permit cleanup failed");
        }
    }
}

pub(super) struct ControlOutbox {
    budget: Arc<Mutex<PeerQuota>>,
    #[cfg(all(test, rings_native))]
    test_hook: Option<Arc<ControlSendTestHook>>,
}

impl Default for ControlOutbox {
    fn default() -> Self {
        Self {
            budget: Arc::new(Mutex::new(PeerQuota::new(
                MAX_PENDING_RELAY_CONTROL_SENDS,
                MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER,
            ))),
            #[cfg(all(test, rings_native))]
            test_hook: None,
        }
    }
}

impl ControlOutbox {
    #[cfg(all(test, rings_native))]
    pub(super) fn with_test_hook(hook: Arc<ControlSendTestHook>) -> Self {
        Self {
            budget: Arc::new(Mutex::new(PeerQuota::new(
                MAX_PENDING_RELAY_CONTROL_SENDS,
                MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER,
            ))),
            test_hook: Some(hook),
        }
    }

    pub(super) fn enqueue(
        &self,
        scope: Scope,
        to: Did,
        payload: Bytes,
    ) -> crate::error::Result<()> {
        let permit = ControlPermit::acquire(Arc::clone(&self.budget), to)?;
        let send = ControlSend {
            scope,
            to,
            payload,
            #[cfg(all(test, rings_native))]
            test_hook: self.test_hook.clone(),
        };
        spawn_detached(async move {
            let _permit = permit;
            apply_control_send(send).await;
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_one_peer_cannot_consume_another_peers_share() -> crate::error::Result<()> {
        let mut budget = PeerQuota::new(
            MAX_PENDING_RELAY_CONTROL_SENDS,
            MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER,
        );
        let busy_peer = Did::from(1_u32);
        let other_peer = Did::from(2_u32);
        for _ in 0..MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
            budget
                .reserve(busy_peer)
                .map_err(|reason| OnionQueueKind::RelayControl.admission(busy_peer, reason))?;
        }
        assert_eq!(
            budget.reserve(busy_peer),
            Err(OnionQueueAdmissionReason::PeerFull)
        );
        assert_eq!(budget.reserve(other_peer), Ok(()));
        assert!(budget.release(other_peer));
        Ok(())
    }

    #[test]
    fn test_global_budget_rejects_at_the_exact_bound_and_recovers() {
        let mut budget = PeerQuota::new(
            MAX_PENDING_RELAY_CONTROL_SENDS,
            MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER,
        );
        for peer_id in 1_u32..=16_u32 {
            let peer = Did::from(peer_id);
            for _ in 0..MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
                assert_eq!(budget.reserve(peer), Ok(()));
            }
        }
        assert_eq!(budget.total(), MAX_PENDING_RELAY_CONTROL_SENDS);
        let recovering_peer = Did::from(100_u32);
        assert_eq!(
            budget.reserve(recovering_peer),
            Err(OnionQueueAdmissionReason::GlobalFull)
        );
        assert!(budget.release(Did::from(1_u32)));
        assert_eq!(budget.reserve(recovering_peer), Ok(()));
        assert_eq!(budget.total(), MAX_PENDING_RELAY_CONTROL_SENDS);
    }

    #[test]
    fn test_permit_drop_reclaims_capacity() -> crate::error::Result<()> {
        let peer = Did::from(3_u32);
        let budget = Arc::new(Mutex::new(PeerQuota::new(
            MAX_PENDING_RELAY_CONTROL_SENDS,
            MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER,
        )));
        {
            let _permit = ControlPermit::acquire(Arc::clone(&budget), peer)?;
            assert_eq!(lock(budget.as_ref())?.total(), 1);
        }
        let budget = lock(budget.as_ref())?;
        assert_eq!(budget.total(), 0);
        assert_eq!(budget.peer_total(peer), 0);
        Ok(())
    }
}
