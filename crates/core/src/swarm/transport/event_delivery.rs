use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Context;
use std::task::Poll;

use async_lock::Mutex as AsyncMutex;
use async_lock::MutexGuard as AsyncMutexGuard;
use futures::channel::oneshot;

use crate::dht::Did;

pub(super) type PeerOperationLock = Arc<AsyncMutex<()>>;

#[derive(Default)]
pub(super) struct PeerOperationLocks {
    locks: Mutex<BTreeMap<Did, PeerOperationLock>>,
}

/// Per-peer event sequencers. Unlike an async mutex guard, a sequence turn holds
/// no lock while application code is awaited.
#[derive(Default)]
pub(super) struct SwarmEventDeliveryLocks {
    sequences: Mutex<BTreeMap<Did, SwarmEventDeliveryLock>>,
}

#[derive(Clone)]
pub(crate) struct SwarmEventDeliveryLock(Arc<SwarmEventDeliverySequence>);

#[derive(Default)]
struct SwarmEventDeliverySequence {
    state: Mutex<DeliverySequenceState>,
}

// Ordered-start state relation (TLA-style, stated beside its executable refinement):
//
// Variables:
//   queue \in Seq(Turn), started \subseteq Turn, cancelled \subseteq Turn
// Init:
//   queue = << >> /\ started = {} /\ cancelled = {}
// Acquire(t):
//   queue' = Append(queue, t); t may start iff Head(queue') = t
// FirstPoll(t):
//   started' = started \cup {t}; queue' = Remove(queue, t); the new head is signalled
// Cancel(t):
//   cancelled' = cancelled \cup {t}; queue' = Remove(queue, t)
//   if t was the head, the new head is signalled
// Safety:
//   for enqueue order a < b, b \in started => a \in started \cup cancelled
//   and no application future owns the sequence capability after its first poll.
// Progress assumes a signalled head is eventually polled or cancelled.

#[derive(Default)]
struct DeliverySequenceState {
    queue: VecDeque<Arc<DeliveryTurnNode>>,
}

struct DeliveryTurnNode {
    start: Mutex<Option<oneshot::Sender<()>>>,
}

pub(crate) struct SwarmEventDeliveryTurn {
    sequence: Arc<SwarmEventDeliverySequence>,
    node: Arc<DeliveryTurnNode>,
    ready: Option<oneshot::Receiver<()>>,
}

struct OrderedCallbackStart<F> {
    turn: Option<SwarmEventDeliveryTurn>,
    callback: Pin<Box<F>>,
}

impl<F: Future> Future for OrderedCallbackStart<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.callback.as_mut().poll(context);
        drop(this.turn.take());
        result
    }
}

impl SwarmEventDeliverySequence {
    fn state(&self) -> MutexGuard<'_, DeliverySequenceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn acquire(self: &Arc<Self>) -> SwarmEventDeliveryTurn {
        let mut turn = {
            let mut state = self.state();
            let (start, ready) = if state.queue.is_empty() {
                (None, None)
            } else {
                let (sender, receiver) = oneshot::channel();
                (Some(sender), Some(receiver))
            };
            let node = Arc::new(DeliveryTurnNode {
                start: Mutex::new(start),
            });
            state.queue.push_back(Arc::clone(&node));
            SwarmEventDeliveryTurn {
                sequence: Arc::clone(self),
                node,
                ready,
            }
        };
        if let Some(ready) = turn.ready.take() {
            let _ = ready.await;
        }
        turn
    }

    fn finish(&self, node: &Arc<DeliveryTurnNode>) {
        let mut state = self.state();
        let Some(position) = state
            .queue
            .iter()
            .position(|queued| Arc::ptr_eq(queued, node))
        else {
            return;
        };
        state.queue.remove(position);
        let next = (position == 0)
            .then(|| state.queue.front())
            .flatten()
            .and_then(|next| {
                next.start
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            });
        drop(state);
        if let Some(next) = next {
            let _ = next.send(());
        }
    }
}

