//! What the swarm reports about the managed targets, as two records written by the
//! [`Backend`] and read by the dial and the supervisor. A probe's lookup report needs no record:
//! the probe awaits the handle `Swarm::lookup_successor` returns.
//!
//! ```text
//!   BackendObserver::peer_admitted(p) ─▶ admissions: Rendezvous<Did, ()> ─▶ dial waiting for p
//!   BackendObserver::peer_retired(p)  ─▶ losses: PeerLosses              ─▶ supervisor drain + wake
//! ```
//!
//! "Retired" is the swarm's word for the fact; "loss" is the supervisor's word for what it means
//! to a managed target. The translation happens here, once, in `peer_retired`.
//!
//! Each record is bounded: a rendezvous by one waiter per key in flight (one per dial), losses
//! by the distinct peers that leave between two drains. Writers never block and never fail the
//! [`Backend`]'s observer call: a poisoned record is logged at the observer boundary.
//!
//! [`Backend`]: crate::extension::Backend

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::ops::DerefMut;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use rings_core::dht::Did;
use tokio::sync::futures::Notified;
use tokio::sync::oneshot;
use tokio::sync::Notify;

use crate::error::Result;
use crate::extension::BackendObserver;
use crate::sync_lock::lock;

/// Rendezvous between one waiter per key and the value observed for that key once the waiter
/// is registered:
///
/// ```text
///   wait_for(k) ─▶ register waiter ─▶ observe(k, v) ─▶ resolved
///   observe(k, v) with no waiter ─▶ dropped
///   drop(Waiter(k)) ─▶ waiter forgotten; idempotent, so a resolved waiter may drop too
/// ```
///
/// A value observed before its waiter is not kept: a caller that may race the value registers
/// first and then checks the source of truth (see [`Admissions`]). A second `wait_for` on the
/// same key replaces the first waiter, which is then closed. Each registration carries a ticket
/// and a waiter forgets only its own registration, so dropping a replaced or resolved waiter
/// never touches its successor; a turn dropped mid-wait (at shutdown) leaves no registration
/// behind.
pub(crate) struct Rendezvous<K, V> {
    state: Mutex<RendezvousState<K, V>>,
}

/// Rendezvous state: one waiter per key.
struct RendezvousState<K, V> {
    /// One waiter per key, with the ticket that identifies its registration.
    awaiting: HashMap<K, (u64, oneshot::Sender<V>)>,
    /// Ticket of the next registration.
    next_ticket: u64,
}

impl<K, V> Default for Rendezvous<K, V> {
    /// A rendezvous with no waiter registered.
    fn default() -> Self {
        Self {
            state: Mutex::new(RendezvousState {
                awaiting: HashMap::new(),
                next_ticket: 0,
            }),
        }
    }
}

impl<K: Copy + Eq + Hash, V> Rendezvous<K, V> {
    /// Deliver `value` to the waiter registered for `key`; dropped when none is.
    pub(super) fn observe(&self, key: K, value: V) -> Result<()> {
        // A waiter that already gave up and dropped its receiver is simply satisfied late.
        if let Some((_, waiter)) = lock(&self.state)?.awaiting.remove(&key) {
            let _ = waiter.send(value);
        }
        Ok(())
    }

    /// Await the value observed for `key` from now on. The waiter forgets its registration
    /// when dropped.
    pub(crate) fn wait_for(&self, key: K) -> Result<Waiter<'_, K, V>> {
        let mut state = lock(&self.state)?;
        let (sender, receiver) = oneshot::channel();
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.awaiting.insert(key, (ticket, sender));
        Ok(Waiter {
            rendezvous: self,
            key,
            ticket,
            receiver,
        })
    }

    /// Drop the registration `ticket` under `key`; a no-op when the value already resolved it
    /// or a later waiter replaced it. A poisoned record is left alone: nothing can be registered
    /// in it either.
    fn forget(&self, key: K, ticket: u64) {
        if let Ok(mut state) = lock(&self.state) {
            if state
                .awaiting
                .get(&key)
                .is_some_and(|(registered, _)| *registered == ticket)
            {
                state.awaiting.remove(&key);
            }
        }
    }

    /// Number of outstanding waiters.
    #[cfg(test)]
    pub(crate) fn len(&self) -> Result<usize> {
        Ok(lock(&self.state)?.awaiting.len())
    }
}

