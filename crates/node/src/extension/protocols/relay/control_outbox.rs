//! Fair off-gate execution for unordered relay terminal frames.

use std::collections::HashMap;
#[cfg(all(test, rings_native))]
use std::collections::HashSet;
#[cfg(all(test, rings_native))]
use std::sync::atomic::AtomicBool;
#[cfg(all(test, rings_native))]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::dht::Did;

use crate::error::OnionQueueAdmissionReason;
use crate::error::OnionQueueKind;
use crate::extension::ext::Scope;
use crate::extension::transport::platform::spawn_detached;

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
    claim_blocked_peer: AtomicBool,
    blocked: AtomicBool,
    released: AtomicBool,
    blocked_peer: Mutex<Option<Did>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completed: Mutex<HashSet<Did>>,
    completion: tokio::sync::Notify,
}

#[cfg(all(test, rings_native))]
impl Default for ControlSendTestHook {
    fn default() -> Self {
        Self {
            claim_blocked_peer: AtomicBool::new(true),
            blocked: AtomicBool::new(false),
            released: AtomicBool::new(false),
            blocked_peer: Mutex::new(None),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            completed: Mutex::new(HashSet::new()),
            completion: tokio::sync::Notify::new(),
        }
    }
}

#[cfg(all(test, rings_native))]
impl ControlSendTestHook {
    async fn before_send(&self, peer: Did) -> crate::error::Result<()> {
        if self.claim_blocked_peer.swap(false, Ordering::AcqRel) {
            *lock_state(&self.blocked_peer)? = Some(peer);
            self.blocked.store(true, Ordering::Release);
            self.entered.notify_one();
        }
        if lock_state(&self.blocked_peer)?.as_ref() != Some(&peer) {
            return Ok(());
        }
        while !self.released.load(Ordering::Acquire) {
            let released = self.release.notified();
            if self.released.load(Ordering::Acquire) {
                return Ok(());
            }
            released.await;
        }
        Ok(())
    }

    fn record_completed(&self, peer: Did) -> crate::error::Result<()> {
        lock_state(&self.completed)?.insert(peer);
        self.completion.notify_waiters();
        Ok(())
    }

    pub(crate) async fn wait_until_blocked(&self) {
        while !self.blocked.load(Ordering::Acquire) {
            let entered = self.entered.notified();
            if self.blocked.load(Ordering::Acquire) {
                return;
            }
            entered.await;
        }
    }

    pub(crate) async fn wait_until_completed(&self, peer: Did) -> crate::error::Result<()> {
        loop {
            let completion = self.completion.notified();
            if lock_state(&self.completed)?.contains(&peer) {
                return Ok(());
            }
            completion.await;
        }
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release.notify_waiters();
    }
}

/// Pure in-flight fairness account.
///
/// Invariant: `pending = sum(by_peer.values()) <= 64` and every peer count is in `1..=4`.
/// Preservation: admission computes both successor counts before committing either; release
/// decrements both and removes the zero-count peer projection.
#[derive(Default)]
struct ControlBudget {
    pending: usize,
    by_peer: HashMap<Did, usize>,
}

impl ControlBudget {
    fn reserve(&mut self, peer: Did) -> Result<(), OnionQueueAdmissionReason> {
        let peer_pending = self.by_peer.get(&peer).copied().unwrap_or_default();
        let next_peer = peer_pending
            .checked_add(1)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        let next_pending = self
            .pending
            .checked_add(1)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        if next_pending > MAX_PENDING_RELAY_CONTROL_SENDS {
            return Err(OnionQueueAdmissionReason::GlobalFull);
        }
        if next_peer > MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
            return Err(OnionQueueAdmissionReason::PeerFull);
        }
        self.pending = next_pending;
        self.by_peer.insert(peer, next_peer);
        Ok(())
    }

    fn release(&mut self, peer: Did) -> bool {
        let Some(peer_pending) = self.by_peer.get(&peer).copied() else {
            return false;
        };
        let Some(next_peer) = peer_pending.checked_sub(1) else {
            return false;
        };
        let Some(next_pending) = self.pending.checked_sub(1) else {
            return false;
        };
        self.pending = next_pending;
        if next_peer == 0 {
            self.by_peer.remove(&peer);
        } else {
            self.by_peer.insert(peer, next_peer);
        }
        true
    }
}

