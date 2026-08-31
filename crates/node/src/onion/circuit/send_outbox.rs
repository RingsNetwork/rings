//! Ordered off-gate delivery for onion data-plane frames.
//!
//! Traffic-shaping contract: let `B = ONION_LINK_BATCH_CELLS` and let a non-empty batch contain
//! `r` real cells, where `1 <= r <= B`. The drain attempts exactly `B - r` authenticated one-hop
//! cover cells after the real cells. When encryption and direct-edge sends succeed, every observable
//! batch therefore contains exactly `B` cells and the visible cell-count amplification is
//! `B / r <= B`. Cover is generated only for a real-driven batch; an idle lane emits nothing.
//!
//! One injected pacing delay precedes each batch. Production delays lie in the closed interval
//! `[MIN_ONION_SEND_JITTER_MS, MAX_ONION_SEND_JITTER_MS]`; with `B = 4`, the pacing-only nominal
//! rate is 160--800 visible cells per second before direct-edge backpressure. Sparse traffic can cost
//! four visible cells for one real cell. These bounded bandwidth, latency, and throughput costs are
//! intentional privacy properties, not queue inefficiencies to optimize away.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::VecDeque;
#[cfg(all(test, rings_native))]
use std::sync::atomic::AtomicUsize;
#[cfg(all(test, rings_native))]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::oneshot;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;

use super::cell::seal_message;
use super::cell::sealed_cell_bucket;
use super::codec::OnionWireMessage;
use super::OnionCellBucket;
use super::OnionLink;
use crate::error::OnionQueueAdmissionReason;
use crate::error::OnionQueueKind;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::extension::transport::platform::sleep;
use crate::extension::transport::platform::spawn_detached;
use crate::peer_quota::PeerQuota;
use crate::sync_lock::lock;
#[cfg(all(test, rings_native))]
use crate::test_support::BlockingSendProbe;

const MAX_PENDING_ONION_SENDS: usize = 1_024;
const MAX_PENDING_ONION_SENDS_PER_PEER: usize = 128;
const MAX_PENDING_ONION_SEND_BYTES: usize = 64 * 1024 * 1024;
const MAX_PENDING_ONION_SEND_BYTES_PER_PEER: usize = 16 * 1024 * 1024;
const MIN_ONION_SEND_JITTER_MS: u64 = 5;
const MAX_ONION_SEND_JITTER_MS: u64 = 25;
const ONION_SEND_JITTER_SPAN_MS: u64 = MAX_ONION_SEND_JITTER_MS - MIN_ONION_SEND_JITTER_MS + 1;
const ONION_LINK_BATCH_CELLS: usize = 4;

/// Pure fixed-batch algebra.
///
/// Post: for every accepted `real_cells`, `real_cells + result == ONION_LINK_BATCH_CELLS`.
const fn cover_cells_for_batch(real_cells: usize) -> Option<usize> {
    if real_cells == 0 || real_cells > ONION_LINK_BATCH_CELLS {
        None
    } else {
        Some(ONION_LINK_BATCH_CELLS - real_cells)
    }
}

/// Effect capability for the observable delay before one ordered overlay batch.
///
/// Keeping entropy behind this trait makes the queue transition algebra deterministic and lets
/// tests replace wall-clock randomness without adding a production-only branch to the drain.
trait OnionSendPacing: Send + Sync {
    fn delay_before_batch(&self) -> Duration;
}

struct RandomizedOnionSendPacing;

impl OnionSendPacing for RandomizedOnionSendPacing {
    fn delay_before_batch(&self) -> Duration {
        onion_send_jitter(rand::random())
    }
}

#[cfg(all(test, rings_native))]
struct ImmediateOnionSendPacing;

#[cfg(all(test, rings_native))]
impl OnionSendPacing for ImmediateOnionSendPacing {
    fn delay_before_batch(&self) -> Duration {
        Duration::ZERO
    }
}

struct OverlaySend {
    scope: Scope,
    payload: Bytes,
    cover: CoverSpec,
    completion: Option<oneshot::Sender<Result<()>>>,
}

#[derive(Clone, Copy)]
struct CoverSpec {
    recipient: PublicKey<33>,
    bucket: OnionCellBucket,
}

