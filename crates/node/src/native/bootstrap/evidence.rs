//! What the swarm reports about the managed targets, as three records written by the
//! [`Backend`] and read by the probe, the dial and the supervisor.
//!
//! ```text
//!   BackendObserver::lookup_report(tx, s) ─▶ reports: Rendezvous<Uuid, Did> ─▶ probe waiting for tx
//!   BackendObserver::peer_admitted(p)     ─▶ admissions: Rendezvous<Did, ()> ─▶ dial waiting for p
//!   BackendObserver::peer_retired(p)      ─▶ losses: PeerLosses            ─▶ supervisor drain + wake
//! ```
//!
//! "Retired" is the swarm's word for the fact; "loss" is the supervisor's word for what it means
//! to a managed target. The translation happens here, once, in `peer_retired`.
//!
//! Each record is bounded: a rendezvous by its early-value capacity plus one waiter per key in
//! flight (one per probe, one per dial), losses by the distinct peers that leave between two
//! drains. Writers never block and never fail the swarm callback: a poisoned record is logged
//! at the observer boundary.
//!
//! [`Backend`]: crate::extension::Backend

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::ops::DerefMut;
use std::sync::Mutex;

use rings_core::dht::Did;
use tokio::sync::futures::Notified;
use tokio::sync::oneshot;
use tokio::sync::Notify;

use crate::error::Result;
use crate::extension::BackendObserver;
use crate::sync_lock::lock;

/// Reports retained while no probe has registered for them, so a report that overtakes the
/// return path of its own send is not lost. Older entries are evicted first; only reports of
/// application lookups reach the record, so the buffer is not diluted by core maintenance.
pub(crate) const EARLY_REPORT_CAPACITY: usize = 32;

/// Rendezvous between one waiter per key and the value observed for that key, in either order:
///
/// ```text
///   wait_for(k) ─┬─ early value buffered? ─▶ resolved at once
///                └─ else register waiter ─▶ observe(k, v) ─▶ resolved
///   observe(k, v) with no waiter ─▶ buffered as an early value (bounded FIFO; dropped at 0)
///   forget(k) ─▶ waiter dropped; idempotent, so a resolved waiter may forget too
/// ```
///
/// A second `wait_for` on the same key replaces the first waiter, which is then closed.
pub(crate) struct Rendezvous<K, V> {
    state: Mutex<RendezvousState<K, V>>,
    early_capacity: usize,
}

/// Rendezvous state: one waiter per key and a bounded FIFO of early values.
struct RendezvousState<K, V> {
    awaiting: HashMap<K, oneshot::Sender<V>>,
    early: VecDeque<(K, V)>,
}

impl<K: Copy + Eq + Hash, V> Rendezvous<K, V> {
    /// A rendezvous that buffers up to `early_capacity` values observed before their waiter.
    pub(crate) fn new(early_capacity: usize) -> Self {
        Self {
            state: Mutex::new(RendezvousState {
                awaiting: HashMap::new(),
                early: VecDeque::new(),
            }),
            early_capacity,
        }
    }

    /// Deliver `value` for `key`: to its waiter when one is registered, otherwise into the
    /// early buffer.
    pub(crate) fn observe(&self, key: K, value: V) -> Result<()> {
        let mut state = lock(&self.state)?;
        match state.awaiting.remove(&key) {
            // A waiter that already gave up and dropped its receiver is simply satisfied late.
            Some(waiter) => {
                let _ = waiter.send(value);
            }
            // Law: the oldest early values are dropped so that at most `early_capacity` are
            // kept; a capacity of zero keeps none.
            None => {
                state.early.push_back((key, value));
                if state.early.len() > self.early_capacity {
                    state.early.pop_front();
                }
            }
        }
        Ok(())
    }

    /// Await the value for `key`; resolves at once when the value arrived first.
    pub(crate) fn wait_for(&self, key: K) -> Result<oneshot::Receiver<V>> {
        let mut state = lock(&self.state)?;
        let (sender, receiver) = oneshot::channel();
        let early = state
            .early
            .iter()
            .position(|(early_key, _)| *early_key == key)
            .and_then(|position| state.early.remove(position));
        match early {
            Some((_, value)) => {
                let _ = sender.send(value);
            }
            None => {
                state.awaiting.insert(key, sender);
            }
        }
        Ok(receiver)
    }

    /// Drop the waiter for `key`; a no-op when the value already resolved it.
    pub(crate) fn forget(&self, key: K) -> Result<()> {
        lock(&self.state)?.awaiting.remove(&key);
        Ok(())
    }

    /// Number of outstanding waiters plus buffered early values.
    #[cfg(test)]
    pub(crate) fn len(&self) -> Result<usize> {
        let state = lock(&self.state)?;
        Ok(state.awaiting.len() + state.early.len())
    }
}

/// Successor lookup reports keyed by the transaction id of their request.
pub(crate) type LookupReportLedger = Rendezvous<uuid::Uuid, Did>;

/// Admissions keyed by peer; a dial checks the transport directly after registering, so no
/// early buffer is needed.
pub(crate) type Admissions = Rendezvous<Did, ()>;

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
    pub(crate) fn observe(&self, peer: Did) -> Result<()> {
        lock(&self.lost)?.insert(peer);
        self.wake.notify_one();
        Ok(())
    }

    /// Take every loss recorded since the previous call.
    pub(crate) fn take(&self) -> Result<BTreeSet<Did>> {
        Ok(std::mem::take(lock(&self.lost)?.deref_mut()))
    }

    /// Resolves once a loss has been recorded since the previous wake.
    pub(crate) fn woken(&self) -> Notified<'_> {
        self.wake.notified()
    }
}

/// What the swarm reports about the managed targets: the successor lookup reports the probe
/// waits for, the admissions the dial waits for, and the retirements the supervisor reacts to.
/// Installed on the [`Backend`] as its [`BackendObserver`].
///
/// [`Backend`]: crate::extension::Backend
pub(crate) struct ReachabilityEvidence {
    reports: LookupReportLedger,
    admissions: Admissions,
    losses: PeerLosses,
}

impl Default for ReachabilityEvidence {
    /// Empty records with the report buffer at [`EARLY_REPORT_CAPACITY`].
    fn default() -> Self {
        Self {
            reports: Rendezvous::new(EARLY_REPORT_CAPACITY),
            admissions: Rendezvous::new(0),
            losses: PeerLosses::default(),
        }
    }
}

impl ReachabilityEvidence {
    /// The lookup-report rendezvous.
    pub(crate) fn reports(&self) -> &LookupReportLedger {
        &self.reports
    }

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
    /// Hand the report to the ledger; a poisoned record is logged, never propagated.
    fn lookup_report(&self, tx_id: uuid::Uuid, successor: Did) {
        if let Err(error) = self.reports.observe(tx_id, successor) {
            tracing::error!(%tx_id, %error, "bootstrap lookup record unavailable");
        }
    }

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
