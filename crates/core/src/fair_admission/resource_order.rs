//! Resource-ordered admission: first come first served per exhausted resource.
//!
//! A request belongs to a *stream*, whose requests must be granted in arrival order, and draws
//! on a set of resources, its *footprint*; a ledger either admits it or names the exhausted
//! resources that *refused* it, a subset of the footprint. Waiting requests are kept in arrival
//! order, and one service pass visits them in that order, accumulating the set `E`
//! of resources taken by earlier waiters:
//!
//! ```text
//!   pass(w₁ … wₙ) :  E₀ = ∅
//!     for i in 1..n, wᵢ not yet granted:
//!       ({sᵢ} ∪ footprint(wᵢ)) ∩ Eᵢ₋₁ ≠ ∅  ⇒  skip            (Eᵢ = Eᵢ₋₁ ∪ {sᵢ})
//!       try_admit(wᵢ) = Ok(g)              ⇒  grant g to wᵢ   (Eᵢ = Eᵢ₋₁)
//!       try_admit(wᵢ) = Err(R)             ⇒  wait            (Eᵢ = Eᵢ₋₁ ∪ {sᵢ} ∪ R)
//!   where sᵢ = stream(wᵢ)
//! ```
//!
//! Laws, model checked in `test_resource_order_model`:
//!
//! - *No overtaking on an exhausted resource.* A request refused by resource `r` is never passed
//!   by a later request whose footprint contains `r`: within a pass, every later request that
//!   draws on `r` is skipped, and the next pass visits the earlier request first.
//! - *Independence.* Only refusal and stream order requests: a request of another stream whose
//!   footprint is disjoint from every earlier waiter's refusal is attempted at once. A skipped
//!   request contributes only its own stream to `E`, since no resource refused it; so requests of
//!   other streams on disjoint resources never wait for one another.
//! - *Fixpoint.* The queue is served ([`serve`]) by repeating the pass until one grants
//!   nothing, so afterwards no waiting request is both unblocked and admissible; it is served on
//!   every arrival, every release and every cancellation, so no wake-up is lost.
//! - *Stream order.* The requests of one stream are granted in arrival order: a waiting request,
//!   skipped or refused, blocks its stream for the rest of the pass.
//! - *Liveness.* When every grant is eventually released, every waiting request is eventually
//!   granted.
//!
//! A single first-in-first-out queue satisfies the first law but not independence: its head
//! holds back requests on resources it does not need. Admission that ignores refusals satisfies
//! independence but not the first law: a stream of small requests starves a large one.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// The capacity a [`ResourceOrderedQueue`] admits against.
pub(crate) trait AdmissionLedger {
    /// What a waiter asks for.
    type Request;
    /// What an admitted request holds. Dropping it releases its capacity and serves the queue
    /// ([`ResourceOrderedQueue::serve`]), outside the ledger's lock.
    type Grant;
    /// One resource a request may draw on.
    type Resource: Ord;
    /// A set of resources.
    type Resources: IntoIterator<Item = Self::Resource>;

    /// The stream `request` belongs to, whose requests are granted in arrival order; a stream is
    /// a resource of its own, disjoint from every footprint.
    fn stream(&self, request: &Self::Request) -> Self::Resource;

    /// The resources `request` draws on, were it attempted now.
    fn footprint(&self, request: &Self::Request) -> Self::Resources;

    /// Admit `request`, or name the exhausted resources of its footprint that refused it.
    fn try_admit(&self, request: &Self::Request) -> Result<Self::Grant, Self::Resources>;
}

/// One waiting request and, once a pass admits it, its grant.
pub(crate) struct Waiting<L: AdmissionLedger> {
    /// The request.
    pub(crate) request: L::Request,
    /// The grant a pass admitted, until its waiter claims it.
    pub(crate) grant: Option<L::Grant>,
}

/// Serve `waiting`, in arrival order, to the fixpoint of [`pass`]; returns how many requests it
/// granted.
///
/// One pass is not idempotent: a grant late in a pass may exhaust a resource that an earlier
/// request of the pass needs as well, so that request is now refused by that resource rather
/// than by the one it named, and a request its first refusal skipped may be admissible. The
/// service therefore repeats the pass until one grants nothing:
///
/// ```text
///   serve = passᵏ   where k = min { k | passᵏ⁺¹ grants nothing } ≤ n
/// ```
///
/// Every pass but the last grants at least one of the `n` waiters, so it ends within `n + 1`
/// passes; a pass that leaves no waiter ungranted is the last, since the next would visit none.
/// The decision is a function of the order and of the ledger's answers alone; the only effects
/// are the ledger's admissions.
pub(crate) fn serve<L, W>(
    ledger: &L,
    waiting: &mut [W],
    view: impl Fn(&mut W) -> &mut Waiting<L>,
) -> usize
where
    L: AdmissionLedger,
{
    let mut granted = 0;
    loop {
        let round = pass(ledger, waiting.iter_mut().map(&view));
        granted += round;
        let waiting_left = waiting
            .iter_mut()
            .any(|waiter| view(waiter).grant.is_none());
        if round == 0 || !waiting_left {
            return granted;
        }
    }
}

