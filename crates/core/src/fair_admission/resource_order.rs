//! Resource-ordered admission: first come first served per exhausted resource.
//!
//! A request belongs to a *stream*, whose requests must be granted in arrival order, and draws
//! on a set of resources, its *footprint*; a ledger either admits it or names the exhausted
//! resources that *refused* it, a subset of the footprint. Waiting requests are kept in arrival
//! order, and each is judged against the waiters before it:
//!
//! ```text
//!   blocked(w) ⟺ ∃ v < w ungranted:  stream(v) = stream(w) ∨ hold(v) ∩ footprint(w) ≠ ∅
//!                                                                   ⇒  w waits
//!   otherwise   try_admit(w) = Ok(g)   ⇒  grant g to w
//!               try_admit(w) = Err(R)  ⇒  w waits, refused by R
//! ```
//!
//! where a refused waiter holds its refusal, and a waiter blocked only for lack of room that a
//! *relief* of its footprint may make ([`AdmissionLedger::reliefs`]) holds those reliefs, so it
//! is served first come first served on them ([`Waitlist`]).
//!
//! An event judges only the waiters whose blockers it may lift, through the indices of
//! [`Waitlist`]: `O((g + k + r) log n)` per event, `g` the requests it grants, `r` the
//! resources it names, `k` the waiters it moves to another blocker (load-bounds paper,
//! Obligation "Admission complexity", #938).
//!
//! Laws, model checked in `test_resource_order_model`:
//!
//! - *No overtaking on an exhausted resource.* A request refused by resource `r` is never passed
//!   by a later request whose footprint contains `r`: every later request that draws on `r` is
//!   blocked while it waits. Likewise on a relief a blocked request holds.
//! - *Independence.* Only refusal and stream order requests: a request of another stream whose
//!   footprint is disjoint from every earlier waiter's hold is attempted at once. A request
//!   blocked by its stream or by a budget of its own holds only its stream; so requests of
//!   other streams on disjoint resources never wait for one another.
//! - *Fixpoint.* Every arrival, release and withdrawal judges again every waiter whose blocker
//!   it may lift, so afterwards no waiting request is both unblocked and admissible, and no
//!   wake-up is lost. A release names the resources it freed.
//! - *Stream order.* The requests of one stream are granted in arrival order: a waiting request,
//!   blocked or refused, blocks its stream.
//! - *Liveness.* When every grant is eventually released, every waiting request is eventually
//!   granted.
//!
//! A single first-in-first-out queue satisfies the first law but not independence: its head
//! holds back requests on resources it does not need. Admission that ignores refusals satisfies
//! independence but not the first law: a stream of small requests starves a large one.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use super::waitlist::Arrival;
use super::waitlist::Served;
use super::waitlist::Waitlist;

/// The capacity a [`ResourceOrderedQueue`] admits against.
///
/// Law (refusals are budgets): a refusal names one budget's resources, the same set whenever
/// that budget refuses, so two refusals that share a resource are equal.
pub(crate) trait AdmissionLedger {
    /// What a waiter asks for.
    type Request;
    /// What an admitted request holds. Dropping it releases its capacity and serves the queue
    /// ([`ResourceOrderedQueue::release`]) with the resources it freed, outside the ledger's
    /// lock.
    type Grant;
    /// One resource a request may draw on.
    type Resource: Ord + Clone;
    /// A set of resources.
    type Resources: IntoIterator<Item = Self::Resource>;

    /// The stream `request` belongs to, whose requests are granted in arrival order; a stream is
    /// a resource of its own, disjoint from every footprint.
    fn stream(&self, request: &Self::Request) -> Self::Resource;

    /// The resources `request` draws on, were it attempted now.
    fn footprint(&self, request: &Self::Request) -> Self::Resources;

    /// The reliefs of `request`'s footprint as it stands: each resource `ρ` of it whose
    /// release may shed resources of the footprint, with those resources.
    ///
    /// Law: `request`'s footprint changes only by an admission, or by a release that names
    /// one of its resources; a release that sheds a resource from it names its relief.
    fn reliefs(&self, request: &Self::Request) -> Vec<(Self::Resource, Vec<Self::Resource>)>;

    /// Admit `request`, or name the exhausted resources of its footprint that refused it.
    fn try_admit(&self, request: &Self::Request) -> Result<Self::Grant, Self::Resources>;
}

/// Where one request of a [`ResourceOrderedQueue`] stands, in the slot its waiter owns.
enum SlotState<G> {
    /// Queued: the waker to wake once a judgement grants it.
    Queued(Option<Waker>),
    /// Granted by a pass and left the queue; its waiter has not claimed it yet.
    Granted(G),
    /// Claimed by its waiter.
    Claimed,
}

/// The slot of one request, shared by its queue entry and its waiter: a pass moves the grant
/// into it, so the waiter claims it without searching the queue.
struct GrantSlot<G>(Mutex<SlotState<G>>);

impl<G> GrantSlot<G> {
    /// The slot's state; every critical section leaves it consistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState<G>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Move `grant` in, returning the waker of the waiter to wake.
    fn grant(&self, grant: G) -> Option<Waker> {
        match std::mem::replace(&mut *self.lock(), SlotState::Granted(grant)) {
            SlotState::Queued(waker) => waker,
            SlotState::Granted(_) | SlotState::Claimed => None,
        }
    }

