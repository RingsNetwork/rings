//! The effectful shell of per-lane credit flow control for one connection.
//!
//! [`crate::core::credit`] is the pure algebra; this module holds one connection's lanes under
//! one lock and interprets the algebra's outputs as effects:
//!
//! ```text
//!   send side    reserve(lane) ─▶ CreditReservation ─bind(send)─▶ settled on drop:
//!                committed iff the send crossed its irrevocable boundary, else returned
//!                grant(credit)  ─▶ wake the senders waiting on that lane
//!   receive side admit(lane)   ─▶ CreditPermit ─drop─▶ release ─▶ outbox[lane] ⊔= limit
//!                next_credit(lane) ◀── the lane's credit pump sends its outbox as a Credit frame
//!   node         admit/release count the bytes in NodeReceiveLoad; above its soft limit a
//!                release defers the lane's advertisement until the load falls below it
//! ```
//!
//! The outbox holds, per lane, the greatest credit not yet sent, so it is itself a join of
//! `(ℕ, max)`: credits released faster than the pump sends them coalesce into one frame. Each
//! lane has its own pump, so a credit held up behind one lane's channel delays no other lane. A
//! sender that finds no credit waits, and that wait is logged at `warn`, once per wait, as
//! backpressure; a frame beyond the advertised credit is a protocol violation of the peer.
//!
//! Lock law: every transition takes the lock once, never across a suspension point, and wakes
//! the tasks it unblocked after committing its state. The node's load is locked on its own, never
//! together with a link's lock, and never across a suspension point either.

#[cfg(rings_transport_backend)]
use std::future::poll_fn;
#[cfg(rings_transport_backend)]
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
#[cfg(rings_transport_backend)]
use std::task::Poll;
use std::task::Waker;

use crate::core::credit::credit_index;
use crate::core::credit::CreditIndex;
use crate::core::credit::CreditViolation;
use crate::core::credit::CreditWindow;
use crate::core::credit::PerLane;
use crate::core::credit::ReceiveWindow;
use crate::core::credit::SendCredit;
use crate::core::pool::ChannelLane;
#[cfg(rings_transport_backend)]
use crate::core::transport::SendAcceptance;
#[cfg(rings_transport_backend)]
use crate::error::Error;
#[cfg(rings_transport_backend)]
use crate::error::Result;
use crate::sync_utils::lock_recover;

/// The bytes of received frames a node holds above which its lanes defer their credit
/// advertisements: 16 MiB, the node-wide frame budget the transport held before per-lane credit.
pub const NODE_RECEIVE_SOFT_LIMIT_BYTES: u64 = 16 * 1024 * 1024;

/// The bytes of frames a node's connections have admitted and not yet released, and the links
/// whose credit advertisements wait for that load to fall below
/// [`NODE_RECEIVE_SOFT_LIMIT_BYTES`]: one per transport, shared by its connections.
///
/// Law (soft bound). Above the limit no lane but [`ChannelLane::PRIORITY`] advertises new
/// credit, so the frames a node holds exceed the limit by at most the credit already advertised
/// and the priority lane's window per connection; the hard bound remains the windows themselves.
/// Deferring narrows what senders may send next, never what they were granted, so no honest
/// frame is refused; and the priority lane never waits for the node's load, so a peer's control
/// traffic (and with it the liveness of every link) never depends on how much other peers make
/// this node hold. Law (progress): a deferred link is
/// woken by the release that brings the load below the limit, and the decision to defer is
/// taken under the same lock as its registration, so no such release is missed.
///
/// Clone law: clones name the same node.
#[derive(Clone)]
pub struct NodeReceiveLoad(Arc<Mutex<NodeLoadState>>);

/// What [`NodeReceiveLoad`] keeps under its lock.
struct NodeLoadState {
    /// Bytes admitted and not yet released, across the node's connections.
    held: u64,
    /// The bytes above which advertisements are deferred.
    limit: u64,
    /// The links that deferred an advertisement since the load last fell below the limit.
    deferred: Vec<Weak<LinkCredit>>,
}

/// What a release may do about its lane's advertisement.
enum LoadVerdict {
    /// The node is below its limit: advertise, and wake the links that deferred theirs.
    Open(Vec<Weak<LinkCredit>>),
    /// The node is at or above its limit: the link is registered to be woken.
    Deferred,
}