/// One service pass over `waiting`, in arrival order (the module law); returns how many
/// requests it granted.
fn pass<'a, L>(ledger: &L, waiting: impl IntoIterator<Item = &'a mut Waiting<L>>) -> usize
where L: AdmissionLedger + 'a {
    let mut exhausted = BTreeSet::new();
    let mut granted = 0;
    for waiter in waiting {
        if waiter.grant.is_some() {
            continue;
        }
        let stream = ledger.stream(&waiter.request);
        let blocked = exhausted.contains(&stream)
            || ledger
                .footprint(&waiter.request)
                .into_iter()
                .any(|resource| exhausted.contains(&resource));
        if blocked {
            exhausted.insert(stream);
            continue;
        }
        match ledger.try_admit(&waiter.request) {
            Ok(grant) => {
                waiter.grant = Some(grant);
                granted += 1;
            }
            Err(refused) => {
                exhausted.extend(refused);
                exhausted.insert(stream);
            }
        }
    }
    granted
}

/// Where one request of a [`ResourceOrderedQueue`] stands, in the slot its waiter owns.
enum SlotState<G> {
    /// Queued: the waker to wake once a pass grants it.
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

/// A waiting request of a [`ResourceOrderedQueue`].
struct Entry<L: AdmissionLedger> {
    /// The request; its grant, once a pass admits it, moves to the slot.
    waiting: Waiting<L>,
    /// The slot the request's waiter owns.
    slot: Arc<GrantSlot<L::Grant>>,
}

/// The waiting requests, in arrival order.
struct QueueState<L: AdmissionLedger> {
    /// The ungranted requests, oldest first.
    entries: VecDeque<Entry<L>>,
}

impl<L: AdmissionLedger> QueueState<L> {
    /// Serve the queue to its fixpoint, move every grant into its slot, and collect the wakers
    /// of the waiters it granted. Post: every queued entry is ungranted.
    fn serve(&mut self, ledger: &L) -> Vec<Waker> {
        serve(ledger, self.entries.make_contiguous(), |entry| {
            &mut entry.waiting
        });
        let mut wakers = Vec::new();
        self.entries
            .retain_mut(|entry| match entry.waiting.grant.take() {
                Some(grant) => {
                    wakers.extend(entry.slot.grant(grant));
                    false
                }
                None => true,
            });
        wakers
    }

    /// Withdraw the request of `slot` if it is still queued: whether it was. A request that
    /// left the queue was granted, and its grant is in its slot.
    fn withdraw_queued(&mut self, slot: &Arc<GrantSlot<L::Grant>>) -> bool {
        let position = self
            .entries
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.slot, slot));
        position
            .and_then(|position| self.entries.remove(position))
            .is_some()
    }
}

/// Requests waiting for a ledger's capacity, admitted by resource order (module docs).
///
/// Lock order: the queue's lock is taken before the ledger's and before a slot's, so a ledger
/// must never call back into its queue while it holds its own lock; a release calls
/// [`Self::serve`] only after the ledger's lock is dropped.
pub(crate) struct ResourceOrderedQueue<L: AdmissionLedger> {
    /// The waiting requests.
    state: Mutex<QueueState<L>>,
}

impl<L: AdmissionLedger> ResourceOrderedQueue<L> {
    /// An empty queue.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                entries: VecDeque::new(),
            }),
        }
    }

    /// Lock the waiting requests.
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState<L>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serve the queue, after a release of `ledger` capacity, and wake the waiters it granted.
    pub(crate) fn serve(&self, ledger: &L) {
        let wakers = self.lock().serve(ledger);
        wakers.into_iter().for_each(Waker::wake);
    }

    /// The number of requests waiting ungranted.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn waiting_for_test(&self) -> usize {
        self.lock().entries.len()
    }

    /// Admit `request`, waiting in resource order while it is refused or blocked; `on_wait`
    /// runs once, if the request has to wait.
    ///
    /// Cancel safe: dropping the future withdraws the request, releases a grant it was given
    /// but had not claimed, and runs a pass, since the withdrawn refusal may have blocked
    /// later requests.
    pub(crate) async fn acquire(
        &self,
        ledger: &L,
        request: L::Request,
        on_wait: impl FnOnce(),
    ) -> L::Grant {
        let slot = Arc::new(GrantSlot(Mutex::new(SlotState::Queued(None))));
        let wakers = {
            let mut state = self.lock();
            state.entries.push_back(Entry {
                waiting: Waiting {
                    request,
                    grant: None,
                },
                slot: Arc::clone(&slot),
            });
            state.serve(ledger)
        };
        wakers.into_iter().for_each(Waker::wake);
        if let Some(grant) = slot.claim(None) {
            return grant;
        }
        on_wait();
        let waiter = Waiter {
            queue: self,
            ledger,
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
    /// The slot a pass moves the request's grant into.
    slot: Arc<GrantSlot<L::Grant>>,
}

impl<L: AdmissionLedger> Waiter<'_, L> {
    /// Claim the grant once a pass has moved it into the slot; otherwise stay registered to be
    /// woken. Polled again after it was ready, it stays pending.
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
        // serves the queue itself, while a withdrawn refusal releases nothing and is served here.
        let withdrawn = self.queue.lock().withdraw_queued(&self.slot);
        if withdrawn {
            self.queue.serve(self.ledger);
        } else if let Some(grant) = self.slot.claim(None) {
            drop(grant);
        }
    }
}
