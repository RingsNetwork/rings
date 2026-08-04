//! Ordered off-gate delivery for onion data-plane frames.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::VecDeque;
#[cfg(all(test, rings_native))]
use std::sync::atomic::AtomicBool;
#[cfg(all(test, rings_native))]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;

use crate::error::Error;
use crate::error::OnionQueueAdmissionReason;
use crate::error::OnionQueueKind;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::extension::transport::platform::spawn_detached;

const MAX_PENDING_ONION_SENDS: usize = 1_024;
const MAX_PENDING_ONION_SENDS_PER_PEER: usize = 128;
const MIN_ONION_SEND_JITTER_MS: u64 = 5;
const ONION_SEND_JITTER_SPAN_MS: u64 = 21;

struct OverlaySend {
    scope: Scope,
    payload: Bytes,
}

struct PeerLane<T> {
    in_flight: bool,
    queued: VecDeque<T>,
}

struct OrderedSendState<T> {
    pending: usize,
    lanes: HashMap<Did, PeerLane<T>>,
}

impl<T> Default for OrderedSendState<T> {
    fn default() -> Self {
        Self {
            pending: 0,
            lanes: HashMap::new(),
        }
    }
}

impl<T> OrderedSendState<T> {
    /// Reserve one frame and return whether its peer needs a new drain task.
    ///
    /// Invariant: `pending` equals queued frames plus one for every in-flight lane. No lane has
    /// more than one in-flight frame, which is the serialization witness for per-peer order.
    fn enqueue(
        &mut self,
        peer: Did,
        item: T,
    ) -> std::result::Result<bool, OnionQueueAdmissionReason> {
        let peer_pending = self
            .lanes
            .get(&peer)
            .map(|lane| lane.queued.len() + usize::from(lane.in_flight))
            .unwrap_or_default();
        let next_peer = peer_pending
            .checked_add(1)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        let next_pending = self
            .pending
            .checked_add(1)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        if next_pending > MAX_PENDING_ONION_SENDS {
            return Err(OnionQueueAdmissionReason::GlobalFull);
        }
        if next_peer > MAX_PENDING_ONION_SENDS_PER_PEER {
            return Err(OnionQueueAdmissionReason::PeerFull);
        }
        self.pending = next_pending;
        match self.lanes.entry(peer) {
            Entry::Occupied(mut lane) => {
                lane.get_mut().queued.push_back(item);
                Ok(false)
            }
            Entry::Vacant(lane) => {
                lane.insert(PeerLane {
                    in_flight: false,
                    queued: VecDeque::from([item]),
                });
                Ok(true)
            }
        }
    }

    fn take_next(&mut self, peer: Did) -> Option<T> {
        let lane = self.lanes.get_mut(&peer)?;
        if lane.in_flight {
            return None;
        }
        let item = lane.queued.pop_front()?;
        lane.in_flight = true;
        Some(item)
    }

    /// Complete one in-flight frame and return whether the lane has more work.
    fn complete(&mut self, peer: Did) -> Option<bool> {
        let lane = self.lanes.get_mut(&peer)?;
        if !lane.in_flight {
            return None;
        }
        let next_pending = self.pending.checked_sub(1)?;
        lane.in_flight = false;
        self.pending = next_pending;
        if lane.queued.is_empty() {
            self.lanes.remove(&peer);
            Some(false)
        } else {
            Some(true)
        }
    }
}

/// Per-next-hop ordered outbox. Enqueue is synchronous; overlay backpressure lives in drains.
#[derive(Default)]
pub(super) struct OnionSendOutbox {
    state: Arc<Mutex<OrderedSendState<OverlaySend>>>,
    #[cfg(all(test, rings_native))]
    test_hook: Option<Arc<OnionSendTestHook>>,
}

