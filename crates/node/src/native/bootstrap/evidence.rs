//! What the swarm reports about the managed targets, as three rendezvous records written by
//! the [`Backend`] and read by the probe, the dial and the supervisor.
//!
//! ```text
//!   BackendObserver::lookup_report(tx, s) ─▶ LookupReportLedger ─▶ probe awaiting tx
//!   BackendObserver::peer_admitted(p)     ─▶ Admissions         ─▶ dial awaiting p
//!   BackendObserver::peer_retired(p)      ─▶ PeerLosses         ─▶ supervisor drain + wake
//! ```
//!
//! Each record is bounded: the ledger by its early-report capacity plus one waiter per probe,
//! admissions by one waiter per in-flight dial, losses by the distinct peers that leave between
//! two drains. Writers never block and never fail the swarm callback: a poisoned record is
//! logged at the observer boundary.
//!
//! [`Backend`]: crate::extension::Backend

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
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
/// application lookups reach the ledger, so the buffer is not diluted by core maintenance.
pub(crate) const EARLY_REPORT_CAPACITY: usize = 32;

/// Rendezvous between lookup probes and the reports they wait for, in either order:
///
/// ```text
///   send ─▶ tx ─▶ await_report(tx) ─┬─ early report buffered? ─▶ resolved at once
///                                   └─ else register waiter ─▶ observe(tx, s) ─▶ resolved
///   observe(tx, s) with no waiter ─▶ buffered as an early report (bounded FIFO)
///   forget(tx) ─▶ waiter dropped; idempotent, so a resolved probe may call it too
/// ```
#[derive(Default)]
pub(crate) struct LookupReportLedger {
    state: Mutex<LedgerState>,
}

/// Ledger state: one waiter per outstanding transaction and a bounded FIFO of early reports.
#[derive(Default)]
struct LedgerState {
    awaiting: HashMap<uuid::Uuid, oneshot::Sender<Did>>,
    early: VecDeque<(uuid::Uuid, Did)>,
}

impl LookupReportLedger {
    /// Deliver the report for `tx_id`: to its waiter when one is registered, otherwise into
    /// the early buffer.
    pub(crate) fn observe(&self, tx_id: uuid::Uuid, successor: Did) -> Result<()> {
        let mut state = lock(&self.state)?;
        match state.awaiting.remove(&tx_id) {
            // A waiter that already timed out and dropped its receiver is simply satisfied late.
            Some(waiter) => {
                let _ = waiter.send(successor);
            }
            None => {
                if state.early.len() >= EARLY_REPORT_CAPACITY {
                    state.early.pop_front();
                }
                state.early.push_back((tx_id, successor));
            }
        }
        Ok(())
    }

    /// Await the report for `tx_id`; resolves at once when the report arrived first.
    pub(crate) fn await_report(&self, tx_id: uuid::Uuid) -> Result<oneshot::Receiver<Did>> {
        let mut state = lock(&self.state)?;
        let (sender, receiver) = oneshot::channel();
        let early = state
            .early
            .iter()
            .position(|(early_tx_id, _)| *early_tx_id == tx_id)
            .and_then(|position| state.early.remove(position));
        match early {
            Some((_, successor)) => {
                let _ = sender.send(successor);
            }
            None => {
                state.awaiting.insert(tx_id, sender);
            }
        }
        Ok(receiver)
    }

    /// Drop the waiter for `tx_id`; a no-op when the report already resolved it.
    pub(crate) fn forget(&self, tx_id: uuid::Uuid) -> Result<()> {
        lock(&self.state)?.awaiting.remove(&tx_id);
        Ok(())
    }

    /// Number of outstanding waiters plus buffered early reports.
    #[cfg(test)]
    pub(crate) fn len(&self) -> Result<usize> {
        let state = lock(&self.state)?;
        Ok(state.awaiting.len() + state.early.len())
    }
}

/// Rendezvous between a dial and the admission of the peer it handshook with. One waiter per
/// peer: a dial registers before it checks the transport directly, so an admission that lands
/// between the two is observed by the check and a later one by the waiter.
#[derive(Default)]
pub(crate) struct Admissions {
    waiting: Mutex<HashMap<Did, oneshot::Sender<()>>>,
}

impl Admissions {
    /// Resolve the waiter for `peer`, if any.
    pub(crate) fn observe(&self, peer: Did) -> Result<()> {
        if let Some(waiter) = lock(&self.waiting)?.remove(&peer) {
            let _ = waiter.send(());
        }
        Ok(())
    }

    /// Await the admission of `peer`, replacing any earlier waiter for it.
    pub(crate) fn await_admission(&self, peer: Did) -> Result<oneshot::Receiver<()>> {
        let (sender, receiver) = oneshot::channel();
        lock(&self.waiting)?.insert(peer, sender);
        Ok(receiver)
    }

    /// Drop the waiter for `peer`; a no-op when the admission already resolved it.
    pub(crate) fn forget(&self, peer: Did) -> Result<()> {
        lock(&self.waiting)?.remove(&peer);
        Ok(())
    }
}

/// Peers that left the local DHT since the supervisor last drained, with a wake for the
/// supervisor. `notify_one` stores a permit when nobody waits, so a loss recorded while the
/// supervisor is between its drain and its wait is not lost.
#[derive(Default)]
pub(crate) struct PeerLosses {
    lost: Mutex<BTreeSet<Did>>,
    wake: Notify,
}

impl PeerLosses {
    /// Record the departure of `peer` and wake the supervisor.
    pub(crate) fn observe(&self, peer: Did) -> Result<()> {
        lock(&self.lost)?.insert(peer);
        self.wake.notify_one();
        Ok(())
    }

    /// Take every loss recorded since the previous call.
    pub(crate) fn take(&self) -> Result<BTreeSet<Did>> {
        Ok(std::mem::take(&mut *lock(&self.lost)?))
    }

    /// Resolves once a loss has been recorded since the previous wake.
    pub(crate) fn woken(&self) -> Notified<'_> {
        self.wake.notified()
    }
}

/// What the swarm reports about the managed targets: the successor lookup reports the probe
/// waits for, the admissions the dial waits for, and the departures the supervisor reacts to.
/// Installed on the [`Backend`] as its [`BackendObserver`].
///
/// [`Backend`]: crate::extension::Backend
#[derive(Default)]
pub struct ReachabilityEvidence {
    reports: LookupReportLedger,
    admissions: Admissions,
    losses: PeerLosses,
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

    /// The departure record.
    pub(crate) fn losses(&self) -> &PeerLosses {
        &self.losses
    }
}

impl BackendObserver for ReachabilityEvidence {
    /// Hand the report to the ledger; a poisoned ledger is logged, never propagated.
    fn lookup_report(&self, tx_id: uuid::Uuid, successor: Did) {
        if let Err(error) = self.reports.observe(tx_id, successor) {
            tracing::error!(%tx_id, %error, "bootstrap lookup ledger unavailable");
        }
    }

    /// Resolve a dial waiting for this admission; a poisoned record is logged, never propagated.
    fn peer_admitted(&self, peer: Did) {
        if let Err(error) = self.admissions.observe(peer) {
            tracing::error!(%peer, %error, "bootstrap admission record unavailable");
        }
    }

    /// Record the departure for the supervisor; a poisoned record is logged, never propagated.
    fn peer_retired(&self, peer: Did) {
        if let Err(error) = self.losses.observe(peer) {
            tracing::error!(%peer, %error, "bootstrap loss record unavailable");
        }
    }
}