impl NodeReceiveLoad {
    /// A node holding nothing, with the production limit.
    pub fn new() -> Self {
        Self::with_limit(NODE_RECEIVE_SOFT_LIMIT_BYTES)
    }

    /// A node holding nothing whose advertisements are deferred above `limit` bytes.
    pub(crate) fn with_limit(limit: u64) -> Self {
        Self(Arc::new(Mutex::new(NodeLoadState {
            held: 0,
            limit,
            deferred: Vec::new(),
        })))
    }

    /// The state, for one transition.
    fn lock(&self) -> MutexGuard<'_, NodeLoadState> {
        lock_recover(&self.0)
    }

    /// Count `bytes` admitted.
    fn admit(&self, bytes: u64) {
        let mut state = self.lock();
        state.held = state.held.saturating_add(bytes);
    }

    /// Count `bytes` released by `link`, and judge whether its lane may advertise now; a link
    /// that may not is registered to be woken.
    fn release(&self, bytes: u64, link: &Arc<LinkCredit>) -> LoadVerdict {
        let mut state = self.lock();
        state.held = state.held.saturating_sub(bytes);
        if state.held < state.limit {
            return LoadVerdict::Open(std::mem::take(&mut state.deferred));
        }
        if !state
            .deferred
            .iter()
            .any(|deferred| std::ptr::eq(deferred.as_ptr(), Arc::as_ptr(link)))
        {
            state.deferred.push(Arc::downgrade(link));
        }
        LoadVerdict::Deferred
    }

    /// The bytes held, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn held(&self) -> u64 {
        self.lock().held
    }
}