struct PeerLane<T> {
    in_flight_bytes: Option<usize>,
    queued: VecDeque<(T, usize)>,
    pending_bytes: usize,
}

struct OrderedSendState<T> {
    quota: PeerQuota,
    pending_bytes: usize,
    lanes: HashMap<Did, PeerLane<T>>,
}

impl<T> Default for OrderedSendState<T> {
    fn default() -> Self {
        Self {
            quota: PeerQuota::new(MAX_PENDING_ONION_SENDS, MAX_PENDING_ONION_SENDS_PER_PEER),
            pending_bytes: 0,
            lanes: HashMap::new(),
        }
    }
}

impl<T> OrderedSendState<T> {
    /// Reserve one frame and return whether its peer needs a new drain task.
    ///
    /// Invariant: `quota.total()`/`pending_bytes` equal queued work plus the optional in-flight item in
    /// every lane. No lane has more than one in-flight item, which is the serialization witness
    /// for per-peer order. Cover cells are generated just in time and are never retained here.
    fn enqueue(
        &mut self,
        peer: Did,
        item: T,
        item_bytes: usize,
    ) -> std::result::Result<bool, OnionQueueAdmissionReason> {
        let peer_pending_bytes = self.lanes.get(&peer).map_or(0, |lane| lane.pending_bytes);
        self.quota.can_reserve(peer)?;
        let next_peer_bytes = peer_pending_bytes
            .checked_add(item_bytes)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        let next_pending_bytes = self
            .pending_bytes
            .checked_add(item_bytes)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        if next_pending_bytes > MAX_PENDING_ONION_SEND_BYTES {
            return Err(OnionQueueAdmissionReason::GlobalFull);
        }
        if next_peer_bytes > MAX_PENDING_ONION_SEND_BYTES_PER_PEER {
            return Err(OnionQueueAdmissionReason::PeerFull);
        }
        self.quota.reserve(peer)?;
        self.pending_bytes = next_pending_bytes;
        match self.lanes.entry(peer) {
            Entry::Occupied(mut lane) => {
                let lane = lane.get_mut();
                lane.queued.push_back((item, item_bytes));
                lane.pending_bytes = next_peer_bytes;
                Ok(false)
            }
            Entry::Vacant(lane) => {
                lane.insert(PeerLane {
                    in_flight_bytes: None,
                    queued: VecDeque::from([(item, item_bytes)]),
                    pending_bytes: item_bytes,
                });
                Ok(true)
            }
        }
    }

    fn take_next(&mut self, peer: Did) -> Option<T> {
        let lane = self.lanes.get_mut(&peer)?;
        if lane.in_flight_bytes.is_some() {
            return None;
        }
        let (item, item_bytes) = lane.queued.pop_front()?;
        lane.in_flight_bytes = Some(item_bytes);
        Some(item)
    }

    fn has_queued(&self, peer: Did) -> bool {
        self.lanes
            .get(&peer)
            .is_some_and(|lane| !lane.queued.is_empty())
    }

    /// Cancel a lane before its next batch starts and return its effects to the caller.
    ///
    /// Pre: the lane has no in-flight item. Post: every queued item, byte, and quota reservation
    /// for `peer` is removed together; an inconsistent predecessor leaves state unchanged.
    fn cancel_queued_lane(&mut self, peer: Did) -> Option<Vec<T>> {
        let lane = self.lanes.get(&peer)?;
        if lane.in_flight_bytes.is_some()
            || lane.queued.is_empty()
            || lane.queued.len() != self.quota.peer_total(peer)
        {
            return None;
        }
        let next_pending_bytes = self.pending_bytes.checked_sub(lane.pending_bytes)?;
        let queued_count = lane.queued.len();
        let released = self.quota.release_peer(peer)?;
        if released != queued_count {
            return None;
        }
        let lane = self.lanes.remove(&peer)?;
        self.pending_bytes = next_pending_bytes;
        Some(lane.queued.into_iter().map(|(item, _)| item).collect())
    }

