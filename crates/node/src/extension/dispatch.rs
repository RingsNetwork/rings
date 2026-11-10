//! Per-namespace dispatch of inbound envelopes.
//!
//! ```text
//!   inbox reader ──submit(from, bytes)──▶ queue[namespace] ──worker──▶ Route::dispatch
//!                  (never waits; a refused       (one task per namespace, until stop)
//!                   envelope is dropped, counted)        └──────────▶ Route::observe_link
//!              ──PeerAdmitted(p)────────▶ Admitted(p), every queue (coalesced on unread churn)
//!              ──PeerRetired(p)─────────▶ Retired(p), every queue  (withdraws unread Admitted(p)
//!                                         when no queued envelope follows it)
//!              ──MessageDropped(p)──────▶ Dropped(p), every queue, once per generation while unread
//!              ──refused from admitted p─▶ Dropped(p), its queue, once per generation while unread
//! ```
//!
//! Laws:
//! - Each registered namespace's envelopes are dispatched in arrival order, one at a time.
//! - Namespaces are independent: a protocol that waits holds back only its own queue.
//! - Registration is checked once, here: an envelope enters a queue only with the [`Route`] its
//!   namespace resolved to, so only registered namespaces get a queue and the number of tasks
//!   is bounded by the registry.
//! - Each queue bounds envelopes twice: by [`NAMESPACE_QUEUE_CAPACITY`] envelopes, and by
//!   [`NAMESPACE_QUEUE_WEIGHT`] encoded bytes queued or in dispatch. A queue holding no bytes
//!   admits any envelope, so a namespace holds at most `max(NAMESPACE_QUEUE_WEIGHT, one
//!   envelope)`, and the largest envelope the transport carries still gets through.
//! - Control facts ([`LinkFact`]) share the namespace's arrival order and are coalesced per peer
//!   (`#1214`, [`FactTransition`]) so sustained peer churn while a protocol callback is pending
//!   cannot accumulate an unbounded history:
//!   - `Retired(p)` withdraws a matching unread `Admitted(p)` (together with any `Dropped(p)`
//!     queued after that `Admitted(p)`) whenever no queued envelope follows that `Admitted(p)`
//!     ([`FactTransition::WithdrewAdmission`]). When a queued envelope *does* follow `Admitted(p)`,
//!     both `Admitted(p)` (before the envelope) and `Retired(p)` (after it) are retained so
//!     queued envelopes always observe their link lifetime and protocol state is invalidated on
//!     retirement.
//!   - Within one link state, `Dropped(p)` is queued at most once while `p` is not already
//!     waiting on `Retired(p)`, and consecutive duplicate `Admitted(p)` or `Retired(p)` facts are
//!     coalesced ([`FactTransition::Coalesced`]).
//!   - Consequently, an envelope-free queue retains at most [`NAMESPACE_RUN_FACT_BOUND_PER_PEER`]
//!     (`4`) facts per peer (`[Dropped] [Retired] [Admitted] [Dropped]`). Across `k ≤
//!     NAMESPACE_QUEUE_CAPACITY` queued envelopes, one peer retains at most
//!     `3 · k + 4 ≤ NAMESPACE_PEER_FACT_BOUND` facts.
//!   - An envelope refused by a full queue from an admitted peer records `Dropped(p)` for that
//!     namespace (#966); a drop by the core's own inbox (`SwarmEvent::MessageDropped`, #969)
//!     names no namespace and goes to every registered namespace. A refusal from a peer not
//!     admitted makes none, since no link state rides a link that is not up. The fact names the
//!     envelope's verified signer, which is the adjacent hop for a direct send; an overlay-routed
//!     envelope from an admitted origin names that origin's link though another carried it, so the
//!     fact may end link state that lost nothing, never miss state that did.
//! - Every envelope that is not dispatched is counted by its [`DispatchDrop`] reason, and logged
//!   at `warn`.
//! - The workers end once the stop of the [`Backend::serve`](super::Backend::serve) that started
//!   them is requested, abandoning what they still hold.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::task::Poll;
use std::task::Waker;

use bytes::Bytes;
use futures::future::poll_fn;
use futures::FutureExt;
use rings_core::dht::Did;
use rings_core::lifecycle::StopToken;
use rings_core::swarm::application::APPLICATION_INBOX_WEIGHT;
use serde::Serialize;

use crate::extension::ext::Core;
use crate::extension::ext::Envelope;
use crate::extension::ext::LinkFact;
use crate::extension::ext::Route;
use crate::tasks::NodeTasks;

/// Envelopes one namespace may hold, queued or in dispatch, before further ones are dropped.
const NAMESPACE_QUEUE_CAPACITY: usize = 256;

/// Encoded bytes one namespace may hold, queued or in dispatch, before further envelopes are
/// dropped: a quarter of the core inbox's [`APPLICATION_INBOX_WEIGHT`], so the few built-in
/// namespaces together park about what the inbox itself retains.
const NAMESPACE_QUEUE_WEIGHT: usize = APPLICATION_INBOX_WEIGHT / 4;

