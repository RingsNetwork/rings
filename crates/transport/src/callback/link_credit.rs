//! The effectful shell of per-lane credit flow control for one connection.
//!
//! [`crate::core::credit`] is the pure algebra; this module holds one connection's lanes under
//! one lock and interprets the algebra's outputs as effects:
//!
//! ```text
//!   send side    reserve(lane) ─▶ CreditReservation ─bind(send)─▶ settled on drop:
//!                committed iff the send crossed its irrevocable boundary, else returned
//!                apply(Credit) ─▶ wake the senders waiting on that lane
//!                apply(Shrink) ─▶ outbox[lane] ← Returned(B, g)
//!   receive side admit(lane)   ─▶ CreditPermit ─drop─▶ release ─▶ outbox[lane] ⊔= Credit(A, g)
//!                admit(data lane) ─▶ demand on the node's pool ─▶ lend ─▶ advertise
//!                idle_check ─▶ outbox[lane] ← Shrink(L′, g)        apply(Returned) ─▶ advertise
//!                next_signal(lane) ◀── the lane's pump sends its outbox, one signal a frame
//!   node         every change of a data lane's charge is reported to the NodeFramePool
//! ```
//!
//! The outbox holds, per lane, the signals not yet sent: the greatest credit of the current
//! generation, a join of `(ℕ, max)`, so credits released faster than the pump sends them
//! coalesce into one frame; and the latest shrink and return. Each lane has its own pump (see
//! `credit_pump`), so a signal held up behind one lane's channel delays no other lane. A sender
//! that finds no credit waits, and that wait is logged at `warn`, once per wait, as
//! backpressure; a frame beyond the advertised credit is a protocol violation of the peer.
//!
//! Lock law: every transition takes the lock once, never across a suspension point, and wakes
//! the tasks it unblocked after committing its state. The node's pool is locked on its own,
//! never together with a link's lock, and never across a suspension point either.

#[cfg(rings_transport_backend)]
use std::future::poll_fn;
#[cfg(rings_transport_backend)]
use std::future::Future;
#[cfg(not(rings_transport_backend))]
use std::marker::PhantomData;
use std::mem;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
#[cfg(rings_transport_backend)]
use std::sync::Weak;
#[cfg(rings_transport_backend)]
use std::task::Context;
#[cfg(rings_transport_backend)]
use std::task::Poll;
use std::task::Waker;

use crate::core::credit::credit_index;
use crate::core::credit::CreditIndex;
use crate::core::credit::CreditViolation;
use crate::core::credit::CreditWindow;
use crate::core::credit::LaneSignal;
use crate::core::credit::PerLane;
use crate::core::credit::ReceiveWindow;
use crate::core::credit::SendCredit;
use crate::core::pool::ChannelLane;
#[cfg(test)]
use crate::core::pool::DATA_CHANNEL_POOL_SIZE;
#[cfg(rings_transport_backend)]
use crate::core::transport::SendAcceptance;
#[cfg(rings_transport_backend)]
use crate::error::Error;
#[cfg(rings_transport_backend)]
use crate::error::Result;
use crate::sync_utils::lock_recover;

mod pool;
mod view;

/// `T_idle`, the period of a link's idle check: a data lane that holds a share of the node's
/// pool and admitted no frame for it is shrunk to its floor (load-bounds paper, Law 3). On a
/// link whose round trip exceeds it, a lane paused for one round trip is shrunk, and its next
/// burst waits one round trip for pooled credit again; no frame is refused.
#[cfg(any(feature = "dummy", rings_transport_webrtc))]
pub(crate) const LANE_IDLE_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

pub use pool::NodeFramePool;
pub use view::LaneCreditView;

/// The signals of one lane not yet sent by its pump.
#[derive(Clone, Copy, Debug, Default)]
struct SignalOutbox {
    /// The greatest credit of the newest generation queued, `(limit, generation)`.
    credit: Option<(u64, u64)>,
    /// The latest shrink, `(limit, generation)`.
    shrink: Option<(u64, u64)>,
    /// The latest return, `(bound, generation)`.
    returned: Option<(u64, u64)>,
}