    /// Complete one in-flight frame and return whether the lane has more work.
    fn complete(&mut self, peer: Did) -> Option<bool> {
        let lane = self.lanes.get_mut(&peer)?;
        let completed_bytes = lane.in_flight_bytes?;
        let next_pending_bytes = self.pending_bytes.checked_sub(completed_bytes)?;
        let next_lane_bytes = lane.pending_bytes.checked_sub(completed_bytes)?;
        if !self.quota.release(peer) {
            return None;
        }
        lane.in_flight_bytes = None;
        lane.pending_bytes = next_lane_bytes;
        self.pending_bytes = next_pending_bytes;
        if lane.queued.is_empty() {
            self.lanes.remove(&peer);
            Some(false)
        } else {
            Some(true)
        }
    }
}

/// Per-next-hop ordered outbox. Enqueue is synchronous; direct-edge backpressure lives in drains.
#[derive(Clone)]
struct OnionSendOutbox {
    state: Arc<Mutex<OrderedSendState<OverlaySend>>>,
    pacing: Arc<dyn OnionSendPacing>,
    #[cfg(all(test, rings_native))]
    test_hook: Option<Arc<OnionSendTestHook>>,
}

impl Default for OnionSendOutbox {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            pacing: Arc::new(RandomizedOnionSendPacing),
            #[cfg(all(test, rings_native))]
            test_hook: None,
        }
    }
}