/// Maximum control facts one peer may retain when no envelope waits in the namespace queue:
/// `[Dropped] [Retired] [Admitted] [Dropped]`.
const NAMESPACE_RUN_FACT_BOUND_PER_PEER: usize = 4;

/// Maximum control facts one peer may retain across an entire namespace queue holding at most
/// [`NAMESPACE_QUEUE_CAPACITY`] envelopes (`3 * k + 4` for `k <= NAMESPACE_QUEUE_CAPACITY`).
const NAMESPACE_PEER_FACT_BOUND: usize =
    3 * NAMESPACE_QUEUE_CAPACITY + NAMESPACE_RUN_FACT_BOUND_PER_PEER;

/// Why the router dropped an envelope instead of dispatching it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchDrop {
    /// The custom message did not decode as an [`Envelope`].
    Undecodable,
    /// No protocol is registered under the envelope's namespace.
    Unregistered,
    /// The namespace's queue was full, by count or by bytes.
    QueueFull,
    /// No worker runs the namespace's queue: there was no runtime to start one.
    Unserved,
}

/// The envelopes the router dropped, by [`DispatchDrop`] reason. Shared by a
/// [`Provider`](crate::provider::Provider), which reads it, and the backends serving it.
#[derive(Debug, Default)]
pub(crate) struct DispatchDrops {
    undecodable: AtomicU64,
    unregistered: AtomicU64,
    queue_full: AtomicU64,
    unserved: AtomicU64,
}

/// A reading of the envelopes the router dropped, by reason; each count is cumulative since the
/// provider was created.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchDropCounters {
    /// Custom messages that did not decode as an envelope.
    pub undecodable: u64,
    /// Envelopes of a namespace no protocol is registered under.
    pub unregistered: u64,
    /// Envelopes refused by their namespace's full queue.
    pub queue_full: u64,
    /// Envelopes of a namespace no worker could be started for.
    pub unserved: u64,
}

impl DispatchDrops {
    /// The counter of `reason`.
    fn counter(&self, reason: DispatchDrop) -> &AtomicU64 {
        match reason {
            DispatchDrop::Undecodable => &self.undecodable,
            DispatchDrop::Unregistered => &self.unregistered,
            DispatchDrop::QueueFull => &self.queue_full,
            DispatchDrop::Unserved => &self.unserved,
        }
    }

    /// Count one envelope dropped for `reason`, and log it.
    fn record(&self, reason: DispatchDrop) {
        let dropped = self
            .counter(reason)
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        tracing::warn!(?reason, dropped, "application envelope dropped");
    }

    /// The counts so far.
    pub(crate) fn counters(&self) -> DispatchDropCounters {
        DispatchDropCounters {
            undecodable: self.undecodable.load(Ordering::Relaxed),
            unregistered: self.unregistered.load(Ordering::Relaxed),
            queue_full: self.queue_full.load(Ordering::Relaxed),
            unserved: self.unserved.load(Ordering::Relaxed),
        }
    }
}

/// One item waiting for its namespace's worker.
enum Queued {
    /// An envelope from `from`, holding `weight` encoded bytes until its dispatch ends.
    Envelope {
        route: Route,
        from: Did,
        payload: Bytes,
        weight: usize,
    },
    /// A fact about the link to one peer.
    Fact { route: Route, fact: LinkFact },
}

/// Monotonic position of an item in one [`NamespaceQueue`].
type Position = u64;

/// Named coalescing transition when recording a [`LinkFact`] (#1214).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FactTransition {
    /// The fact was appended to the queue in arrival order.
    Queued,
    /// `Retired(peer)` withdrew its matching unread `Admitted(peer)` (and any `Dropped(peer)`
    /// after it) because no queued envelope depended on that unobserved link generation.
    WithdrewAdmission,
    /// A duplicate `Dropped`, `Admitted`, or `Retired` within the current link state was coalesced
    /// into the fact already waiting.
    Coalesced,
    /// No worker serves this namespace queue.
    Unserved,
}

/// Indexed positions of one peer's queued control facts, in ascending order.
#[derive(Default)]
struct PeerFacts {
    /// Queued admissions (`true`) and retirements (`false`), by position.
    lifecycle: VecDeque<(Position, bool)>,
    /// Queued `Dropped` facts, by position.
    drops: VecDeque<Position>,
}

impl PeerFacts {
    fn last_lifecycle(&self) -> Option<Position> {
        self.lifecycle.back().map(|(at, _)| *at)
    }

    fn len(&self) -> usize {
        self.lifecycle.len() + self.drops.len()
    }

    fn is_empty(&self) -> bool {
        self.lifecycle.is_empty() && self.drops.is_empty()
    }
}

/// Whether a queue holding `held` bytes admits an envelope of `weight` more: always when it
/// holds none.
fn admits(held: usize, weight: usize) -> bool {
    held == 0 || held.saturating_add(weight) <= NAMESPACE_QUEUE_WEIGHT
}