impl SignalOutbox {
    /// Queue `signal`: a credit joins the queued one of its generation and replaces one of an
    /// older generation, and a shrink or return replaces an older one. Idempotent, commutative
    /// and associative per kind, so signals queued faster than the pump sends them coalesce.
    fn queue(&mut self, signal: LaneSignal) {
        let newer = |queued: Option<(u64, u64)>, (value, generation): (u64, u64)| match queued {
            Some((queued, at)) if at > generation || (at == generation && queued >= value) => {
                Some((queued, at))
            }
            _ => Some((value, generation)),
        };
        match signal {
            LaneSignal::Credit { limit, generation } => {
                self.credit = newer(self.credit, (limit, generation));
            }
            LaneSignal::Shrink { limit, generation } => {
                self.shrink = newer(self.shrink, (limit, generation));
                // A credit of an older generation is ignored by its sender: drop it.
                self.credit = self.credit.filter(|(_, at)| *at >= generation);
            }
            LaneSignal::Returned { bound, generation } => {
                self.returned = newer(self.returned, (bound, generation));
            }
        }
    }

    #[cfg(rings_transport_backend)]
    /// Take one signal to send: a return first, then a shrink, then a credit.
    fn take(&mut self) -> Option<LaneSignal> {
        if let Some((bound, generation)) = self.returned.take() {
            return Some(LaneSignal::Returned { bound, generation });
        }
        if let Some((limit, generation)) = self.shrink.take() {
            return Some(LaneSignal::Shrink { limit, generation });
        }
        self.credit
            .take()
            .map(|(limit, generation)| LaneSignal::Credit { limit, generation })
    }
}

/// One sender waiting for a lane's credit: its waker, under the identity of its wait, so the
/// wait removes exactly its own entry when it ends.
struct SendWaiter {
    /// The wait's identity.
    #[cfg(rings_transport_backend)]
    id: u64,
    /// The task to wake.
    waker: Waker,
}

/// One connection's lanes, behind [`LinkCredit`]'s lock.
struct LinkCreditState {
    /// The sending end of each lane.
    send: PerLane<SendCredit>,
    /// Senders waiting for credit, per lane.
    send_waiters: PerLane<Vec<SendWaiter>>,
    /// The identity of the next credit wait.
    #[cfg(rings_transport_backend)]
    next_waiter: u64,
    /// The receiving end of each lane.
    receive: PerLane<ReceiveWindow>,
    /// The signals not yet sent, per lane.
    outbox: PerLane<SignalOutbox>,
    /// What the node's pool counts for each lane: its charge as last reported.
    charged: PerLane<u64>,
    /// Each lane's credit pump, while it waits for the lane's outbox.
    pump: PerLane<Option<Waker>>,
    /// Whether the connection is gone: waiting senders fail and the pump stops.
    closed: bool,
}

#[cfg(rings_transport_backend)]
impl LinkCreditState {
    /// Register `waker` for the wait `id` on credit index `index`, giving the wait its identity
    /// on its first registration; a wait that registers again only refreshes its waker.
    ///
    /// Law (no lost wakeup). The caller found no credit under the same lock, so the transition
    /// that next frees a credit or ends the link finds this waker and wakes it.
    fn register_waiter(&mut self, index: CreditIndex, id: &mut Option<u64>, waker: &Waker) {
        let id = *id.get_or_insert_with(|| {
            let id = self.next_waiter;
            self.next_waiter = id.wrapping_add(1);
            id
        });
        let waiters = &mut self.send_waiters[index];
        match waiters.iter_mut().find(|waiter| waiter.id == id) {
            Some(waiter) => waiter.waker.clone_from(waker),
            None => waiters.push(SendWaiter {
                id,
                waker: waker.clone(),
            }),
        }
    }
}

/// Per-lane credit flow control of one connection; see the module documentation.
pub(crate) struct LinkCredit {
    /// The peer, for logs.
    pub(super) peer: Arc<str>,
    /// The node's frame pool, which every change of a data lane's charge is reported to.
    pool: NodeFramePool,
    /// All lanes, under one lock.
    state: Mutex<LinkCreditState>,
}