struct ControlPermit {
    budget: Arc<Mutex<ControlBudget>>,
    peer: Did,
}

impl ControlPermit {
    fn acquire(budget: Arc<Mutex<ControlBudget>>, peer: Did) -> crate::error::Result<Self> {
        lock_state(budget.as_ref())?
            .reserve(peer)
            .map_err(|reason| capacity_error(peer, reason))?;
        Ok(Self { budget, peer })
    }
}

impl Drop for ControlPermit {
    fn drop(&mut self) {
        let released = lock_state(self.budget.as_ref())
            .map(|mut budget| budget.release(self.peer))
            .unwrap_or(false);
        if !released {
            tracing::debug!(peer = %self.peer, "relay control-send permit cleanup failed");
        }
    }
}

#[derive(Default)]
pub(super) struct ControlOutbox {
    budget: Arc<Mutex<ControlBudget>>,
    #[cfg(all(test, rings_native))]
    test_hook: Option<Arc<ControlSendTestHook>>,
}

impl ControlOutbox {
    #[cfg(all(test, rings_native))]
    pub(super) fn with_test_hook(hook: Arc<ControlSendTestHook>) -> Self {
        Self {
            budget: Arc::new(Mutex::new(ControlBudget::default())),
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

fn lock_state<T>(slot: &Mutex<T>) -> crate::error::Result<std::sync::MutexGuard<'_, T>> {
    slot.lock().map_err(|_| crate::error::Error::Lock)
}

fn capacity_error(peer: Did, reason: OnionQueueAdmissionReason) -> crate::error::Error {
    crate::error::Error::OnionQueueAdmission {
        queue: OnionQueueKind::RelayControl,
        peer,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_peer_cannot_consume_another_peers_share() -> crate::error::Result<()> {
        let mut budget = ControlBudget::default();
        let busy_peer = Did::from(1_u32);
        let other_peer = Did::from(2_u32);
        for _ in 0..MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
            budget
                .reserve(busy_peer)
                .map_err(|reason| capacity_error(busy_peer, reason))?;
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
    fn global_budget_rejects_at_the_exact_bound_and_recovers() {
        let mut budget = ControlBudget::default();
        for peer_id in 1_u32..=16_u32 {
            let peer = Did::from(peer_id);
            for _ in 0..MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
                assert_eq!(budget.reserve(peer), Ok(()));
            }
        }
        assert_eq!(budget.pending, MAX_PENDING_RELAY_CONTROL_SENDS);
        let recovering_peer = Did::from(100_u32);
        assert_eq!(
            budget.reserve(recovering_peer),
            Err(OnionQueueAdmissionReason::GlobalFull)
        );
        assert!(budget.release(Did::from(1_u32)));
        assert_eq!(budget.reserve(recovering_peer), Ok(()));
        assert_eq!(budget.pending, MAX_PENDING_RELAY_CONTROL_SENDS);
    }

    #[test]
    fn permit_drop_reclaims_capacity() -> crate::error::Result<()> {
        let peer = Did::from(3_u32);
        let budget = Arc::new(Mutex::new(ControlBudget::default()));
        {
            let _permit = ControlPermit::acquire(Arc::clone(&budget), peer)?;
            assert_eq!(lock_state(budget.as_ref())?.pending, 1);
        }
        let budget = lock_state(budget.as_ref())?;
        assert_eq!(budget.pending, 0);
        assert!(!budget.by_peer.contains_key(&peer));
        Ok(())
    }
}
