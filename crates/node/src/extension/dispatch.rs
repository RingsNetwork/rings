//! Per-namespace dispatch of inbound envelopes.
//!
//! ```text
//!   inbox reader ──submit(from, bytes)──▶ queue[namespace] ──worker──▶ Route::dispatch
//!                  (never waits; a refused       (one task per namespace, until stop)
//!                   envelope is dropped, counted)        └──────────▶ Route::observe_link
//!              ──PeerRetired(p)─────────▶ Retired(p), every queue
//!              ──refused from admitted p─▶ Dropped(p), its queue, once while one waits
//! ```
//!
//! Laws:
//! - Each registered namespace's envelopes are dispatched in arrival order, one at a time.
//! - Namespaces are independent: a protocol that waits holds back only its own queue.
//! - Registration is checked once, here: an envelope enters a queue only with the [`Route`] its
//!   namespace resolved to, so only registered namespaces get a queue and the number of tasks
//!   is bounded by the registry.
//! - Each queue is bounded twice: by [`NAMESPACE_QUEUE_CAPACITY`] envelopes, and by
//!   [`NAMESPACE_QUEUE_WEIGHT`] encoded bytes queued or in dispatch. A queue holding no bytes
//!   admits any envelope, so a namespace holds at most `max(NAMESPACE_QUEUE_WEIGHT, one
//!   envelope)`, and the largest envelope the transport carries still gets through.
//! - A [`LinkFact`] is not an envelope: it is queued behind the envelopes that preceded it,
//!   outside both bounds, and never dropped, so a protocol keeping per-link state learns of every
//!   fact in order. A peer's retirement goes to every namespace (#963). An envelope a queue
//!   refuses from an admitted peer makes that namespace a `Dropped` fact for the peer (#966): a
//!   message lost here is observed here, so a protocol whose state rides the link (an onion
//!   circuit) can end it instead of losing a cell silently. At most one `Dropped` per peer waits
//!   in a queue, so facts are bounded by the admissions the core announced; a refusal of a peer
//!   not admitted makes none, since no link state rides a link that is not up. The fact names the
//!   envelope's verified signer, which is the adjacent hop for a direct send; an overlay-routed
//!   envelope from an admitted origin names that origin's link though another carried it, so the
//!   fact may end link state that lost nothing, never miss state that did.
//! - Every envelope that is not dispatched is counted by its [`DispatchDrop`] reason, and logged
//!   at `warn`.
//! - The workers end once the stop of the [`Backend::serve`](super::Backend::serve) that started
//!   them is requested, abandoning what they still hold.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::FutureExt;
use futures::StreamExt;
use rings_core::dht::Did;
use rings_core::lifecycle::StopToken;
use rings_core::swarm::application::APPLICATION_INBOX_WEIGHT;
use serde::Serialize;

use crate::extension::ext::Core;
use crate::extension::ext::Envelope;
use crate::extension::ext::LinkFact;
use crate::extension::ext::Route;
use crate::sync_lock::lock;

/// Envelopes one namespace may hold before further ones are dropped.
const NAMESPACE_QUEUE_CAPACITY: usize = 256;

/// Encoded bytes one namespace may hold, queued or in dispatch, before further envelopes are
/// dropped: a quarter of the core inbox's [`APPLICATION_INBOX_WEIGHT`], so the few built-in
/// namespaces together park about what the inbox itself retains.
const NAMESPACE_QUEUE_WEIGHT: usize = APPLICATION_INBOX_WEIGHT / 4;

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

/// What one namespace's queue holds, queued or in dispatch.
#[derive(Default)]
struct Held {
    /// Envelopes; retirements are not counted.
    envelopes: AtomicUsize,
    /// Encoded envelope bytes.
    weight: AtomicUsize,
    /// Peers whose [`LinkFact::Dropped`] waits in the queue, not yet taken by the worker.
    dropped: Mutex<HashSet<Did>>,
}

impl Held {
    /// Whether this queue admits one more envelope of `weight` bytes.
    fn admits(&self, weight: usize) -> bool {
        self.envelopes.load(Ordering::Acquire) < NAMESPACE_QUEUE_CAPACITY
            && admits(self.weight.load(Ordering::Acquire), weight)
    }

    /// Take one envelope of `weight` bytes.
    fn take(&self, weight: usize) {
        self.envelopes.fetch_add(1, Ordering::AcqRel);
        self.weight.fetch_add(weight, Ordering::AcqRel);
    }

    /// Release one envelope of `weight` bytes.
    fn release(&self, weight: usize) {
        self.envelopes.fetch_sub(1, Ordering::AcqRel);
        self.weight.fetch_sub(weight, Ordering::AcqRel);
    }

    /// Mark a drop from `peer`; `true` iff no `Dropped(peer)` was waiting, so one must be queued.
    /// A poisoned set coalesces nothing: every drop is then told.
    fn mark_dropped(&self, peer: Did) -> bool {
        lock(&self.dropped).map_or(true, |mut waiting| waiting.insert(peer))
    }

    /// Unmark `peer` as its `Dropped` fact is taken, so a later drop is told again.
    fn unmark_dropped(&self, peer: Did) {
        if let Ok(mut waiting) = lock(&self.dropped) {
            waiting.remove(&peer);
        }
    }
}