impl LinkCredit {
    /// The credit of a fresh connection generation with `peer`: every lane granted its floor,
    /// the priority lane its whole window ([`CreditWindow::of`]), its data lanes lent beyond
    /// that from the node's `pool`.
    pub(crate) fn new(peer: Arc<str>, pool: NodeFramePool) -> Self {
        Self {
            peer,
            pool,
            state: Mutex::new(LinkCreditState {
                send: PerLane::from_fn(|index| SendCredit::new(CreditWindow::of(index))),
                send_waiters: PerLane::default(),
                #[cfg(rings_transport_backend)]
                next_waiter: 0,
                receive: PerLane::from_fn(|index| ReceiveWindow::new(CreditWindow::of(index))),
                outbox: PerLane::default(),
                charged: PerLane::default(),
                pump: PerLane::default(),
                closed: false,
            }),
        }
    }

    /// The state, for one transition.
    fn lock(&self) -> MutexGuard<'_, LinkCreditState> {
        lock_recover(&self.state)
    }

    #[cfg(rings_transport_backend)]
    /// Wait for one credit on `lane`.
    ///
    /// Law (backpressure, not a verdict). The wait ends only when the peer raises the lane's
    /// credit or the connection generation ends: a slow receiver is backpressure, and whether
    /// the peer is alive is liveness's to judge, not credit's. A caller whose wait pins a budget
    /// bounds that wait itself, by what the budget is for.
    ///
    /// Post: `Ok` holds a reservation the caller binds to its send (see
    /// [`CreditReservation::bind`]); `Err` once the connection is gone. Dropping the future
    /// before it completes reserves nothing. A wait is logged at `warn` once, when it starts.
    pub(crate) async fn reserve(self: &Arc<Self>, lane: ChannelLane) -> Result<CreditReservation> {
        let mut wait = CreditWait::new(Arc::downgrade(self), credit_index(lane));
        poll_fn(|context| self.poll_reserve(wait.index, &mut wait.id, context)).await
    }

    #[cfg(rings_transport_backend)]
    /// One attempt of [`Self::reserve`]: a reservation now, or the wait `id` registered (and
    /// logged when it starts).
    fn poll_reserve(
        self: &Arc<Self>,
        index: CreditIndex,
        id: &mut Option<u64>,
        context: &mut Context<'_>,
    ) -> Poll<Result<CreditReservation>> {
        let mut state = self.lock();
        if state.closed {
            return Poll::Ready(Err(Error::LinkCreditClosed(self.peer.to_string())));
        }
        let credit = &mut state.send[index];
        if credit.try_reserve() {
            return Poll::Ready(Ok(CreditReservation {
                link: Arc::clone(self),
                index,
                send: None,
            }));
        }
        if id.is_none() {
            let (sent, limit) = credit.usage();
            tracing::warn!(
                peer = %self.peer,
                lane = index.lane().index(),
                sent,
                limit,
                "transport backpressure: the receiver holds a full credit window on this lane; \
                 the send waits for credit"
            );
        }
        state.register_waiter(index, id, context.waker());
        Poll::Pending
    }

    #[cfg(rings_transport_backend)]
    /// Remove the wait `id` on credit index `index`: it ended, granted or abandoned.
    fn forget_waiter(&self, index: CreditIndex, id: u64) {
        self.lock().send_waiters[index].retain(|waiter| waiter.id != id);
    }

    /// Wake every sender waiting on credit index `index`, after the lock is released. A woken
    /// wait registers again if it still finds no credit.
    fn wake_senders(&self, index: CreditIndex) {
        let waiters = mem::take(&mut self.lock().send_waiters[index]);
        waiters.into_iter().for_each(|waiter| waiter.waker.wake());
    }

    /// Apply a signal the peer sent on `lane`: a credit raises the lane's send limit and wakes
    /// its senders; a shrink sets it to the bound it returns, queued for the pump; a return ends
    /// the lane's pending shrink, which may advertise again.
    pub(crate) fn apply(&self, lane: ChannelLane, signal: LaneSignal) {
        let index = credit_index(lane);
        match signal {
            LaneSignal::Credit { limit, generation } => {
                self.lock().send[index].grant(limit, generation);
                self.wake_senders(index);
            }
            LaneSignal::Shrink { limit, generation } => {
                let pump = {
                    let mut state = self.lock();
                    let Some(bound) = state.send[index].shrink(limit, generation) else {
                        return;
                    };
                    state.outbox[index].queue(LaneSignal::Returned { bound, generation });
                    state.pump[index].take()
                };
                pump.into_iter().for_each(Waker::wake);
                self.wake_senders(index);
            }
            LaneSignal::Returned { bound, generation } => {
                if self.lock().receive[index].returned(bound, generation) {
                    self.settle(index);
                    self.advertise_due([index]);
                }
            }
        }
    }

    /// Admit one arriving frame of `lane`: a data lane's admission is also its demand on the
    /// node's pool (Law 3).
    ///
    /// Post: `Ok` holds the frame's place in the window until the permit drops; `Err` is a
    /// frame beyond the advertised credit, which the window does not count.
    pub(crate) fn admit(
        self: &Arc<Self>,
        lane: ChannelLane,
    ) -> std::result::Result<CreditPermit, CreditViolation> {
        let index = credit_index(lane);
        let wants = {
            let mut state = self.lock();
            let receive = &mut state.receive[index];
            receive.admit()?;
            receive.want(CreditWindow::of(index)) > 0
        };
        if wants {
            self.pool.demand(self, index);
        }
        Ok(CreditPermit {
            link: Arc::clone(self),
            index,
        })
    }

    /// The share of the pool the lane of `index` may still borrow.
    pub(super) fn want(&self, index: CreditIndex) -> u64 {
        let state = self.lock();
        match state.closed {
            true => 0,
            false => state.receive[index].want(CreditWindow::of(index)),
        }
    }

    /// Take `share` of the pool for the lane of `index`, which the pool counted already: the
    /// lane keeps what it wants of it, gives the rest back, and advertises the credit it grants.
    pub(super) fn lend(&self, index: CreditIndex, share: u64) {
        {
            let mut state = self.lock();
            if !state.closed {
                state.receive[index].lend(CreditWindow::of(index), share);
            }
            state.charged[index] = state.charged[index].saturating_add(share);
        }
        self.settle(index);
        self.advertise_due([index]);
    }

    /// Report the charge of the lane of `index` to the node's pool, if it changed.
    fn settle(&self, index: CreditIndex) {
        let (before, after) = {
            let mut state = self.lock();
            let after = state.receive[index].charge(CreditWindow::of(index));
            (mem::replace(&mut state.charged[index], after), after)
        };
        self.pool.adjust(pool::edge_key(self), before, after);
    }

    /// Shrink this edge's pooled shares to `share` in all, the largest first: a claimant below
    /// its share found the node's pool full (Law 3, rebalance). Each lane shrunk sends its
    /// `Shrink`; the shares come back with the returns.
    pub(super) fn shrink_to_share(&self, share: u64) {
        let pumps = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            let state = &mut *state;
            let mut lanes = CreditIndex::ALL
                .into_iter()
                .map(|index| (state.receive[index].lent(), index))
                .collect::<Vec<_>>();
            lanes.sort_unstable_by_key(|(lent, _)| std::cmp::Reverse(*lent));
            let mut excess = lanes
                .iter()
                .map(|(lent, _)| lent)
                .sum::<u64>()
                .saturating_sub(share);
            let mut pumps = Vec::new();
            for (lent, index) in lanes {
                if excess == 0 {
                    break;
                }
                let cut = lent.min(excess);
                let window = CreditWindow::of(index);
                let Some(shrink) = state.receive[index].shrink_to(window, lent - cut) else {
                    continue;
                };
                excess -= cut;
                state.outbox[index].queue(shrink);
                pumps.extend(state.pump[index].take());
            }
            pumps
        };
        pumps.into_iter().for_each(Waker::wake);
    }

    /// Release one admitted frame of credit index `index`: advertise the credit it completes,
    /// and report the lane's charge, which a lane draining after a shrink gives back.
    fn release(&self, index: CreditIndex) {
        self.lock().receive[index].release();
        self.settle(index);
        self.advertise_due([index]);
    }

    #[cfg(any(feature = "dummy", rings_transport_webrtc))]
    /// The idle check, made every [`LANE_IDLE_PERIOD`] by its driver (`credit_pump` in a WebRTC
    /// backend, the controlled scheduler's virtual clock in the dummy one), or by a test: every
    /// data lane that holds a share of the pool and admitted no
    /// frame since the last check is shrunk to its floor (Law 3). Returns `false` once the
    /// connection is gone, which ends the driver.
    pub(crate) fn idle_check(&self) -> bool {
        let pumps = {
            let mut state = self.lock();
            if state.closed {
                return false;
            }
            let state = &mut *state;
            let mut pumps = Vec::new();
            for index in CreditIndex::ALL {
                let Some(shrink) = state.receive[index].idle_shrink(CreditWindow::of(index)) else {
                    continue;
                };
                state.outbox[index].queue(shrink);
                pumps.extend(state.pump[index].take());
            }
            pumps
        };
        pumps.into_iter().for_each(Waker::wake);
        CreditIndex::ALL
            .into_iter()
            .for_each(|index| self.settle(index));
        true
    }

    /// Advertise the credit of every lane of `indices` that its releases have completed,
    /// queuing it for the lane's pump. A closed link advertises nothing.
    fn advertise_due(&self, indices: impl IntoIterator<Item = CreditIndex>) {
        let pumps = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            let state = &mut *state;
            let mut pumps = Vec::new();
            for index in indices {
                let Some((limit, generation)) =
                    state.receive[index].advertise(CreditWindow::of(index))
                else {
                    continue;
                };
                state.outbox[index].queue(LaneSignal::Credit { limit, generation });
                pumps.extend(state.pump[index].take());
            }
            pumps
        };
        pumps.into_iter().for_each(Waker::wake);
    }

    #[cfg(rings_transport_backend)]
    /// Wait for a signal to send on the lane of `index`, and take it.
    ///
    /// Post: `Some` with the next signal of the lane's outbox; `None` once the connection is
    /// gone and nothing is left to send on the lane.
    pub(crate) fn next_signal(
        &self,
        index: CreditIndex,
    ) -> impl Future<Output = Option<LaneSignal>> + '_ {
        poll_fn(move |context| {
            let mut state = self.lock();
            if let Some(signal) = state.outbox[index].take() {
                return Poll::Ready(Some(signal));
            }
            if state.closed {
                return Poll::Ready(None);
            }
            state.pump[index] = Some(context.waker().clone());
            Poll::Pending
        })
    }

    #[cfg(rings_transport_webrtc)]
    /// Queue `signal` on the lane of `index` again after its send failed, idempotent as every
    /// signal is. Post: `false`, queuing nothing, once the connection is gone, when no send can
    /// succeed.
    pub(crate) fn requeue(&self, index: CreditIndex, signal: LaneSignal) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        state.outbox[index].queue(signal);
        true
    }

    /// Mark the connection gone: waiting senders fail, and the pump stops once the outbox is
    /// empty. No frame arrives any more, so each lane counts only the frames it still holds,
    /// and gives the rest of its charge back to the node's pool; the frames held count until
    /// they are released.
    pub(crate) fn close(self: &Arc<Self>) {
        tracing::debug!(peer = %self.peer, "link credit closed: waiting senders fail");
        let (waiters, pump) = {
            let mut state = self.lock();
            state.closed = true;
            // No frame arrives any more: every lane counts only the frames it holds.
            state
                .receive
                .iter_mut()
                .for_each(|(_, receive)| receive.close());
            let waiters = state
                .send_waiters
                .iter_mut()
                .flat_map(|(_, waiters)| mem::take(waiters))
                .collect::<Vec<_>>();
            let pumps = state
                .pump
                .iter_mut()
                .filter_map(|(_, pump)| pump.take())
                .collect::<Vec<_>>();
            (waiters, pumps)
        };
        waiters.into_iter().for_each(|waiter| waiter.waker.wake());
        pump.into_iter().for_each(Waker::wake);
        self.pool.forget(self);
        CreditIndex::ALL
            .into_iter()
            .for_each(|index| self.settle(index));
    }

    /// The waits registered on `lane`'s credit.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn send_waiters_for_test(&self, lane: ChannelLane) -> usize {
        self.lock().send_waiters[credit_index(lane)].len()
    }

    /// Frames of each lane admitted and not yet released.
    #[cfg(test)]
    pub(crate) fn occupancy(&self) -> [u64; DATA_CHANNEL_POOL_SIZE as usize] {
        let mut receive = self.lock().receive;
        let mut occupancy = [0; DATA_CHANNEL_POOL_SIZE as usize];
        for ((_, window), slot) in receive.iter_mut().zip(occupancy.iter_mut()) {
            *slot = window.occupancy();
        }
        occupancy
    }
}

