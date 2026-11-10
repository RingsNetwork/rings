//! Bounded per-peer asynchronous overlay outbox for protocol shells (`JsShell`, etc.) that
//! must not hold `Runner::transition_gate` across network sends (#1231).

use std::collections::HashMap;
#[cfg(all(test, rings_native))]
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::dht::Did;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::peer_quota::PeerQuota;
use crate::sync_lock::lock;
#[cfg(all(test, rings_native))]
use crate::test_support::BlockingSendProbe;

const MAX_PENDING_OVERLAY_SENDS: usize = 256;
const MAX_PENDING_OVERLAY_SENDS_PER_PEER: usize = 32;

struct OutboxState {
    quota: PeerQuota,
    lanes: HashMap<Did, VecDeque<Bytes>>,
}

impl OutboxState {
    fn new(max_total: usize, max_per_peer: usize) -> Self {
        Self {
            quota: PeerQuota::new(max_total, max_per_peer),
            lanes: HashMap::new(),
        }
    }
}

/// Cleans up a peer's active lane and any in-flight/queued permits if `NodeTasks::stop` drops
/// the drain worker mid-send.
struct PeerLaneGuard {
    state: Arc<Mutex<OutboxState>>,
    peer: Did,
    active: bool,
}

impl Drop for PeerLaneGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut state) = lock(self.state.as_ref()) {
            state.lanes.remove(&self.peer);
            let _ = state.quota.release_peer(self.peer);
        }
    }
}

#[cfg(all(test, rings_native))]
#[derive(Default)]
pub(crate) struct OverlayOutboxTestHook {
    blocking: BlockingSendProbe<Did>,
    delivered: Mutex<Vec<(Did, Bytes)>>,
    completed_peers: Mutex<HashSet<Did>>,
    completion: tokio::sync::Notify,
}

#[cfg(all(test, rings_native))]
impl OverlayOutboxTestHook {
    async fn before_send(&self, peer: Did) -> Result<()> {
        self.blocking.block_key(peer).await
    }

    fn record_attempt(&self, peer: Did, payload: Bytes) -> Result<()> {
        lock(&self.delivered)?.push((peer, payload));
        lock(&self.completed_peers)?.insert(peer);
        self.completion.notify_waiters();
        Ok(())
    }

    pub(crate) async fn wait_until_blocked(&self) {
        self.blocking.wait_until_blocked().await;
    }

    pub(crate) async fn wait_until_completed(&self, peer: Did) -> Result<()> {
        loop {
            let notified = self.completion.notified();
            if lock(&self.completed_peers)?.contains(&peer) {
                return Ok(());
            }
            notified.await;
        }
    }

    pub(crate) fn delivered(&self) -> Result<Vec<(Did, Bytes)>> {
        Ok(lock(&self.delivered)?.clone())
    }

    pub(crate) fn release(&self) {
        self.blocking.release();
    }
}

/// Asynchronous per-peer FIFO overlay send queue that decouples outbound network I/O from a
/// protocol's `transition_gate`.
pub(crate) struct OverlayOutbox {
    state: Arc<Mutex<OutboxState>>,
    #[cfg(all(test, rings_native))]
    test_hook: Option<Arc<OverlayOutboxTestHook>>,
}

impl Default for OverlayOutbox {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayOutbox {
    /// Construct an outbox with default global and per-peer pending-send bounds.
    pub(crate) fn new() -> Self {
        Self::with_bounds(
            MAX_PENDING_OVERLAY_SENDS,
            MAX_PENDING_OVERLAY_SENDS_PER_PEER,
        )
    }

    pub(crate) fn with_bounds(max_total: usize, max_per_peer: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(OutboxState::new(max_total, max_per_peer))),
            #[cfg(all(test, rings_native))]
            test_hook: None,
        }
    }

    #[cfg(all(test, rings_native))]
    pub(crate) fn with_test_hook(
        max_total: usize,
        max_per_peer: usize,
        hook: Arc<OverlayOutboxTestHook>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(OutboxState::new(max_total, max_per_peer))),
            test_hook: Some(hook),
        }
    }

    /// Enqueue `payload` to `to` under `scope`, spawning a per-peer drain worker onto the node's
    /// task set when no worker is currently active for `to`.
    ///
    /// Law (Lock order): `OutboxState`'s mutex is a synchronous leaf never held across `.await`
    /// or task spawning; network sends run on the spawned node task outside `transition_gate`.
    pub(crate) fn enqueue(&self, scope: Scope, to: Did, payload: Bytes) -> Result<()> {
        let spawner = scope.tasks().spawner()?;
        let spawn_worker = {
            let mut state = lock(self.state.as_ref())?;
            state.quota.reserve(to).map_err(|reason| {
                Error::ExtensionError(format!(
                    "overlay outbox for {:?} rejected peer {to}: {reason:?}",
                    scope.namespace()
                ))
            })?;
            if let Some(lane) = state.lanes.get_mut(&to) {
                lane.push_back(payload);
                false
            } else {
                let mut lane = VecDeque::new();
                lane.push_back(payload);
                state.lanes.insert(to, lane);
                true
            }
        };

        if spawn_worker {
            let state = Arc::clone(&self.state);
            #[cfg(all(test, rings_native))]
            let test_hook = self.test_hook.clone();
            spawner.spawn(async move {
                drain_peer_lane(
                    state,
                    scope,
                    to,
                    #[cfg(all(test, rings_native))]
                    test_hook,
                )
                .await;
            });
        }
        Ok(())
    }

    #[cfg(all(test, rings_native))]
    pub(crate) fn pending_total(&self) -> Result<usize> {
        Ok(lock(self.state.as_ref())?.quota.total())
    }
}