impl Default for NodeReceiveLoad {
    fn default() -> Self {
        Self::new()
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
    /// The greatest credit not yet sent, per lane.
    outbox: PerLane<Option<u64>>,
    /// Each lane's credit pump, while it waits for the lane's outbox.
    pump: PerLane<Option<Waker>>,
    /// Whether the connection is gone: waiting senders fail and the pump stops.
    closed: bool,
}

/// Per-lane credit flow control of one connection; see the module documentation.
pub(crate) struct LinkCredit {
    /// The peer, for logs.
    peer: Arc<str>,
    /// The window every lane uses.
    window: CreditWindow,
    /// The node's receive load, which every admission and release counts.
    load: NodeReceiveLoad,
    /// All lanes, under one lock.
    state: Mutex<LinkCreditState>,
}

impl LinkCredit {
    /// The credit of a fresh connection generation with `peer`: every lane granted the window,
    /// its receive load counted in the node's `load`.
    pub(crate) fn new(peer: Arc<str>, window: CreditWindow, load: NodeReceiveLoad) -> Self {
        Self {
            peer,
            window,
            load,
            state: Mutex::new(LinkCreditState {
                send: PerLane::from_fn(|_| SendCredit::new(window)),
                send_waiters: PerLane::default(),
                #[cfg(rings_transport_backend)]
                next_waiter: 0,
                receive: PerLane::from_fn(|_| ReceiveWindow::new(window)),
                outbox: PerLane::default(),
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
        let mut wait = CreditWait {
            link: self,
            index: credit_index(lane),
            id: None,
        };
        poll_fn(|context| wait.poll(context)).await
    }

    #[cfg(rings_transport_backend)]
    /// One attempt of [`Self::reserve`]: a reservation now, or the wait `id` registered (and
    /// logged when it starts).
    fn poll_reserve(
        self: &Arc<Self>,
        index: CreditIndex,
        id: &mut Option<u64>,
        context: &mut std::task::Context<'_>,
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
        let wait = *id.get_or_insert_with(|| {
            let wait = state.next_waiter;
            state.next_waiter = wait.wrapping_add(1);
            wait
        });
        let waiters = &mut state.send_waiters[index];
        match waiters.iter_mut().find(|waiter| waiter.id == wait) {
            Some(waiter) => waiter.waker.clone_from(context.waker()),
            None => waiters.push(SendWaiter {
                id: wait,
                waker: context.waker().clone(),
            }),
        }
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
        let waiters = std::mem::take(&mut self.lock().send_waiters[index]);
        waiters.into_iter().for_each(|waiter| waiter.waker.wake());
    }

    /// Apply a credit the peer advertised for `lane`: `limit ← max(limit, credit)`.
    pub(crate) fn grant(&self, lane: ChannelLane, credit: u64) {
        let index = credit_index(lane);
        self.lock().send[index].grant(credit);
        self.wake_senders(index);
    }

    /// Admit one arriving frame of `lane`.
    ///
    /// Post: `Ok` holds the frame's place in the window until the permit drops; `Err` is a
    /// frame beyond the advertised credit, which the window does not count.
    pub(crate) fn admit(
        self: &Arc<Self>,
        lane: ChannelLane,
        bytes: usize,
    ) -> std::result::Result<CreditPermit, CreditViolation> {
        let index = credit_index(lane);
        self.lock().receive[index].admit()?;
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.load.admit(bytes);
        Ok(CreditPermit {
            link: Arc::clone(self),
            index,
            bytes,
        })
    }

    /// Release one admitted frame of credit index `index`, of `bytes`: advertise the credit it
    /// completes, unless the node is at its soft limit, which defers the advertisement until
    /// the load falls (the priority lane's excepted); a release that brings it below the limit
    /// wakes the deferred links.
    ///
    /// The release is counted before the load is judged, so a link registered as deferred has
    /// counted every release the wake must advertise.
    fn release(self: &Arc<Self>, index: CreditIndex, bytes: u64) {
        self.lock().receive[index].release();
        match self.load.release(bytes, self) {
            LoadVerdict::Open(deferred) => {
                self.advertise_due(CreditIndex::ALL);
                deferred
                    .iter()
                    .filter_map(Weak::upgrade)
                    .for_each(|link| link.advertise_due(CreditIndex::ALL));
            }
            LoadVerdict::Deferred => self.advertise_due([credit_index(ChannelLane::PRIORITY)]),
        }
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
                let Some(limit) = state.receive[index].advertise(self.window) else {
                    continue;
                };
                let pending = &mut state.outbox[index];
                *pending = Some(pending.map_or(limit, |queued| queued.max(limit)));
                pumps.extend(state.pump[index].take());
            }
            pumps
        };
        pumps.into_iter().for_each(Waker::wake);
    }

    #[cfg(rings_transport_backend)]
    /// Wait for a credit to advertise on the lane of `index`, and take it.
    ///
    /// Post: `Some` with the greatest credit not yet sent; `None` once the connection is gone
    /// and nothing is left to send on the lane.
    pub(crate) fn next_credit(&self, index: CreditIndex) -> impl Future<Output = Option<u64>> + '_ {
        poll_fn(move |context| {
            let mut state = self.lock();
            if let Some(credit) = state.outbox[index].take() {
                return Poll::Ready(Some(credit));
            }
            if state.closed {
                return Poll::Ready(None);
            }
            state.pump[index] = Some(context.waker().clone());
            Poll::Pending
        })
    }

    #[cfg(any(
        feature = "native-webrtc",
        all(feature = "web-sys-webrtc", target_family = "wasm")
    ))]
    /// Queue `credit` on the lane of `index` again after its send failed: `outbox ⊔= credit`,
    /// idempotent as every credit is. Post: `false`, queuing nothing, once the connection is
    /// gone, when no send can succeed.
    pub(crate) fn requeue(&self, index: CreditIndex, credit: u64) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        let pending = &mut state.outbox[index];
        *pending = Some(pending.map_or(credit, |queued| queued.max(credit)));
        true
    }

    /// Mark the connection gone: waiting senders fail, and the pump stops once the outbox is
    /// empty. The frames still admitted keep counting in the node's load until they are
    /// released.
    pub(crate) fn close(&self) {
        tracing::debug!(peer = %self.peer, "link credit closed: waiting senders fail");
        let (waiters, pump) = {
            let mut state = self.lock();
            state.closed = true;
            let waiters = state
                .send_waiters
                .iter_mut()
                .flat_map(|(_, waiters)| std::mem::take(waiters))
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
    }

    /// The waits registered on `lane`'s credit.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn send_waiters_for_test(&self, lane: ChannelLane) -> usize {
        self.lock().send_waiters[credit_index(lane)].len()
    }

    /// Frames of each lane admitted and not yet released.
    #[cfg(test)]
    pub(crate) fn occupancy(&self) -> [u64; crate::core::pool::DATA_CHANNEL_POOL_SIZE as usize] {
        let mut receive = self.lock().receive;
        let mut occupancy = [0; crate::core::pool::DATA_CHANNEL_POOL_SIZE as usize];
        for ((_, window), slot) in receive.iter_mut().zip(occupancy.iter_mut()) {
            *slot = window.occupancy();
        }
        occupancy
    }
}