#[cfg(rings_transport_backend)]
/// One pending credit wait, of [`LinkCredit::reserve`] or [`LaneCreditView::credited`]: its
/// registration is removed when it ends, granted or abandoned, so an abandoned wait leaves no
/// waker behind. It holds the link weakly: a link already gone holds no waker of it.
struct CreditWait {
    /// The link waited on.
    link: Weak<LinkCredit>,
    /// The credit index of the lane.
    index: CreditIndex,
    /// The wait's identity, once it has registered.
    id: Option<u64>,
}

#[cfg(rings_transport_backend)]
impl CreditWait {
    /// A wait on credit index `index` of `link`, not yet registered.
    fn new(link: Weak<LinkCredit>, index: CreditIndex) -> Self {
        Self {
            link,
            index,
            id: None,
        }
    }
}

#[cfg(rings_transport_backend)]
impl Drop for CreditWait {
    fn drop(&mut self) {
        if let (Some(id), Some(link)) = (self.id, self.link.upgrade()) {
            link.forget_waiter(self.index, id);
        }
    }
}

/// One lane credit reserved ahead of a send, for a caller that bounds the send itself in time.
///
/// Waiting for credit is backpressure, which may last as long as the receiver holds its window
/// full; accepting a frame into the channel is not. A caller that times the latter reserves the
/// credit first, untimed, and hands it to the send through [`SendPermit::with_credit`], so only
/// the send is timed. Dropping the reservation unused returns the credit.
///
/// [`SendPermit::with_credit`]: crate::core::transport::SendPermit::with_credit
pub struct LaneCreditReservation(
    #[cfg(rings_transport_backend)] pub(crate) CreditReservation,
    #[cfg(not(rings_transport_backend))] PhantomData<()>,
);