impl OnionSendOutbox {
    pub(super) fn enqueue(&self, scope: Scope, to: Did, payload: Bytes) -> Result<()> {
        let should_spawn = lock(&self.state)?
            .enqueue(to, OverlaySend { scope, payload })
            .map_err(|reason| capacity_error(to, reason))?;
        if should_spawn {
            let state = Arc::clone(&self.state);
            #[cfg(all(test, rings_native))]
            let test_hook = self.test_hook.clone();
            spawn_detached(async move {
                drain_peer(
                    state,
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
    pub(super) fn with_test_hook(test_hook: Arc<OnionSendTestHook>) -> Self {
        Self {
            state: Arc::default(),
            test_hook: Some(test_hook),
        }
    }
}

async fn drain_peer(
    state: Arc<Mutex<OrderedSendState<OverlaySend>>>,
    peer: Did,
    #[cfg(all(test, rings_native))] test_hook: Option<Arc<OnionSendTestHook>>,
) {
    loop {
        let Some(send) = lock(&state)
            .ok()
            .and_then(|mut state| state.take_next(peer))
        else {
            tracing::debug!(%peer, "onion send outbox lost drain ownership");
            return;
        };
        // One lane owns FIFO order, so bounded random delay changes only observable timing, never
        // the state-machine order. It weakens immediate one-for-one correlation without holding
        // the protocol transition gate or creating unbounded cover traffic.
        futures_timer::Delay::new(onion_send_jitter(rand::random())).await;
        #[cfg(all(test, rings_native))]
        if let Some(hook) = test_hook.as_ref() {
            hook.before_send(&send.payload).await;
        }
        if let Err(error) = send.scope.send(peer, send.payload).await {
            tracing::debug!(%peer, ?error, "ordered onion overlay send failed");
        }
        let has_more = lock(&state).ok().and_then(|mut state| state.complete(peer));
        let Some(has_more) = has_more else {
            tracing::debug!(%peer, "onion send outbox could not complete in-flight frame");
            return;
        };
        if !has_more {
            return;
        }
    }
}

const fn onion_send_jitter(sample: u8) -> Duration {
    Duration::from_millis(MIN_ONION_SEND_JITTER_MS + (sample as u64 % ONION_SEND_JITTER_SPAN_MS))
}

fn capacity_error(peer: Did, reason: OnionQueueAdmissionReason) -> Error {
    Error::OnionQueueAdmission {
        queue: OnionQueueKind::CircuitData,
        peer,
        reason,
    }
}

fn lock<T>(state: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    state.lock().map_err(|_| Error::Lock)
}

#[cfg(all(test, rings_native))]
pub(super) struct OnionSendTestHook {
    first: AtomicBool,
    released: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    observed: Mutex<Vec<Bytes>>,
    changed: tokio::sync::Notify,
}

#[cfg(all(test, rings_native))]
impl Default for OnionSendTestHook {
    fn default() -> Self {
        Self {
            first: AtomicBool::new(true),
            released: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            observed: Mutex::new(Vec::new()),
            changed: tokio::sync::Notify::new(),
        }
    }
}

#[cfg(all(test, rings_native))]
impl OnionSendTestHook {
    async fn before_send(&self, payload: &Bytes) {
        if self.first.swap(false, Ordering::AcqRel) {
            self.entered.notify_one();
            while !self.released.load(Ordering::Acquire) {
                let release = self.release.notified();
                if self.released.load(Ordering::Acquire) {
                    break;
                }
                release.await;
            }
        }
        if let Ok(mut observed) = self.observed.lock() {
            observed.push(payload.clone());
            self.changed.notify_waiters();
        }
    }

    pub(super) async fn wait_until_blocked(&self) {
        if !self.first.load(Ordering::Acquire) {
            return;
        }
        self.entered.notified().await;
    }

    pub(super) fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release.notify_waiters();
    }

    pub(super) fn observed(&self) -> Result<Vec<Bytes>> {
        lock(&self.observed).map(|observed| observed.clone())
    }

    pub(super) async fn wait_for_observed(&self, count: usize) -> Result<Vec<Bytes>> {
        loop {
            let changed = self.changed.notified();
            let complete = {
                let observed = lock(&self.observed)?;
                (observed.len() >= count).then(|| observed.clone())
            };
            if let Some(observed) = complete {
                return Ok(observed);
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_queue_preserves_peer_fifo_and_atomic_retirement() {
        let peer = Did::from(1_u32);
        let mut state = OrderedSendState::default();

        assert_eq!(state.enqueue(peer, 1), Ok(true));
        assert_eq!(state.enqueue(peer, 2), Ok(false));
        assert_eq!(state.take_next(peer), Some(1));
        assert_eq!(state.take_next(peer), None);
        assert_eq!(state.complete(peer), Some(true));
        assert_eq!(state.take_next(peer), Some(2));
        assert_eq!(state.complete(peer), Some(false));
        assert_eq!(state.pending, 0);
        assert!(!state.lanes.contains_key(&peer));
    }

    #[test]
    fn one_peer_cannot_exceed_its_queue_share() {
        let peer = Did::from(2_u32);
        let other = Did::from(3_u32);
        let mut state = OrderedSendState::default();
        for value in 0..MAX_PENDING_ONION_SENDS_PER_PEER {
            assert!(state.enqueue(peer, value).is_ok());
        }
        assert_eq!(
            state.enqueue(peer, 200),
            Err(OnionQueueAdmissionReason::PeerFull)
        );
        assert_eq!(state.enqueue(other, 201), Ok(true));
    }

    #[test]
    fn global_bound_rejects_exact_overflow_and_recovers_after_completion() {
        let mut state = OrderedSendState::default();
        for peer_id in 1_u32..=8 {
            let peer = Did::from(peer_id);
            for value in 0..MAX_PENDING_ONION_SENDS_PER_PEER {
                assert!(state.enqueue(peer, value).is_ok());
            }
        }
        let recovering_peer = Did::from(9_u32);
        assert_eq!(state.pending, MAX_PENDING_ONION_SENDS);
        assert_eq!(
            state.enqueue(recovering_peer, 1),
            Err(OnionQueueAdmissionReason::GlobalFull)
        );
        let first = Did::from(1_u32);
        assert!(state.take_next(first).is_some());
        assert_eq!(state.complete(first), Some(true));
        assert_eq!(state.enqueue(recovering_peer, 1), Ok(true));
    }
}