#[cfg(rings_transport_backend)]
/// One pending [`LinkCredit::reserve`]: its registration is removed when it ends, granted or
/// abandoned, so an abandoned wait leaves no waker behind.
struct CreditWait<'a> {
    /// The link waited on.
    link: &'a Arc<LinkCredit>,
    /// The credit index of the lane.
    index: CreditIndex,
    /// The wait's identity, once it has registered.
    id: Option<u64>,
}

#[cfg(rings_transport_backend)]
impl CreditWait<'_> {
    /// One attempt to reserve; see [`LinkCredit::poll_reserve`].
    fn poll(&mut self, context: &mut std::task::Context<'_>) -> Poll<Result<CreditReservation>> {
        self.link.poll_reserve(self.index, &mut self.id, context)
    }
}

#[cfg(rings_transport_backend)]
impl Drop for CreditWait<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.link.forget_waiter(self.index, id);
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
    #[cfg(not(rings_transport_backend))] std::marker::PhantomData<()>,
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

/// One admitted frame's place in its lane's window and in its node's receive load: dropping it
/// releases both, which may advertise more credit to the peer.
pub(crate) struct CreditPermit {
    /// The connection the window belongs to.
    link: Arc<LinkCredit>,
    /// The credit index of the lane.
    index: CreditIndex,
    /// The frame's bytes, as counted in the node's load.
    bytes: u64,
}

impl Drop for CreditPermit {
    fn drop(&mut self) {
        self.link.release(self.index, self.bytes);
    }
}

/// How long a lane's credit pump pauses after a failed send before it sends the credit again:
/// a send fails transiently when one of the connection's outbound channels is closed or not yet
/// open, and the pause keeps a send that fails at once from spinning the pump.
#[cfg(any(
    feature = "native-webrtc",
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
const CREDIT_RESEND_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// Run the credit pump of the lane of `index`: send every credit `link` queues on the lane as a
/// [`TransportMessage::Credit`] frame, until the connection is gone.
///
/// A credit frame is never charged against credit, so the pump never waits on the peer. A
/// credit is cumulative, so losing one can leave the peer's sender waiting for good: a credit
/// whose send fails is queued again and sent after [`CREDIT_RESEND_PAUSE`] (a send fails
/// transiently while any of this end's outbound channels is closed or not yet open; credits
/// leave on the outbound channel, frames arrive on the remote-created one), until the
/// connection is gone, which a send failing with the connection released also says. One pump
/// runs per lane, so a credit held up behind one lane's channel delays no other lane's.
#[cfg(any(
    feature = "native-webrtc",
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
pub(crate) async fn pump_lane_credits<C>(
    link: Arc<LinkCredit>,
    connection: crate::connection_ref::ConnectionRef<C>,
    index: CreditIndex,
) where
    C: crate::core::transport::ConnectionInterface<Error = Error, Sdp = String>
        + rings_runtime::MaybeSendSync,
{
    use crate::core::transport::ConnectionInterface;
    use crate::core::transport::SendPermit;
    use crate::core::transport::TransportMessage;

    while let Some(credit) = link.next_credit(index).await {
        let sent = connection
            .send_message_with_permit(
                TransportMessage::Credit(credit),
                index.lane(),
                SendPermit::always(),
            )
            .await;
        if let Err(error) = sent {
            // The connection is gone for good: no send of it can succeed again.
            if matches!(
                error,
                Error::ConnectionReleased(_) | Error::LinkCreditClosed(_)
            ) {
                return;
            }
            tracing::debug!(peer = %link.peer, %error, "failed to send a credit frame; resending");
            if rings_runtime::sleep(CREDIT_RESEND_PAUSE).await.is_err()
                || !link.requeue(index, credit)
            {
                return;
            }
        }
    }
}