/// One namespace's queue: the sender into it, and what it holds.
struct NamespaceQueue {
    sender: mpsc::UnboundedSender<Queued>,
    held: Arc<Held>,
}

/// Whether a queue holding `held` bytes admits an envelope of `weight` more: always when it
/// holds none.
fn admits(held: usize, weight: usize) -> bool {
    held == 0 || held.saturating_add(weight) <= NAMESPACE_QUEUE_WEIGHT
}

/// The dispatch queue of every namespace that has received an envelope.
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
    pub(super) fn submit(&mut self, from: Did, bytes: &[u8]) {
        let Ok(envelope) = Envelope::decode(bytes) else {
            self.drops.record(DispatchDrop::Undecodable);
            return;
        };
        let Some(route) = self.core.route(envelope.namespace.as_str()) else {
            self.drops.record(DispatchDrop::Unregistered);
            return;
        };
        let queue = queue_of(&mut self.queues, &self.stop, envelope.namespace);
        let weight = bytes.len();
        if !queue.held.admits(weight) {
            self.drops.record(DispatchDrop::QueueFull);
            if self.core.peer_admitted(from) && queue.held.mark_dropped(from) {
                queue.tell(route, LinkFact::Dropped(from));
            }
            return;
        }
        // Only this reader takes; the worker only releases, so the check holds.
        queue.held.take(weight);
        let queued = Queued::Envelope {
            route,
            from,
            payload: envelope.payload,
            weight,
        };
        if queue.sender.unbounded_send(queued).is_err() {
            queue.held.release(weight);
            self.drops.record(DispatchDrop::Unserved);
        }
    }

    /// Queue the retirement of `peer`'s link generation for every registered namespace, behind
    /// the envelopes already queued. Never waits and never refuses for room.
    pub(super) fn submit_retirement(&mut self, peer: Did) {
        for (namespace, route) in self.core.routes() {
            queue_of(&mut self.queues, &self.stop, namespace).tell(route, LinkFact::Retired(peer));
        }
    }
}

impl NamespaceQueue {
    /// Queue `fact` for `route`, outside both bounds.
    fn tell(&self, route: Route, fact: LinkFact) {
        if self
            .sender
            .unbounded_send(Queued::Fact { route, fact })
            .is_err()
        {
            tracing::error!(?fact, "no worker to deliver a link fact");
        }
    }
}

/// The queue of `namespace` in `queues`, starting its worker, which ends once `stop` is
/// requested, on first use.
fn queue_of<'queues>(
    queues: &'queues mut HashMap<String, NamespaceQueue>,
    stop: &StopToken,
    namespace: String,
) -> &'queues NamespaceQueue {
    queues
        .entry(namespace)
        .or_insert_with(|| spawn_worker(stop.clone()))
}

/// Start the worker of one namespace: it dispatches its envelopes in order until its queue is
/// dropped or `stop` is requested. Without a runtime to run it, the queue is returned closed,
/// so every envelope is dropped and counted.
fn spawn_worker(stop: StopToken) -> NamespaceQueue {
    let (sender, receiver) = mpsc::unbounded::<Queued>();
    let held = Arc::new(Held::default());
    let drain = drain(receiver, Arc::clone(&held));
    let worker = async move {
        let stopped = stop.stopped().fuse();
        let drain = drain.fuse();
        futures::pin_mut!(stopped, drain);
        futures::select_biased! {
            () = stopped => {},
            () = drain => {},
        }
    };
    if rings_runtime::spawn_detached(worker).is_err() {
        tracing::error!("no runtime to run a namespace dispatch worker");
    }
    NamespaceQueue { sender, held }
}

/// Dispatch every item of `receiver` in order, releasing each envelope from `held` once its
/// dispatch ends, and unmarking each `Dropped` fact as it is taken.
async fn drain(mut receiver: mpsc::UnboundedReceiver<Queued>, held: Arc<Held>) {
    while let Some(queued) = receiver.next().await {
        let dispatched = match queued {
            Queued::Envelope {
                route,
                from,
                payload,
                weight,
            } => {
                let dispatched = route.dispatch(from, payload).await;
                held.release(weight);
                dispatched
            }
            Queued::Fact { route, fact } => {
                if let LinkFact::Dropped(peer) = fact {
                    held.unmark_dropped(peer);
                }
                route.observe_link(fact).await
            }
        };
        if let Err(error) = dispatched {
            tracing::warn!(error = ?error, "protocol dispatch failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use rings_core::dht::Did;

    use super::admits;
    use super::DispatchDrop;
    use super::DispatchDropCounters;
    use super::DispatchDrops;
    use super::Held;
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
        let held = Held::default();
        for _ in 0..NAMESPACE_QUEUE_CAPACITY {
            assert!(held.admits(1));
            held.take(1);
        }
        assert!(!held.admits(1));
        held.release(1);
        assert!(held.admits(1));
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

    /// Law (#966): at most one `Dropped` fact per peer waits in a queue; once the worker takes
    /// it, a later drop from that peer is told again.
    #[test]
    fn test_a_drop_is_told_once_while_its_fact_waits() {
        let held = Held::default();
        let peer = Did::from(7_u32);
        assert!(held.mark_dropped(peer));
        assert!(!held.mark_dropped(peer));
        assert!(held.mark_dropped(Did::from(8_u32)));
        held.unmark_dropped(peer);
        assert!(held.mark_dropped(peer));
    }
}