/// Shared state of one namespace's ordered queue.
#[derive(Default)]
struct Queue {
    /// Producer dropped: worker drains remaining items and exits.
    closed: bool,
    /// Receiver dropped (no runtime or worker stopped): refuse envelopes as `Unserved`.
    unserved: bool,
    /// Queued items in arrival order, keyed by monotonic position so withdrawn admissions are
    /// removed in place without shifting other items.
    items: BTreeMap<Position, Queued>,
    /// Next position to assign.
    next: Position,
    /// Position of the latest `Queued::Envelope` still waiting in `items`, if any.
    last_envelope: Option<Position>,
    /// Per-peer positions of queued control facts.
    facts: BTreeMap<Did, PeerFacts>,
    /// Envelopes currently waiting in `items` or executing `Route::dispatch`.
    envelopes: usize,
    /// Encoded envelope bytes currently waiting in `items` or executing `Route::dispatch`.
    weight: usize,
    /// Worker task waker when parked on an empty queue.
    waker: Option<Waker>,
}

impl Queue {
    /// Whether this queue admits one more envelope of `weight` bytes.
    fn admits_envelope(&self, weight: usize) -> bool {
        self.envelopes < NAMESPACE_QUEUE_CAPACITY && admits(self.weight, weight)
    }

    fn append(&mut self, queued: Queued) -> Position {
        let at = self.next;
        self.next = at.saturating_add(1);
        self.items.insert(at, queued);
        at
    }

    fn push_envelope(
        &mut self,
        route: Route,
        from: Did,
        payload: Bytes,
        weight: usize,
    ) -> Result<Option<Waker>, DispatchDrop> {
        if self.unserved {
            return Err(DispatchDrop::Unserved);
        }
        if !self.admits_envelope(weight) {
            return Err(DispatchDrop::QueueFull);
        }
        self.envelopes = self.envelopes.saturating_add(1);
        self.weight = self.weight.saturating_add(weight);
        let at = self.append(Queued::Envelope {
            route,
            from,
            payload,
            weight,
        });
        self.last_envelope = Some(at);
        Ok(self.waker.take())
    }

    fn release_envelope(&mut self, weight: usize) {
        if !self.unserved {
            self.envelopes = self.envelopes.saturating_sub(1);
            self.weight = self.weight.saturating_sub(weight);
        }
    }

    fn record_fact(&mut self, route: Route, fact: LinkFact) -> (FactTransition, Option<Waker>) {
        if self.unserved {
            return (FactTransition::Unserved, None);
        }
        let peer = fact.peer();
        let result = match fact {
            LinkFact::Admitted(_) => {
                if self
                    .facts
                    .get(&peer)
                    .and_then(|facts| facts.lifecycle.back())
                    .is_some_and(|&(_, admitted)| admitted)
                {
                    (FactTransition::Coalesced, None)
                } else {
                    let at = self.append(Queued::Fact { route, fact });
                    self.facts
                        .entry(peer)
                        .or_default()
                        .lifecycle
                        .push_back((at, true));
                    (FactTransition::Queued, self.waker.take())
                }
            }
            LinkFact::Retired(_) => {
                let withdraw_at = self
                    .facts
                    .get(&peer)
                    .and_then(|facts| facts.lifecycle.back())
                    .and_then(|&(at, admitted)| {
                        (admitted && self.last_envelope.is_none_or(|env_at| at > env_at))
                            .then_some(at)
                    });
                if let Some(admitted_at) = withdraw_at {
                    let mut remove_peer = false;
                    let mut withdrawn_drops = Vec::new();
                    if let Some(facts) = self.facts.get_mut(&peer) {
                        facts.lifecycle.pop_back();
                        while facts.drops.back().is_some_and(|&at| at > admitted_at) {
                            if let Some(drop_at) = facts.drops.pop_back() {
                                withdrawn_drops.push(drop_at);
                            }
                        }
                        remove_peer = facts.is_empty();
                    }
                    if remove_peer {
                        self.facts.remove(&peer);
                    }
                    self.items.remove(&admitted_at);
                    for drop_at in withdrawn_drops {
                        self.items.remove(&drop_at);
                    }
                    (FactTransition::WithdrewAdmission, None)
                } else if self
                    .facts
                    .get(&peer)
                    .and_then(|facts| facts.lifecycle.back())
                    .is_some_and(|&(_, admitted)| !admitted)
                {
                    (FactTransition::Coalesced, None)
                } else {
                    let at = self.append(Queued::Fact { route, fact });
                    self.facts
                        .entry(peer)
                        .or_default()
                        .lifecycle
                        .push_back((at, false));
                    (FactTransition::Queued, self.waker.take())
                }
            }
            LinkFact::Dropped(_) => {
                let coalesced = self.facts.get(&peer).is_some_and(|facts| {
                    let retired = facts
                        .lifecycle
                        .back()
                        .is_some_and(|&(_, admitted)| !admitted);
                    let since = facts.last_lifecycle();
                    let drop_waiting = facts
                        .drops
                        .back()
                        .is_some_and(|&at| since.is_none_or(|since| at > since));
                    retired || drop_waiting
                });
                if coalesced {
                    (FactTransition::Coalesced, None)
                } else {
                    let at = self.append(Queued::Fact { route, fact });
                    self.facts.entry(peer).or_default().drops.push_back(at);
                    (FactTransition::Queued, self.waker.take())
                }
            }
        };
        debug_assert!(self.facts.get(&peer).is_none_or(|facts| {
            let bound = if self.last_envelope.is_none() {
                NAMESPACE_RUN_FACT_BOUND_PER_PEER
            } else {
                NAMESPACE_PEER_FACT_BOUND
            };
            facts.len() <= bound
        }));
        result
    }