async fn drain_peer_lane(
    state: Arc<Mutex<OutboxState>>,
    scope: Scope,
    peer: Did,
    #[cfg(all(test, rings_native))] test_hook: Option<Arc<OverlayOutboxTestHook>>,
) {
    let mut guard = PeerLaneGuard {
        state: Arc::clone(&state),
        peer,
        active: true,
    };

    let mut release_previous = false;
    loop {
        let next_payload = {
            let Ok(mut locked) = lock(state.as_ref()) else {
                return;
            };
            if release_previous {
                let _ = locked.quota.release(peer);
            }
            match locked.lanes.get_mut(&peer).and_then(VecDeque::pop_front) {
                Some(payload) => payload,
                None => {
                    locked.lanes.remove(&peer);
                    guard.active = false;
                    return;
                }
            }
        };

        #[cfg(all(test, rings_native))]
        if let Some(hook) = test_hook.as_ref() {
            if let Err(error) = hook.before_send(peer).await {
                tracing::debug!(?error, "overlay outbox test hook failed");
            }
        }

        if let Err(error) = scope.send(peer, next_payload.clone()).await {
            tracing::debug!(
                namespace = %scope.namespace(),
                %peer,
                ?error,
                "async extension overlay send failed"
            );
        }

        #[cfg(all(test, rings_native))]
        if let Some(hook) = test_hook.as_ref() {
            if let Err(error) = hook.record_attempt(peer, next_payload) {
                tracing::debug!(?error, "overlay outbox test hook failed");
            }
        }

        release_previous = true;
    }
}

#[cfg(all(test, rings_native))]
mod tests {
    use rings_core::delegation::DelegateeKey;
    use rings_core::ecc::SecretKey;

    use super::*;
    use crate::test_support::test_scope;
    use crate::test_support::without_runtime;

    #[test]
    fn test_enqueue_without_runtime_claims_no_quota() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let delegatee_key = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let scope = runtime.block_on(async { test_scope(delegatee_key, "js-test") })?;
        let outbox = OverlayOutbox::with_bounds(8, 2);
        let peer = Did::from(11_u32);

        let refused =
            without_runtime(|| outbox.enqueue(scope.clone(), peer, Bytes::from_static(b"msg")));
        assert!(matches!(refused, Err(Error::RuntimeUnavailable(_))));
        assert_eq!(outbox.pending_total()?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_stalled_peer_does_not_block_independent_peer_and_preserves_fifo() -> Result<()> {
        let delegatee_key = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let scope = test_scope(delegatee_key, "js-test")?;
        let hook = Arc::new(OverlayOutboxTestHook::default());
        let outbox = OverlayOutbox::with_test_hook(8, 2, Arc::clone(&hook));
        let stalled_peer: Did = SecretKey::random().address().into();
        let healthy_peer: Did = SecretKey::random().address().into();

        outbox.enqueue(scope.clone(), stalled_peer, Bytes::from_static(b"first"))?;
        tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
            .await
            .map_err(|_| Error::ExtensionError("stalled peer did not reach gate".to_string()))?;

        // Second message to the stalled peer queues in FIFO order; third exceeds per-peer bound.
        outbox.enqueue(scope.clone(), stalled_peer, Bytes::from_static(b"second"))?;
        assert!(outbox
            .enqueue(scope.clone(), stalled_peer, Bytes::from_static(b"overflow"))
            .is_err());

        // Healthy peer proceeds without waiting on stalled_peer.
        outbox.enqueue(scope, healthy_peer, Bytes::from_static(b"healthy"))?;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            hook.wait_until_completed(healthy_peer),
        )
        .await
        .map_err(|_| Error::ExtensionError("healthy peer was blocked".to_string()))??;

        hook.release();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            hook.wait_until_completed(stalled_peer),
        )
        .await
        .map_err(|_| Error::ExtensionError("stalled peer did not drain".to_string()))??;

        // Wait until the second queued payload on stalled_peer also drains.
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while outbox.pending_total().unwrap_or(1) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| Error::ExtensionError("outbox did not drain to zero".to_string()))?;

        let stalled_trace: Vec<Bytes> = hook
            .delivered()?
            .into_iter()
            .filter_map(|(peer, payload)| (peer == stalled_peer).then_some(payload))
            .collect();
        assert_eq!(stalled_trace, vec![
            Bytes::from_static(b"first"),
            Bytes::from_static(b"second"),
        ]);
        Ok(())
    }

    #[tokio::test]
    async fn test_node_stop_cancels_stalled_lane_and_reclaims_permits() -> Result<()> {
        let delegatee_key = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let scope = test_scope(delegatee_key, "js-stop")?;
        let hook = Arc::new(OverlayOutboxTestHook::default());
        let outbox = OverlayOutbox::with_test_hook(8, 4, Arc::clone(&hook));
        let stalled_peer: Did = SecretKey::random().address().into();

        outbox.enqueue(scope.clone(), stalled_peer, Bytes::from_static(b"one"))?;
        outbox.enqueue(scope.clone(), stalled_peer, Bytes::from_static(b"two"))?;
        tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
            .await
            .map_err(|_| Error::ExtensionError("stalled peer did not block".to_string()))?;
        assert_eq!(outbox.pending_total()?, 2);

        scope.tasks().stop();
        scope.tasks().drained().await;
        assert_eq!(outbox.pending_total()?, 0);
        Ok(())
    }
}