impl Drop for SwarmEventDeliveryTurn {
    fn drop(&mut self) {
        self.sequence.finish(&self.node);
    }
}

impl SwarmEventDeliveryTurn {
    /// Poll an external callback once in turn order, then release the ordering capability before
    /// awaiting application-owned suspension.
    ///
    /// The first poll is the formal "callback started" witness. Releasing immediately afterward
    /// prevents a callback that awaits another same-peer operation from deadlocking behind its
    /// own turn while preserving `start(A) < start(B)`.
    pub(crate) fn poll_once_then_release<F>(self, callback: F) -> impl Future<Output = F::Output>
    where F: Future {
        OrderedCallbackStart {
            turn: Some(self),
            callback: Box::pin(callback),
        }
    }
}

impl SwarmEventDeliveryLocks {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn lock_map(&self) -> MutexGuard<'_, BTreeMap<Did, SwarmEventDeliveryLock>> {
        self.sequences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn lock(&self, peer: Did) -> SwarmEventDeliveryLock {
        self.lock_map()
            .entry(peer)
            .or_insert_with(|| {
                SwarmEventDeliveryLock(Arc::new(SwarmEventDeliverySequence::default()))
            })
            .clone()
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
        let mut sequences = self.lock_map();
        if sequences.get(&peer).is_some_and(|current| {
            Arc::ptr_eq(&current.0, &delivery.0) && Arc::strong_count(&current.0) <= 2
        }) {
            sequences.remove(&peer);
        }
    }
}

impl SwarmEventDeliveryLock {
    pub(crate) async fn acquire(&self) -> SwarmEventDeliveryTurn {
        self.0.acquire().await
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    fn queued_turns(&self) -> usize {
        self.0.state().queue.len()
    }
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

    fn lock_map(&self) -> MutexGuard<'_, BTreeMap<Did, PeerOperationLock>> {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(all(
        test,
        feature = "dummy",
        not(all(feature = "wasm", target_family = "wasm"))
    ))]
    pub(super) fn lock(&self, peer: Did) -> PeerOperationLock {
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

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    #[tokio::test]
    async fn cancelled_event_turn_does_not_block_the_following_turn() {
        let delivery = SwarmEventDeliveryLock(Arc::new(SwarmEventDeliverySequence::default()));
        let first = delivery.acquire().await;
        let waiting_delivery = delivery.clone();
        let waiting = tokio::spawn(async move { waiting_delivery.acquire().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while delivery.queued_turns() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiting turn must be registered before cancellation");
        waiting.abort();
        let _ = waiting.await;
        drop(first);

        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.acquire())
            .await
            .expect("cancelled turn must be skipped");
    }

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    #[tokio::test]
    async fn callback_start_order_does_not_hold_turn_across_suspension() {
        let delivery = SwarmEventDeliveryLock(Arc::new(SwarmEventDeliverySequence::default()));
        let first = delivery.acquire().await;
        let starts = Arc::new(Mutex::new(Vec::new()));
        let first_starts = Arc::clone(&starts);
        let (release_first, first_released) = oneshot::channel();

        let first_callback = tokio::spawn(async move {
            first
                .poll_once_then_release(async move {
                    first_starts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(1);
                    let _ = first_released.await;
                })
                .await;
        });

        let second = tokio::time::timeout(std::time::Duration::from_secs(1), delivery.acquire())
            .await
            .expect("the first callback must release its turn after its first poll");
        let second_starts = Arc::clone(&starts);
        second
            .poll_once_then_release(async move {
                second_starts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(2);
            })
            .await;

        assert_eq!(
            *starts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [1, 2]
        );
        assert!(
            !first_callback.is_finished(),
            "the second callback must start while the first remains suspended"
        );
        let _ = release_first.send(());
        tokio::time::timeout(std::time::Duration::from_secs(1), first_callback)
            .await
            .expect("the released callback must complete")
            .expect("the callback task must not panic");
    }
}