#[cfg(rings_transport_backend)]
/// One credit held for a send, settled when the reservation is dropped.
///
/// Law (exact accounting). The credit is spent iff the bound send crossed its irrevocable
/// boundary, the one point after which its frame reaches the wire or its connection
/// generation is retired; otherwise it is returned. The settlement atomically cancels a send
/// that has not crossed that boundary, so no detached part of the send can cross it after the
/// credit was returned. Every frame on the wire is thus counted once, and no credit is spent
/// on a frame that never left, which would shrink the lane's window for good.
pub(crate) struct CreditReservation {
    /// The connection the credit belongs to.
    link: Arc<LinkCredit>,
    /// The credit index of the lane.
    index: CreditIndex,
    /// The send the credit was reserved for, once bound.
    send: Option<SendAcceptance>,
}

#[cfg(rings_transport_backend)]
impl CreditReservation {
    /// Whether the credit is of `link`'s connection: a credit settles only its own window.
    pub(crate) fn is_of(&self, link: &Arc<LinkCredit>) -> bool {
        Arc::ptr_eq(&self.link, link)
    }

    /// Bind the credit to the send whose admission `send` observes. The caller holds the
    /// reservation until that send is decided.
    pub(crate) fn bind(mut self, send: SendAcceptance) -> Self {
        self.send = Some(send);
        self
    }
}