/// A registered waiter: resolves to the value observed for its key, and forgets its
/// registration on drop.
pub(crate) struct Waiter<'a, K: Copy + Eq + Hash, V> {
    rendezvous: &'a Rendezvous<K, V>,
    key: K,
    ticket: u64,
    receiver: oneshot::Receiver<V>,
}

impl<K: Copy + Eq + Hash, V> Waiter<'_, K, V> {
    /// The value if it has arrived, `Empty` while it has not, `Closed` once this waiter was
    /// replaced.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> std::result::Result<V, oneshot::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl<K: Copy + Eq + Hash + Unpin, V> Future for Waiter<'_, K, V> {
    type Output = std::result::Result<V, oneshot::error::RecvError>;

    /// Poll the underlying receiver.
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().receiver).poll(context)
    }
}

impl<K: Copy + Eq + Hash, V> Drop for Waiter<'_, K, V> {
    /// Forget this registration, whether or not it resolved; a successor's is left alone.
    fn drop(&mut self) {
        self.rendezvous.forget(self.key, self.ticket);
    }
}

/// Admissions keyed by peer; a dial checks the transport directly after registering, so an
/// admission announced before the waiter needs no record.
pub(super) type Admissions = Rendezvous<Did, ()>;

/// Peers that left the local DHT since the supervisor last drained, with a wake for the
/// supervisor. `notify_one` stores a permit when nobody waits, so a loss recorded while the
/// supervisor is between its drain and its wait is not lost.
#[derive(Default)]
pub(crate) struct PeerLosses {
    lost: Mutex<BTreeSet<Did>>,
    wake: Notify,
}

impl PeerLosses {
    /// Record the retirement of `peer` as a loss and wake the supervisor.
    pub(super) fn observe(&self, peer: Did) -> Result<()> {
        lock(&self.lost)?.insert(peer);
        self.wake.notify_one();
        Ok(())
    }

    /// Take every loss recorded since the previous call.
    pub(crate) fn take(&self) -> Result<BTreeSet<Did>> {
        Ok(std::mem::take(lock(&self.lost)?.deref_mut()))
    }

    /// Resolves once a loss has been recorded since the previous wake.
    pub(super) fn woken(&self) -> Notified<'_> {
        self.wake.notified()
    }
}

/// What the swarm reports about the managed targets: the admissions the dial waits for and the
/// retirements the supervisor reacts to. Installed on the [`Backend`] as its
/// [`BackendObserver`].
///
/// [`Backend`]: crate::extension::Backend
#[derive(Default)]
pub(crate) struct ReachabilityEvidence {
    admissions: Admissions,
    losses: PeerLosses,
}

impl ReachabilityEvidence {
    /// The admission rendezvous.
    pub(crate) fn admissions(&self) -> &Admissions {
        &self.admissions
    }

    /// The record of retired targets.
    pub(crate) fn losses(&self) -> &PeerLosses {
        &self.losses
    }
}

impl BackendObserver for ReachabilityEvidence {
    /// Resolve a dial waiting for this admission; a poisoned record is logged, never propagated.
    fn peer_admitted(&self, peer: Did) {
        if let Err(error) = self.admissions.observe(peer, ()) {
            tracing::error!(%peer, %error, "bootstrap admission record unavailable");
        }
    }

    /// Record the retirement as a loss for the supervisor; a poisoned record is logged, never
    /// propagated.
    fn peer_retired(&self, peer: Did) {
        if let Err(error) = self.losses.observe(peer) {
            tracing::error!(%peer, %error, "bootstrap loss record unavailable");
        }
    }
}