    fn take(&mut self) -> Option<Queued> {
        let (at, queued) = self.items.pop_first()?;
        match &queued {
            Queued::Envelope { .. } => {
                if self.last_envelope == Some(at) {
                    self.last_envelope = None;
                }
            }
            Queued::Fact { fact, .. } => {
                let peer = fact.peer();
                let mut remove_peer = false;
                if let Some(facts) = self.facts.get_mut(&peer) {
                    match fact {
                        LinkFact::Admitted(_) | LinkFact::Retired(_) => {
                            if facts.lifecycle.front().is_some_and(|&(pos, _)| pos == at) {
                                facts.lifecycle.pop_front();
                            } else {
                                facts.lifecycle.retain(|&(pos, _)| pos != at);
                            }
                        }
                        LinkFact::Dropped(_) => {
                            if facts.drops.front() == Some(&at) {
                                facts.drops.pop_front();
                            } else {
                                facts.drops.retain(|&pos| pos != at);
                            }
                        }
                    }
                    remove_peer = facts.is_empty();
                }
                if remove_peer {
                    self.facts.remove(&peer);
                }
            }
        }
        Some(queued)
    }

    fn close_sender(&mut self) -> Option<Waker> {
        self.closed = true;
        self.waker.take()
    }

    fn close_receiver(&mut self) {
        self.unserved = true;
        self.items.clear();
        self.facts.clear();
        self.last_envelope = None;
        self.envelopes = 0;
        self.weight = 0;
        self.waker = None;
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
}

impl Shared {
    /// Every critical section leaves [`Queue`] consistent, so a poisoned lock remains valid.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Producer handle for one namespace's queue.
struct NamespaceQueue {
    shared: Arc<Shared>,
}

/// Worker handle for one namespace's queue.
struct NamespaceReceiver {
    shared: Arc<Shared>,
}

/// Releases an envelope's count and byte weight once `Route::dispatch` finishes or is cancelled.
struct EnvelopePermit {
    shared: Arc<Shared>,
    weight: usize,
}

impl Drop for EnvelopePermit {
    fn drop(&mut self) {
        self.shared.lock().release_envelope(self.weight);
    }
}

impl Drop for NamespaceQueue {
    fn drop(&mut self) {
        let waker = self.shared.lock().close_sender();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Drop for NamespaceReceiver {
    fn drop(&mut self) {
        self.shared.lock().close_receiver();
    }
}

fn channel() -> (NamespaceQueue, NamespaceReceiver) {
    let shared = Arc::new(Shared::default());
    (
        NamespaceQueue {
            shared: Arc::clone(&shared),
        },
        NamespaceReceiver { shared },
    )
}

impl NamespaceReceiver {
    async fn next(&self) -> Option<Queued> {
        poll_fn(|cx| {
            let mut queue = self.shared.lock();
            if let Some(queued) = queue.take() {
                return Poll::Ready(Some(queued));
            }
            if queue.closed {
                return Poll::Ready(None);
            }
            queue.waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}

/// The dispatch queue of every namespace that has received an envelope or link fact.
pub(super) struct NamespaceQueues {
    core: Core,
    queues: HashMap<String, NamespaceQueue>,
    drops: Arc<DispatchDrops>,
    stop: StopToken,
}

impl NamespaceQueues {
    /// No queue yet, over the registry of `core`, counting drops in `drops`; the workers end
    /// once `stop` is requested.
    pub(super) fn new(core: Core, drops: Arc<DispatchDrops>, stop: StopToken) -> Self {
        Self {
            core,
            queues: HashMap::new(),
            drops,
            stop,
        }
    }

    /// Queue the encoded envelope `bytes` from `from` for its namespace's worker, starting the
    /// worker on first use; drop it, counted, when it is refused, telling the namespace of a drop
    /// from an admitted peer ([`LinkFact::Dropped`]). Never waits.
    ///
    /// The envelope is read in place: its payload is a slice of `bytes`, and its namespace is
    /// copied only when the namespace's queue is first made.
    pub(super) fn submit(&mut self, from: Did, bytes: &Bytes) {
        let Ok(envelope) = Envelope::view(bytes) else {
            self.drops.record(DispatchDrop::Undecodable);
            return;
        };
        let Some(route) = self.core.route(envelope.namespace) else {
            self.drops.record(DispatchDrop::Unregistered);
            return;
        };
        let queue = queue_of(
            &mut self.queues,
            self.core.tasks(),
            &self.stop,
            envelope.namespace,
        );
        let weight = bytes.len();
        let payload = bytes.slice_ref(envelope.payload);
        match queue.submit_envelope(route.clone(), from, payload, weight) {
            Ok(()) => {}
            Err(DispatchDrop::QueueFull) => {
                self.drops.record(DispatchDrop::QueueFull);
                if self.core.peer_admitted(from) {
                    queue.tell(route, LinkFact::Dropped(from));
                }
            }
            Err(reason) => self.drops.record(reason),
        }
    }

    /// Queue `fact` for every registered namespace, behind the envelopes already queued: an
    /// admission, a retirement, or a drop by the core's inbox, which names no namespace. Never
    /// waits; coalesces per-peer churn according to [`FactTransition`].
    pub(super) fn submit_link_fact(&mut self, fact: LinkFact) {
        for (namespace, route) in self.core.routes() {
            let queue = queue_of(&mut self.queues, self.core.tasks(), &self.stop, &namespace);
            queue.tell(route, fact);
        }
    }
}

impl NamespaceQueue {
    fn submit_envelope(
        &self,
        route: Route,
        from: Did,
        payload: Bytes,
        weight: usize,
    ) -> Result<(), DispatchDrop> {
        let waker = self
            .shared
            .lock()
            .push_envelope(route, from, payload, weight)?;
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Record `fact` for `route` under the queue's coalescing policy.
    fn tell(&self, route: Route, fact: LinkFact) -> FactTransition {
        let (transition, waker) = self.shared.lock().record_fact(route, fact);
        if let Some(waker) = waker {
            waker.wake();
        }
        if transition == FactTransition::Unserved {
            tracing::error!(?fact, "no worker to deliver a link fact");
        }
        transition
    }
}

/// The queue of `namespace` in `queues`, starting its worker as a node task of `tasks`, which
/// ends once `stop` is requested or the node stops, on first use.
fn queue_of<'queues>(
    queues: &'queues mut HashMap<String, NamespaceQueue>,
    tasks: &NodeTasks,
    stop: &StopToken,
    namespace: &str,
) -> &'queues NamespaceQueue {
    if !queues.contains_key(namespace) {
        queues.insert(namespace.to_owned(), spawn_worker(tasks, stop.clone()));
    }
    &queues[namespace]
}

/// Start the worker of one namespace as a node task of `tasks`: it dispatches its envelopes in
/// order until its queue is dropped, `stop` is requested, or the node stops. Without a runtime
/// to run it, the receiver is dropped immediately, so every envelope is dropped and counted.
fn spawn_worker(tasks: &NodeTasks, stop: StopToken) -> NamespaceQueue {
    let (queue, receiver) = channel();
    let drain = drain(receiver);
    let worker = async move {
        let stopped = stop.stopped().fuse();
        let drain = drain.fuse();
        futures::pin_mut!(stopped, drain);
        futures::select_biased! {
            () = stopped => {},
            () = drain => {},
        }
    };
    if tasks.spawn(worker).is_err() {
        tracing::error!("no runtime to run a namespace dispatch worker");
    }
    queue
}

/// Dispatch every item of `receiver` in order, releasing each envelope's held capacity once its
/// dispatch ends.
async fn drain(receiver: NamespaceReceiver) {
    while let Some(queued) = receiver.next().await {
        let dispatched = match queued {
            Queued::Envelope {
                route,
                from,
                payload,
                weight,
            } => {
                let _permit = EnvelopePermit {
                    shared: Arc::clone(&receiver.shared),
                    weight,
                };
                route.dispatch(from, payload).await
            }
            Queued::Fact { route, fact } => route.observe_link(fact).await,
        };
        if let Err(error) = dispatched {
            tracing::warn!(error = ?error, "protocol dispatch failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::admits;
    use super::DispatchDrop;
    use super::DispatchDropCounters;
    use super::DispatchDrops;
    use super::Queue;
    use super::NAMESPACE_QUEUE_CAPACITY;
    use super::NAMESPACE_QUEUE_WEIGHT;

    #[test]
    fn test_each_drop_is_counted_under_its_own_reason() {
        let drops = DispatchDrops::default();
        drops.record(DispatchDrop::Undecodable);
        drops.record(DispatchDrop::QueueFull);
        drops.record(DispatchDrop::QueueFull);
        assert_eq!(drops.counters(), DispatchDropCounters {
            undecodable: 1,
            unregistered: 0,
            queue_full: 2,
            unserved: 0,
        });
    }

    /// Law: a queue admits an envelope iff it holds fewer than `NAMESPACE_QUEUE_CAPACITY`
    /// envelopes within its weight; a release makes room again.
    #[test]
    fn test_a_queue_admits_envelopes_up_to_its_capacity() {
        let mut queue = Queue::default();
        for _ in 0..NAMESPACE_QUEUE_CAPACITY {
            assert!(queue.admits_envelope(1));
            queue.envelopes += 1;
            queue.weight += 1;
        }
        assert!(!queue.admits_envelope(1));
        queue.release_envelope(1);
        assert!(queue.admits_envelope(1));
    }

    #[test]
    fn test_an_empty_queue_admits_any_envelope() {
        assert!(admits(0, NAMESPACE_QUEUE_WEIGHT.saturating_mul(4)));
    }

    #[test]
    fn test_a_holding_queue_admits_up_to_its_weight_only() {
        assert!(admits(1, NAMESPACE_QUEUE_WEIGHT - 1));
        assert!(!admits(1, NAMESPACE_QUEUE_WEIGHT));
    }

    #[cfg(rings_native)]
    mod native {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::time::Duration;

        use async_trait::async_trait;
        use bytes::Bytes;
        use rings_core::delegation::DelegateeKey;
        use rings_core::dht::Did;
        use rings_core::ecc::SecretKey;
        use rings_core::lifecycle::StopSource;

        use super::super::channel;
        use super::super::DispatchDrops;
        use super::super::FactTransition;
        use super::super::NamespaceQueues;
        use super::super::Queued;
        use super::super::NAMESPACE_PEER_FACT_BOUND;
        use super::super::NAMESPACE_RUN_FACT_BOUND_PER_PEER;
        use crate::extension::ext::Ctx;
        use crate::extension::ext::EffectScope;
        use crate::extension::ext::Envelope;
        use crate::extension::ext::Extensions;
        use crate::extension::ext::Interpret;
        use crate::extension::ext::LinkFact;
        use crate::extension::ext::Protocol;
        use crate::extension::ext::Reject;
        use crate::extension::ext::Route;
        use crate::extension::ext::Transition;
        use crate::extension::ext::Wire;
        use crate::processor::ProcessorBuilder;
        use crate::processor::ProcessorConfig;

        #[derive(Clone, Debug, Eq, PartialEq)]
        enum ObservedItem {
            Payload(Did, Bytes),
            Fact(LinkFact),
        }

        struct TestProtocol {
            namespace: &'static str,
        }

        impl Protocol for TestProtocol {
            type State = ();
            type Event = ObservedItem;
            type Effect = ObservedItem;

            fn namespace(&self) -> &str {
                self.namespace
            }

            fn init(&self) -> Self::State {}

            fn decode(&self, wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
                Ok(ObservedItem::Payload(
                    wire.from,
                    Bytes::copy_from_slice(wire.payload),
                ))
            }

            fn step(
                &self,
                _ctx: Ctx<'_, Self::State>,
                event: Self::Event,
            ) -> Transition<Self::State, Self::Effect> {
                Transition::with((), vec![event])
            }

            fn link_fact(&self, fact: LinkFact) -> Option<Self::Event> {
                Some(ObservedItem::Fact(fact))
            }
        }

        struct RecordingInterpreter {
            observed: Arc<Mutex<Vec<ObservedItem>>>,
        }

        #[async_trait]
        impl Interpret for RecordingInterpreter {
            type Effect = ObservedItem;

            async fn run(
                &self,
                _scope: &EffectScope,
                effect: Self::Effect,
            ) -> crate::error::Result<Vec<Bytes>> {
                self.observed.lock().expect("observed log").push(effect);
                Ok(Vec::new())
            }
        }

        fn test_extensions_and_route(namespace: &'static str) -> (Extensions, Route) {
            let key = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("delegatee key");
            let config = ProcessorConfig::new(1, String::new(), key, 1);
            let processor = ProcessorBuilder::from_config(&config)
                .expect("builder")
                .advertise_presence(false)
                .build()
                .expect("processor");
            let extensions = Extensions::new(Arc::new(processor));
            extensions
                .register(TestProtocol { namespace }, RecordingInterpreter {
                    observed: Arc::new(Mutex::new(Vec::new())),
                })
                .expect("register test protocol");
            let route = extensions.core().route(namespace).expect("resolved route");
            (extensions, route)
        }

        /// Law (#966): at most one `Dropped` fact per peer generation waits in a queue; once the
        /// worker takes it, a later drop from that peer is told again.
        #[tokio::test]
        async fn test_a_drop_is_told_once_while_its_fact_waits() {
            let (_extensions, route) = test_extensions_and_route("drop-once");
            let (sender, _receiver) = channel();
            let peer = Did::from(7_u32);

            assert_eq!(
                sender.tell(route.clone(), LinkFact::Dropped(peer)),
                FactTransition::Queued
            );
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Dropped(peer)),
                FactTransition::Coalesced
            );
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Dropped(Did::from(8_u32))),
                FactTransition::Queued
            );

            let taken = sender.shared.lock().take();
            assert!(matches!(
                taken,
                Some(Queued::Fact {
                    fact: LinkFact::Dropped(actual),
                    ..
                }) if actual == peer
            ));
            assert_eq!(
                sender.tell(route, LinkFact::Dropped(peer)),
                FactTransition::Queued
            );
        }

        /// Regression (#1214): 10,000 alternating `Admitted` / `Retired` pairs on an unread
        /// namespace queue coalesce instead of retaining 20,000 items, while preserving an initial
        /// `Retired` needed to invalidate already-admitted protocol state.
        #[tokio::test]
        async fn test_unread_peer_churn_is_bounded_and_preserves_prior_retirement() {
            let (_extensions, route) = test_extensions_and_route("churn-bound");
            let (sender, _receiver) = channel();
            let peer = Did::from(42_u32);

            // Case 1: peer was not previously admitted; 10,000 unobserved admit+retire pairs
            // collapse to zero retained items.
            for _ in 0..10_000 {
                assert_eq!(
                    sender.tell(route.clone(), LinkFact::Admitted(peer)),
                    FactTransition::Queued
                );
                assert_eq!(
                    sender.tell(route.clone(), LinkFact::Dropped(peer)),
                    FactTransition::Queued
                );
                assert_eq!(
                    sender.tell(route.clone(), LinkFact::Retired(peer)),
                    FactTransition::WithdrewAdmission
                );
            }
            {
                let guard = sender.shared.lock();
                assert_eq!(guard.items.len(), 0);
                assert!(guard.facts.is_empty());
                assert_eq!(guard.envelopes, 0);
                assert_eq!(guard.weight, 0);
            }

            // Case 2: peer WAS admitted before the worker stalled (worker already consumed
            // Admitted). Subsequent churn must retain the initial `Retired(peer)` to invalidate
            // old link state, while drops arriving while retired coalesce into `Retired(peer)`.
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Dropped(peer)),
                FactTransition::Queued
            );
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Retired(peer)),
                FactTransition::Queued
            );
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Dropped(peer)),
                FactTransition::Coalesced
            );
            for _ in 0..10_000 {
                assert_eq!(
                    sender.tell(route.clone(), LinkFact::Admitted(peer)),
                    FactTransition::Queued
                );
                assert_eq!(
                    sender.tell(route.clone(), LinkFact::Retired(peer)),
                    FactTransition::WithdrewAdmission
                );
            }
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Admitted(peer)),
                FactTransition::Queued
            );
            assert_eq!(
                sender.tell(route, LinkFact::Dropped(peer)),
                FactTransition::Queued
            );

            let drained: Vec<LinkFact> = std::iter::from_fn(|| sender.shared.lock().take())
                .map(|item| match item {
                    Queued::Fact { fact, .. } => fact,
                    Queued::Envelope { .. } => panic!("expected only link facts"),
                })
                .collect();
            assert!(drained.len() <= NAMESPACE_RUN_FACT_BOUND_PER_PEER);
            assert_eq!(drained, vec![
                LinkFact::Dropped(peer),
                LinkFact::Retired(peer),
                LinkFact::Admitted(peer),
                LinkFact::Dropped(peer),
            ]);
        }

        /// Regression (#1214): queued envelopes pin the `Admitted` / `Retired` facts of the link
        /// generation they arrived in (and per-generation `Dropped` visibility), while unobserved
        /// churn before, between, and after envelopes coalesces within `NAMESPACE_PEER_FACT_BOUND`.
        #[tokio::test]
        async fn test_interleaved_envelopes_preserve_needed_link_facts_and_coalesce_churn() {
            let (_extensions, route) = test_extensions_and_route("interleaved");
            let (sender, _receiver) = channel();
            let peer = Did::from(77_u32);

            // Pre-envelope churn coalesces completely.
            for _ in 0..100 {
                sender.tell(route.clone(), LinkFact::Admitted(peer));
                sender.tell(route.clone(), LinkFact::Retired(peer));
            }

            // Generation 1 carries envelope `e1` and a drop, then retires.
            sender.tell(route.clone(), LinkFact::Admitted(peer));
            sender
                .submit_envelope(route.clone(), peer, Bytes::from_static(b"e1"), 2)
                .expect("admit e1");
            sender.tell(route.clone(), LinkFact::Dropped(peer));
            sender.tell(route.clone(), LinkFact::Dropped(peer));
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Retired(peer)),
                FactTransition::Queued,
                "retirement after a queued envelope must be preserved"
            );

            // 500 unobserved churn cycles between `e1` and `e2` collapse to zero items.
            for _ in 0..500 {
                sender.tell(route.clone(), LinkFact::Admitted(peer));
                sender.tell(route.clone(), LinkFact::Dropped(peer));
                assert_eq!(
                    sender.tell(route.clone(), LinkFact::Retired(peer)),
                    FactTransition::WithdrewAdmission
                );
            }

            // Generation 2 carries envelope `e2` and a fresh drop (not suppressed by gen 1's
            // drop!).
            sender.tell(route.clone(), LinkFact::Admitted(peer));
            sender
                .submit_envelope(route.clone(), peer, Bytes::from_static(b"e2"), 2)
                .expect("admit e2");
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Dropped(peer)),
                FactTransition::Queued
            );
            assert_eq!(
                sender.tell(route.clone(), LinkFact::Retired(peer)),
                FactTransition::Queued
            );

            assert!(sender.shared.lock().items.len() <= NAMESPACE_PEER_FACT_BOUND);

            let drained: Vec<ObservedItem> = std::iter::from_fn(|| sender.shared.lock().take())
                .map(|item| match item {
                    Queued::Envelope { from, payload, .. } => ObservedItem::Payload(from, payload),
                    Queued::Fact { fact, .. } => ObservedItem::Fact(fact),
                })
                .collect();
            assert_eq!(drained, vec![
                ObservedItem::Fact(LinkFact::Admitted(peer)),
                ObservedItem::Payload(peer, Bytes::from_static(b"e1")),
                ObservedItem::Fact(LinkFact::Dropped(peer)),
                ObservedItem::Fact(LinkFact::Retired(peer)),
                ObservedItem::Fact(LinkFact::Admitted(peer)),
                ObservedItem::Payload(peer, Bytes::from_static(b"e2")),
                ObservedItem::Fact(LinkFact::Dropped(peer)),
                ObservedItem::Fact(LinkFact::Retired(peer)),
            ]);
        }

        struct BlockingInterpreter {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            block_first: Mutex<bool>,
            observed: Arc<Mutex<Vec<ObservedItem>>>,
        }

        #[async_trait]
        impl Interpret for BlockingInterpreter {
            type Effect = ObservedItem;

            async fn run(
                &self,
                _scope: &EffectScope,
                effect: Self::Effect,
            ) -> crate::error::Result<Vec<Bytes>> {
                let should_block = {
                    let mut first = self.block_first.lock().expect("block_first");
                    let was_first = *first;
                    *first = false;
                    was_first
                };
                if should_block {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                self.observed.lock().expect("observed").push(effect);
                Ok(Vec::new())
            }
        }

        /// Regression (#1214): when one namespace worker blocks inside a callback under sustained
        /// single-peer churn and interleaved envelopes, a sibling namespace progresses
        /// independently and the stalled namespace remains bounded and delivers coherent state once
        /// unblocked.
        #[tokio::test]
        async fn test_stalled_namespace_bounds_churn_while_sibling_namespace_progresses() {
            let key = DelegateeKey::new_with_seckey(&SecretKey::random()).expect("delegatee key");
            let config = ProcessorConfig::new(1, String::new(), key, 1);
            let processor = ProcessorBuilder::from_config(&config)
                .expect("builder")
                .advertise_presence(false)
                .build()
                .expect("processor");
            let extensions = Extensions::new(Arc::new(processor));

            let stalled_entered = Arc::new(tokio::sync::Notify::new());
            let stalled_release = Arc::new(tokio::sync::Notify::new());
            let stalled_observed = Arc::new(Mutex::new(Vec::new()));
            let fast_observed = Arc::new(Mutex::new(Vec::new()));

            extensions
                .register(
                    TestProtocol {
                        namespace: "stalled",
                    },
                    BlockingInterpreter {
                        entered: Arc::clone(&stalled_entered),
                        release: Arc::clone(&stalled_release),
                        block_first: Mutex::new(true),
                        observed: Arc::clone(&stalled_observed),
                    },
                )
                .expect("register stalled");
            extensions
                .register(TestProtocol { namespace: "fast" }, RecordingInterpreter {
                    observed: Arc::clone(&fast_observed),
                })
                .expect("register fast");

            let drops = Arc::new(DispatchDrops::default());
            let stop = StopSource::new();
            let mut queues =
                NamespaceQueues::new(extensions.core(), Arc::clone(&drops), stop.token());
            let peer = Did::from(55_u32);

            let encode_env = |ns: &str, body: &'static [u8]| -> Bytes {
                Bytes::from(
                    Envelope::new(ns, Bytes::from_static(body))
                        .encode()
                        .expect("encode envelope"),
                )
            };

            // Stall `stalled` on its first `Admitted(peer)` callback.
            queues.submit_link_fact(LinkFact::Admitted(peer));
            tokio::time::timeout(Duration::from_secs(2), stalled_entered.notified())
                .await
                .expect("stalled worker entered callback");

            // Interleave an envelope on the open generation, retire it, then churn 2,000 times
            // while also submitting work to `fast`.
            queues.submit(peer, &encode_env("stalled", b"gen1-msg"));
            queues.submit(peer, &encode_env("fast", b"fast-1"));
            queues.submit_link_fact(LinkFact::Retired(peer));
            for _ in 0..2_000 {
                queues.submit_link_fact(LinkFact::Admitted(peer));
                queues.submit_link_fact(LinkFact::Retired(peer));
            }
            queues.submit_link_fact(LinkFact::Admitted(peer));
            queues.submit(peer, &encode_env("fast", b"fast-2"));

            // `fast` must progress to completion without waiting on `stalled`!
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let len = fast_observed.lock().expect("fast_observed").len();
                    if fast_observed.lock().expect("fast_observed").last()
                        == Some(&ObservedItem::Payload(peer, Bytes::from_static(b"fast-2")))
                    {
                        assert!(len >= 4);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("fast namespace progressed independently");

            // Meanwhile `stalled` is still blocked on item 0 and its queued tail must be tiny!
            {
                let stalled_queue = queues.queues.get("stalled").expect("stalled queue");
                let guard = stalled_queue.shared.lock();
                assert_eq!(
                    guard.items.len(),
                    3,
                    "only [Envelope(gen1-msg), Retired(peer), Admitted(peer)] should remain queued"
                );
            }

            // Release `stalled` and verify exact preserved causal sequence!
            stalled_release.notify_waiters();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if stalled_observed.lock().expect("stalled_observed").len() == 4 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("stalled namespace drained after release");

            assert_eq!(*stalled_observed.lock().expect("stalled_observed"), vec![
                ObservedItem::Fact(LinkFact::Admitted(peer)),
                ObservedItem::Payload(peer, Bytes::from_static(b"gen1-msg")),
                ObservedItem::Fact(LinkFact::Retired(peer)),
                ObservedItem::Fact(LinkFact::Admitted(peer)),
            ]);
            stop.request_stop();
        }
    }
}