#[cfg(rings_transport_backend)]
impl Drop for CreditReservation {
    fn drop(&mut self) {
        let irrevocable = self
            .send
            .as_ref()
            .is_some_and(|send| !send.try_cancel() && send.is_irrevocable());
        {
            let mut state = self.link.lock();
            let send = &mut state.send[self.index];
            if irrevocable {
                send.commit();
            } else {
                send.cancel();
            }
        }
        if !irrevocable {
            self.link.wake_senders(self.index);
        }
    }
}

/// One admitted frame's place in its lane's window: dropping it releases the place, which may
/// advertise more credit to the peer.
pub(crate) struct CreditPermit {
    /// The connection the window belongs to.
    link: Arc<LinkCredit>,
    /// The credit index of the lane.
    index: CreditIndex,
}

impl CreditPermit {
    /// Whether the permit holds a place in `link`'s windows: a permit is released only by the
    /// connection that admitted it.
    pub(crate) fn is_of(&self, link: &Arc<LinkCredit>) -> bool {
        Arc::ptr_eq(&self.link, link)
    }

    /// The lane whose window the permit holds a place in, named by its channel's least lane.
    pub(crate) const fn lane(&self) -> ChannelLane {
        self.index.lane()
    }
}

impl Drop for CreditPermit {
    fn drop(&mut self) {
        self.link.release(self.index);
    }
}