    /// Claim the grant, if a pass moved one in; otherwise register `waker`, if any, while the
    /// request is queued.
    fn claim(&self, waker: Option<&Waker>) -> Option<G> {
        let mut state = self.lock();
        match std::mem::replace(&mut *state, SlotState::Claimed) {
            SlotState::Granted(grant) => Some(grant),
            SlotState::Queued(registered) => {
                *state = SlotState::Queued(waker.cloned().or(registered));
                None
            }
            SlotState::Claimed => None,
        }
    }
}

/// The waiting requests and the slots their waiters own.
struct QueueState<L: AdmissionLedger> {
    /// The ungranted requests, indexed.
    waitlist: Waitlist<L::Request, L::Resource>,
    /// The slot of each ungranted request's waiter.
    slots: BTreeMap<Arrival, Arc<GrantSlot<L::Grant>>>,
}

impl<L: AdmissionLedger> QueueState<L> {
    /// Move the grants of `served` into their slots, and collect the wakers of the waiters
    /// granted.
    fn deliver(&mut self, served: Served<L::Grant>) -> Vec<Waker> {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        rings_transport::hot_path::record(
            rings_transport::hot_path::HotPath::QueueService,
            served.visits,
        );
        served
            .granted
            .into_iter()
            .filter_map(|(arrival, grant)| self.slots.remove(&arrival)?.grant(grant))
            .collect()
    }
}

/// Requests waiting for a ledger's capacity, admitted by resource order (module docs).
///
/// Lock order: the queue's lock is taken before the ledger's and before a slot's, so a ledger
/// must never call back into its queue while it holds its own lock; a release calls
/// [`Self::release`] only after the ledger's lock is dropped.
pub(crate) struct ResourceOrderedQueue<L: AdmissionLedger> {
    /// The waiting requests.
    state: Mutex<QueueState<L>>,
}

impl<L: AdmissionLedger> ResourceOrderedQueue<L> {
    /// An empty queue.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                waitlist: Waitlist::default(),
                slots: BTreeMap::new(),
            }),
        }
    }

    /// Lock the waiting requests.
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState<L>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ledger freed `resources`: judge again the waiters they may unblock, and wake those
    /// granted.
    pub(crate) fn release(&self, ledger: &L, resources: impl IntoIterator<Item = L::Resource>) {
        let wakers = {
            let mut state = self.lock();
            let served = state.waitlist.release(ledger, resources);
            state.deliver(served)
        };
        wakers.into_iter().for_each(Waker::wake);
    }

    /// The number of requests waiting ungranted.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn waiting_for_test(&self) -> usize {
        self.lock().waitlist.len()
    }

    /// Admit `request`, waiting in resource order while it is refused or blocked; `on_wait`
    /// runs once, if the request has to wait.
    ///
    /// Cancel safe: dropping the future withdraws the request, releases a grant it was given
    /// but had not claimed, and judges again what its withdrawal may unblock.
    pub(crate) async fn acquire(
        &self,
        ledger: &L,
        request: L::Request,
        on_wait: impl FnOnce(),
    ) -> L::Grant {
        let slot = Arc::new(GrantSlot(Mutex::new(SlotState::Queued(None))));
        let (arrival, wakers) = {
            let mut state = self.lock();
            let (arrival, served) = state.waitlist.arrive(ledger, request);
            state.slots.insert(arrival, Arc::clone(&slot));
            (arrival, state.deliver(served))
        };
        wakers.into_iter().for_each(Waker::wake);
        if let Some(grant) = slot.claim(None) {
            return grant;
        }
        on_wait();
        let waiter = Waiter {
            queue: self,
            ledger,
            arrival,
            slot,
        };
        poll_fn(|context| waiter.poll_grant(context)).await
    }
}

/// A request waiting in a [`ResourceOrderedQueue`]; dropping it withdraws the request.
struct Waiter<'a, L: AdmissionLedger> {
    /// The queue the request waits in.
    queue: &'a ResourceOrderedQueue<L>,
    /// The ledger a withdrawal serves.
    ledger: &'a L,
    /// The request's arrival in the queue.
    arrival: Arrival,
    /// The slot a judgement moves the request's grant into.
    slot: Arc<GrantSlot<L::Grant>>,
}

impl<L: AdmissionLedger> Waiter<'_, L> {
    /// Claim the grant once a judgement has moved it into the slot; otherwise stay registered
    /// to be woken. Polled again after it was ready, it stays pending.
    fn poll_grant(&self, context: &mut Context<'_>) -> Poll<L::Grant> {
        match self.slot.claim(Some(context.waker())) {
            Some(grant) => Poll::Ready(grant),
            None => Poll::Pending,
        }
    }
}

impl<L: AdmissionLedger> Drop for Waiter<'_, L> {
    fn drop(&mut self) {
        // Withdrawn under the queue's lock, released before the grant is: a grant's release
        // serves the queue itself, while a withdrawn refusal releases nothing and is judged here.
        let wakers = {
            let mut state = self.queue.lock();
            state.slots.remove(&self.arrival);
            let (_, served) = state.waitlist.withdraw(self.ledger, self.arrival);
            state.deliver(served)
        };
        wakers.into_iter().for_each(Waker::wake);
        if let Some(grant) = self.slot.claim(None) {
            drop(grant);
        }
    }
}
