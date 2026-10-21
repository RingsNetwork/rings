//! What the swarm reports about the managed targets, as two records written by the
//! [`Backend`] and read by the dial and the supervisor. A probe's lookup report needs no record:
//! the probe awaits the handle `Swarm::lookup_successor` returns.
//!
//! ```text
//!   BackendObserver::link_fact(Admitted(p)) ─▶ admissions: Admissions ─▶ dial waiting for p
//!   BackendObserver::link_fact(Retired(p))  ─▶ losses: PeerLosses     ─▶ supervisor drain + wake
//! ```
//!
//! "Retired" is the swarm's word for the fact; "loss" is the supervisor's word for what it means
//! to a managed target. The translation happens here, once, in `link_fact`.
//!
//! Each record is bounded: admissions by one waiter per peer in flight (one per dial), losses
//! by the distinct peers that leave between two drains. Writers never block and never fail the
//! [`Backend`]'s observer call: a poisoned record is logged at the observer boundary.
//!
//! [`Backend`]: crate::extension::Backend

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::future::Future;
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
use crate::extension::ext::LinkFact;
use crate::extension::BackendObserver;
use crate::sync_lock::lock;

/// Rendezvous between one dial per peer and that peer's admission, observed once the dial's
/// waiter is registered:
///
/// ```text
///   wait_for(p) ─▶ register waiter ─▶ observe(p) ─▶ resolved
///   observe(p) with no waiter ─▶ dropped
///   drop(AdmissionWaiter(p)) ─▶ waiter forgotten
///                               (idempotent, so a resolved waiter may drop too)
/// ```
///
/// An admission observed before its waiter is not kept: a dial registers first and then checks
/// the transport directly, so an admission committed before the waiter needs no record. A second
/// `wait_for` on the same peer replaces the first waiter, which is then closed. Each
/// registration carries a ticket and a waiter forgets only its own registration, so dropping a
/// replaced or resolved waiter never touches its successor; a turn dropped mid-wait (at
/// shutdown) leaves no registration behind.
#[derive(Default)]
pub(crate) struct Admissions {
    state: Mutex<AdmissionsState>,
}

/// Admission rendezvous state: one waiter per peer.
#[derive(Default)]
struct AdmissionsState {
    /// One waiter per peer, with the ticket that identifies its registration.
    awaiting: HashMap<Did, (u64, oneshot::Sender<()>)>,
    /// Ticket of the next registration.
    next_ticket: u64,
}

impl Admissions {
    /// Resolve the waiter registered for `peer`; dropped when none is.
    pub(super) fn observe(&self, peer: Did) -> Result<()> {
        // A waiter that already gave up and dropped its receiver is simply satisfied late.
        if let Some((_, waiter)) = lock(&self.state)?.awaiting.remove(&peer) {
            let _ = waiter.send(());
        }
        Ok(())
    }

    /// Await the admission of `peer` from now on. The waiter forgets its registration when
    /// dropped.
    pub(crate) fn wait_for(&self, peer: Did) -> Result<AdmissionWaiter<'_>> {
        let mut state = lock(&self.state)?;
        let (sender, receiver) = oneshot::channel();
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.awaiting.insert(peer, (ticket, sender));
        Ok(AdmissionWaiter {
            admissions: self,
            peer,
            ticket,
            receiver,
        })
    }

    /// Drop the registration `ticket` under `peer`; a no-op when the admission already resolved
    /// it or a later waiter replaced it. A poisoned record is left alone: nothing can be
    /// registered in it either.
    fn forget(&self, peer: Did, ticket: u64) {
        if let Ok(mut state) = lock(&self.state) {
            if state
                .awaiting
                .get(&peer)
                .is_some_and(|(registered, _)| *registered == ticket)
            {
                state.awaiting.remove(&peer);
            }
        }
    }

    /// Number of outstanding waiters.
    #[cfg(test)]
    pub(crate) fn len(&self) -> Result<usize> {
        Ok(lock(&self.state)?.awaiting.len())
    }
}

/// A registered dial waiter: resolves once its peer is admitted, and forgets its registration
/// on drop.
pub(crate) struct AdmissionWaiter<'a> {
    admissions: &'a Admissions,
    peer: Did,
    ticket: u64,
    receiver: oneshot::Receiver<()>,
}

impl AdmissionWaiter<'_> {
    /// `Ok` once the admission has arrived, `Empty` while it has not, `Closed` once this waiter
    /// was replaced.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> std::result::Result<(), oneshot::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Future for AdmissionWaiter<'_> {
    type Output = std::result::Result<(), oneshot::error::RecvError>;

    /// Poll the underlying receiver.
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().receiver).poll(context)
    }
}

impl Drop for AdmissionWaiter<'_> {
    /// Forget this registration, whether or not it resolved; a successor's is left alone.
    fn drop(&mut self) {
        self.admissions.forget(self.peer, self.ticket);
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
    /// Resolve a dial waiting for an admission, and record a retirement as a loss for the
    /// supervisor; a drop is no evidence of reachability. A poisoned record is logged, never
    /// propagated.
    fn link_fact(&self, fact: LinkFact) {
        match fact {
            LinkFact::Admitted(peer) => {
                if let Err(error) = self.admissions.observe(peer) {
                    tracing::error!(%peer, %error, "bootstrap admission record unavailable");
                }
            }
            LinkFact::Retired(peer) => {
                if let Err(error) = self.losses.observe(peer) {
                    tracing::error!(%peer, %error, "bootstrap loss record unavailable");
                }
            }
            LinkFact::Dropped(_) => {}
        }
    }
}