impl OnionSendOutbox {
    fn enqueue(
        &self,
        scope: Scope,
        to: Did,
        recipient: PublicKey<33>,
        bucket: OnionCellBucket,
        payload: Bytes,
        completion: Option<oneshot::Sender<Result<()>>>,
    ) -> Result<()> {
        let item_bytes = payload.len();
        let should_spawn = lock(&self.state)?
            .enqueue(
                to,
                OverlaySend {
                    scope,
                    payload,
                    cover: CoverSpec { recipient, bucket },
                    completion,
                },
                item_bytes,
            )
            .map_err(|reason| OnionQueueKind::CircuitData.admission(to, reason))?;
        if should_spawn {
            let state = Arc::clone(&self.state);
            let pacing = Arc::clone(&self.pacing);
            #[cfg(all(test, rings_native))]
            let test_hook = self.test_hook.clone();
            spawn_detached(async move {
                drain_peer(
                    state,
                    to,
                    pacing,
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
            pacing: Arc::new(ImmediateOnionSendPacing),
            test_hook: Some(test_hook),
        }
    }
}

/// Shared endpoint/relay capability for one node's ordered, paced onion link traffic.
///
/// Clones share the same bounded peer lanes. This is the single effect boundary through which
/// real circuit cells enter the overlay; relay effects enqueue without waiting, while endpoint
/// adapters may await the real cell's overlay result without holding protocol transition state.
#[derive(Clone, Default)]
pub(crate) struct OnionLinkSender {
    outbox: OnionSendOutbox,
}

impl OnionLinkSender {
    pub(super) fn enqueue_sealed(
        &self,
        scope: Scope,
        link: OnionLink,
        payload: Bytes,
    ) -> Result<()> {
        let bucket = sealed_cell_bucket(&payload)?;
        self.outbox
            .enqueue(scope, link.peer, link.recipient, bucket, payload, None)
    }

    pub(crate) async fn send_sealed(
        &self,
        scope: Scope,
        link: OnionLink,
        payload: Bytes,
    ) -> Result<()> {
        let bucket = sealed_cell_bucket(&payload)?;
        let (completion, completed) = oneshot::channel();
        self.outbox.enqueue(
            scope,
            link.peer,
            link.recipient,
            bucket,
            payload,
            Some(completion),
        )?;
        completed.await.map_err(|_| {
            crate::error::Error::OnionRouteError(crate::onion::OnionRouteError::LinkSendCancelled)
        })?
    }

    #[cfg(all(test, rings_native))]
    pub(super) fn with_test_hook(test_hook: Arc<OnionSendTestHook>) -> Self {
        Self {
            outbox: OnionSendOutbox::with_test_hook(test_hook),
        }
    }
}

async fn drain_peer(
    state: Arc<Mutex<OrderedSendState<OverlaySend>>>,
    peer: Did,
    pacing: Arc<dyn OnionSendPacing>,
    #[cfg(all(test, rings_native))] test_hook: Option<Arc<OnionSendTestHook>>,
) {
    loop {
        // One lane owns FIFO order. Delay once per bounded batch, then fill its unused slots with
        // authenticated one-hop cover cells. This quantizes both the forwarding time and visible
        // count without holding the protocol transition gate or creating an unbounded cover task.
        if let Err(error) = sleep(pacing.delay_before_batch()).await {
            let cancelled = lock(&state)
                .ok()
                .and_then(|mut state| state.cancel_queued_lane(peer));
            let Some(cancelled) = cancelled else {
                tracing::debug!(%peer, ?error, "onion send outbox pacing and cleanup failed");
                return;
            };
            let cancelled_count = cancelled.len();
            drop(cancelled);
            tracing::debug!(%peer, ?error, cancelled_count, "onion send outbox pacing failed");
            return;
        }
        let mut batch_slots = 0;
        loop {
            let Some(send) = lock(&state)
                .ok()
                .and_then(|mut state| state.take_next(peer))
            else {
                tracing::debug!(%peer, "onion send outbox lost drain ownership");
                return;
            };
            let OverlaySend {
                scope,
                payload,
                cover,
                completion,
            } = send;
            #[cfg(all(test, rings_native))]
            if let Some(hook) = test_hook.as_ref() {
                hook.before_send(&payload).await;
            }
            // `peer` is the circuit's exact physical next hop. Overlay routing could select a
            // different Chord path and would invalidate both adjacency and per-link cover shape.
            let send_result = scope.send_direct(peer, payload).await;
            match completion {
                Some(completion) => {
                    let _ = completion.send(send_result);
                }
                None => {
                    if let Err(error) = send_result {
                        tracing::debug!(%peer, ?error, "ordered onion direct-edge send failed");
                    }
                }
            }
            batch_slots += 1;
            let queued_real = batch_slots < ONION_LINK_BATCH_CELLS
                && lock(&state)
                    .map(|state| state.has_queued(peer))
                    .unwrap_or(false);
            if queued_real {
                if lock(&state).ok().and_then(|mut state| state.complete(peer)) != Some(true) {
                    tracing::debug!(%peer, "onion send outbox lost queued batch ownership");
                    return;
                }
                continue;
            }

            let Some(cover_cells) = cover_cells_for_batch(batch_slots) else {
                tracing::debug!(%peer, batch_slots, "onion send outbox produced invalid batch");
                return;
            };
            for _ in 0..cover_cells {
                let cover = match seal_message(
                    &OnionWireMessage::Cover,
                    cover.recipient,
                    Some(cover.bucket),
                ) {
                    Ok(cover) => cover,
                    Err(error) => {
                        tracing::debug!(%peer, ?error, "onion link-cover encryption failed");
                        break;
                    }
                };
                #[cfg(all(test, rings_native))]
                if let Some(hook) = test_hook.as_ref() {
                    hook.before_cover();
                }
                if let Err(error) = scope.send_direct(peer, cover).await {
                    tracing::debug!(%peer, ?error, "onion link-cover send failed");
                }
            }

            let has_more = lock(&state).ok().and_then(|mut state| state.complete(peer));
            let Some(has_more) = has_more else {
                tracing::debug!(%peer, "onion send outbox could not complete in-flight frame");
                return;
            };
            if !has_more {
                return;
            }
            break;
        }
    }
}

const fn onion_send_jitter(sample: u8) -> Duration {
    Duration::from_millis(MIN_ONION_SEND_JITTER_MS + (sample as u64 % ONION_SEND_JITTER_SPAN_MS))
}

#[cfg(all(test, rings_native))]
pub(super) struct OnionSendTestHook {
    blocking: BlockingSendProbe<()>,
    observed: Mutex<Vec<Bytes>>,
    changed: tokio::sync::Notify,
    cover_count: AtomicUsize,
    cover_changed: tokio::sync::Notify,
}

#[cfg(all(test, rings_native))]
impl Default for OnionSendTestHook {
    fn default() -> Self {
        Self {
            blocking: BlockingSendProbe::default(),
            observed: Mutex::new(Vec::new()),
            changed: tokio::sync::Notify::new(),
            cover_count: AtomicUsize::new(0),
            cover_changed: tokio::sync::Notify::new(),
        }
    }
}

#[cfg(all(test, rings_native))]
impl OnionSendTestHook {
    async fn before_send(&self, payload: &Bytes) {
        let _ = self.blocking.block_first(()).await;
        if let Ok(mut observed) = self.observed.lock() {
            observed.push(payload.clone());
            self.changed.notify_waiters();
        }
    }

    fn before_cover(&self) {
        self.cover_count.fetch_add(1, Ordering::AcqRel);
        self.cover_changed.notify_waiters();
    }

    pub(super) async fn wait_until_blocked(&self) {
        self.blocking.wait_until_blocked().await;
    }

    pub(super) fn release(&self) {
        self.blocking.release();
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

    pub(super) async fn wait_for_covers(&self, count: usize) {
        loop {
            let changed = self.cover_changed.notified();
            if self.cover_count.load(Ordering::Acquire) >= count {
                return;
            }
            changed.await;
        }
    }

    pub(super) fn cover_count(&self) -> usize {
        self.cover_count.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pure_queue_preserves_peer_fifo_and_atomic_retirement() {
        let peer = Did::from(1_u32);
        let mut state = OrderedSendState::default();

        assert_eq!(state.enqueue(peer, 1, 1), Ok(true));
        assert_eq!(state.enqueue(peer, 2, 1), Ok(false));
        assert_eq!(state.take_next(peer), Some(1));
        assert_eq!(state.take_next(peer), None);
        assert_eq!(state.complete(peer), Some(true));
        assert_eq!(state.take_next(peer), Some(2));
        assert_eq!(state.complete(peer), Some(false));
        assert_eq!(state.quota.total(), 0);
        assert_eq!(state.pending_bytes, 0);
        assert!(!state.lanes.contains_key(&peer));
    }

    #[test]
    fn test_failed_quota_release_does_not_half_retire_in_flight_item() {
        let peer = Did::from(12_u32);
        let mut state = OrderedSendState::default();

        assert_eq!(state.enqueue(peer, 1, 7), Ok(true));
        assert_eq!(state.take_next(peer), Some(1));
        state.quota = PeerQuota::new(MAX_PENDING_ONION_SENDS, MAX_PENDING_ONION_SENDS_PER_PEER);

        assert_eq!(state.complete(peer), None);
        let lane = state.lanes.get(&peer).expect("in-flight lane is retained");
        assert_eq!(lane.in_flight_bytes, Some(7));
        assert_eq!(lane.pending_bytes, 7);
        assert_eq!(state.pending_bytes, 7);
    }

    #[test]
    fn test_pre_batch_cancellation_atomically_retires_the_peer_lane() {
        let peer = Did::from(13_u32);
        let other = Did::from(14_u32);
        let mut state = OrderedSendState::default();

        assert_eq!(state.enqueue(peer, 1, 7), Ok(true));
        assert_eq!(state.enqueue(peer, 2, 11), Ok(false));
        assert_eq!(state.enqueue(other, 3, 5), Ok(true));
        assert_eq!(state.cancel_queued_lane(peer), Some(vec![1, 2]));
        assert_eq!(state.quota.total(), 1);
        assert_eq!(state.quota.peer_total(peer), 0);
        assert_eq!(state.quota.peer_total(other), 1);
        assert_eq!(state.pending_bytes, 5);
        assert!(!state.lanes.contains_key(&peer));
        assert!(state.lanes.contains_key(&other));
    }

    #[test]
    fn test_pre_batch_cancellation_rejects_an_in_flight_lane_without_mutation() {
        let peer = Did::from(15_u32);
        let mut state = OrderedSendState::default();

        assert_eq!(state.enqueue(peer, 1, 7), Ok(true));
        assert_eq!(state.take_next(peer), Some(1));
        assert_eq!(state.cancel_queued_lane(peer), None);
        assert_eq!(state.quota.peer_total(peer), 1);
        assert_eq!(state.pending_bytes, 7);
        assert!(state.lanes.contains_key(&peer));
    }

    #[test]
    fn test_one_peer_cannot_exceed_its_queue_share() {
        let peer = Did::from(2_u32);
        let other = Did::from(3_u32);
        let mut state = OrderedSendState::default();
        for value in 0..MAX_PENDING_ONION_SENDS_PER_PEER {
            assert!(state.enqueue(peer, value, 1).is_ok());
        }
        assert_eq!(
            state.enqueue(peer, 200, 1),
            Err(OnionQueueAdmissionReason::PeerFull)
        );
        assert_eq!(state.enqueue(other, 201, 1), Ok(true));
    }

    #[test]
    fn test_global_bound_rejects_exact_overflow_and_recovers_after_completion() {
        let mut state = OrderedSendState::default();
        for peer_id in 1_u32..=8 {
            let peer = Did::from(peer_id);
            for value in 0..MAX_PENDING_ONION_SENDS_PER_PEER {
                assert!(state.enqueue(peer, value, 1).is_ok());
            }
        }
        let recovering_peer = Did::from(9_u32);
        assert_eq!(state.quota.total(), MAX_PENDING_ONION_SENDS);
        assert_eq!(
            state.enqueue(recovering_peer, 1, 1),
            Err(OnionQueueAdmissionReason::GlobalFull)
        );
        let first = Did::from(1_u32);
        assert!(state.take_next(first).is_some());
        assert_eq!(state.complete(first), Some(true));
        assert_eq!(state.enqueue(recovering_peer, 1, 1), Ok(true));
    }

    #[test]
    fn test_queued_cell_bytes_have_global_and_per_peer_hard_bounds() {
        let peer = Did::from(10_u32);
        let other = Did::from(11_u32);
        let mut state = OrderedSendState::default();

        assert_eq!(
            state.enqueue(peer, 1, MAX_PENDING_ONION_SEND_BYTES_PER_PEER),
            Ok(true)
        );
        assert_eq!(
            state.enqueue(peer, 2, 1),
            Err(OnionQueueAdmissionReason::PeerFull)
        );
        assert_eq!(state.enqueue(other, 3, 1), Ok(true));
        assert!(state.take_next(peer).is_some());
        assert_eq!(state.complete(peer), Some(false));
        assert_eq!(state.pending_bytes, 1);

        let mut global = OrderedSendState::default();
        for peer_id in 20_u32..24 {
            assert!(global
                .enqueue(
                    Did::from(peer_id),
                    peer_id,
                    MAX_PENDING_ONION_SEND_BYTES_PER_PEER,
                )
                .is_ok());
        }
        assert_eq!(global.pending_bytes, MAX_PENDING_ONION_SEND_BYTES);
        assert_eq!(
            global.enqueue(Did::from(24_u32), 24, 1),
            Err(OnionQueueAdmissionReason::GlobalFull)
        );
    }

    #[test]
    fn test_production_pacing_maps_entropy_into_the_documented_closed_interval() {
        assert_eq!(onion_send_jitter(0), Duration::from_millis(5));
        assert_eq!(onion_send_jitter(20), Duration::from_millis(25));
        assert_eq!(onion_send_jitter(u8::MAX), Duration::from_millis(8));
        for sample in u8::MIN..=u8::MAX {
            let delay_ms = onion_send_jitter(sample).as_millis();
            assert!(delay_ms >= u128::from(MIN_ONION_SEND_JITTER_MS));
            assert!(delay_ms <= u128::from(MAX_ONION_SEND_JITTER_MS));
        }
    }

    #[test]
    fn test_fixed_link_batch_algebra_bounds_cover_amplification() {
        assert_eq!(cover_cells_for_batch(0), None);
        assert_eq!(cover_cells_for_batch(ONION_LINK_BATCH_CELLS + 1), None);

        for real_cells in 1..=ONION_LINK_BATCH_CELLS {
            let cover_cells = cover_cells_for_batch(real_cells);
            assert_eq!(
                cover_cells.map(|cover| real_cells + cover),
                Some(ONION_LINK_BATCH_CELLS)
            );
            assert_eq!(
                cover_cells.map(|cover| real_cells + cover <= real_cells * ONION_LINK_BATCH_CELLS),
                Some(true)
            );
        }
    }
}
