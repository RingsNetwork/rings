//! Constant-rate emission of onion cells on each link, with real cells substituted for cover
//! (#880, #834 D6 and L9).
//!
//! Every received cell costs its receiver `u(b) = b / 16 KiB` units of the per-sender budget
//! `B` per window `V`, cover included (L9 charges before `α` is decoded). A sender therefore
//! shapes each link to the budget, not above it: while a link is active it emits one cell per
//! slot, the next queued real cell if there is one and a uniform cover cell of the link's class
//! otherwise, and a slot for `u` units lasts
//!
//! ```text
//! slot(u) = u / r,     r = ρ·B / V,     ρ = 9/10        (r ≈ 98.3 units/s, 10.17 ms at 16 KiB)
//! ```
//!
//! and while it is idle it emits uniform cover at the floor `r_idle ≤ r` (#880 option C):
//!
//! ```text
//! link facts:  Opened(l) ─▶ lane(l), emitter(l)      Closed(l) ─▶ lane dropped, emitter stops
//! enqueue(l, c) ─▶ queue(l) ← c;  last_real(l) ← now;  wake emitter(l)
//! emitter(l):  loop { plan(queue, last_real, last_start, free(l) ≥ 1, now)   (pure: [`plan`])
//!                       Emit          ⇒ send the next real cell, else uniform cover
//!                       WaitCredit    ⇒ wait for credit on l's lane, or until woken
//!                       WaitUntil(t)  ⇒ sleep until t, or until woken }
//! active ⇔ queued ∨ now < last_real + D,   D = Q = 30 s;   rate r while active, r_idle idle
//! ```
//!
//! `free(l)` is the send credit of the transport lane onion cells travel on, the Application lane
//! of `l`'s connection (#913), read through a read-only view (`Swarm::application_credit`): the
//! emitter never holds a transport handle and never reserves, commits or returns a credit, so
//! the transport's own credit algebra, applied on arrival in its read loop, is all there is to
//! flow control. A slot is emitted, real or cover, only while `free(l) ≥ 1`.
//!
//! Laws (the pure schedule is tested in `tests` below):
//!
//! - **Budget safety.** An emission starts at least `slot(u)` after the start of the previous one
//!   of `u` units, whatever the phase, so every interval of length `V` carries at most
//!   `r·V + u_max = ρ·B + u_max` units, which is `< B` (`u_max = 768` at 12 MiB,
//!   `(1 − ρ)·B ≈ 1638`). An honest sender is therefore never refused by its receiver's L9
//!   admission budget, assuming the receiver's arrival spacing follows the sender's start
//!   spacing: the margin after `u_max`, `(1 − ρ)·B − 768 ≈ 870` units at 12 MiB and `≈ 1637`
//!   units (`≈ 16.6 s` of `r`) at 16 KiB, absorbs transport jitter and window misalignment
//!   beyond that. Credit only delays an emission, so it keeps this bound.
//! - **Rate.** Let `f(b)` be the transport frames of one class-`b` cell and `ρ_rel` the rate at
//!   which `l`'s receiver releases Application-lane frames. A slot starts only while
//!   `free(l) ≥ 1`, and the emitter awaits each cell's send before it plans the next, so the
//!   link's cell rate is `≤ min(r / u(b), ρ_rel / f(b))`: a receiver slower than `r` slows the
//!   link. The transport refuses no honest frame (#913), so no cell, real or cover, is lost to
//!   the receiver's window. The gate is per frame ([`is_credited`]): a cell's first frame starts
//!   only into a window with room, and its other `f(b) − 1` frames wait for credit inside its
//!   send. A cell-sized gate `free(l) ≥ f(b)` is not a law one can keep: `f(b)` reaches 192
//!   frames at 12 MiB, beyond the lane's whole window `W = 16`, so it would never hold. Hence a
//!   real cell enqueued while a cell is in flight waits at most for that one cell, its own
//!   frames' credit included, whether it is real or cover; never for a cover cell committed to a
//!   window with no room for its first frame.
//! - **Isolation.** Onion cells travel on the Application lane alone, whose credit is its own:
//!   a stalled link holds back neither the node's DHT control lane nor any other link (#913).
//! - **Floor.** While a link is up and its lane has credit, its rate is at least `r_idle`: an
//!   idle link emits exactly the floor. A link with no credit emits nothing until credit
//!   returns.
//! - **Dwell.** A real cell at `t` keeps the link at its active rate on `[t, t + D)`; the link
//!   returns to the floor only after `D` without a real cell, so every active period lasts at
//!   least `D`. A credit stall neither shortens nor extends the dwell, which is measured from
//!   the enqueue.
//! - **Volume hiding.** Emission depends only on (queue non-empty, dwell, `free(l) ≥ 1`): while
//!   a link is active its cell rate is `min(r / u(b), ρ_rel / f(b))` whatever the real volume,
//!   since real cells replace cover, never add to it, and credit slows both alike. What is
//!   visible is the active/idle phase at the resolution `D` and the receiver's release rate,
//!   which reveals its load, never the sender's volume (#834 leakage table). A slot whose real
//!   cell is refused before acceptance and requeued carries nothing, neither the cell nor
//!   cover; a refusal is a link-level event, visible as such.
//! - **Order.** Real cells of one link leave in FIFO order, one per slot, so a queued cell waits
//!   at most its queue position times the slot, plus its credit waits. A real cell whose send
//!   was refused before acceptance (`Error::is_send_refused_before_acceptance`: no frame of it
//!   left, so sending it again cannot duplicate it) goes back to the head of its lane while its
//!   link is up: a refusal delays a real cell, it never loses one. A cancelled send
//!   is such a refusal, never a success (#965).
//! - **Bound.** The queues hold at most `MAX_PENDING_ONION_SENDS` cells and
//!   `MAX_PENDING_ONION_SEND_BYTES` bytes, with a per-peer share of each.
//! - **Fairness.** While an endpoint waits for a lane, relayed cells may hold at most half of
//!   that lane's places and bytes ([`RELAY_SHARE_CELLS`], [`RELAY_SHARE_BYTES`]): under
//!   contention each role keeps half of the lane, so neither starves the other.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::oneshot;
use futures::FutureExt;
use rand::RngCore;
use rings_core::dht::Did;
use rings_core::swarm::LaneCreditView;
use rings_runtime::sleep;
use rings_runtime::Spawner;
use web_time::Instant;

use super::OnionLink;
use crate::error::Error;
use crate::error::OnionQueueKind;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionRouteError;
use crate::sync_lock::lock;

mod lanes;
mod schedule;
#[cfg(test)]
mod tests;

use lanes::EmitterStep;
use lanes::OrderedSendState;
use lanes::QueueRole;
use lanes::Refusal;
use schedule::is_credited;
use schedule::Emission;
pub use schedule::OnionIdleFloor;

/// Most cells queued for all links together.
const MAX_PENDING_ONION_SENDS: usize = 1_024;
/// Most cells queued for one link.
const MAX_PENDING_ONION_SENDS_PER_PEER: usize = 128;
/// Most bytes queued for all links together.
const MAX_PENDING_ONION_SEND_BYTES: usize = 64 * 1024 * 1024;
/// Most bytes queued for one link.
const MAX_PENDING_ONION_SEND_BYTES_PER_PEER: usize = 16 * 1024 * 1024;

/// The places of one lane relayed cells may hold while an endpoint waits for it: half.
const RELAY_SHARE_CELLS: usize = MAX_PENDING_ONION_SENDS_PER_PEER / 2;

/// The bytes of one lane relayed cells may hold while an endpoint waits for it: half.
const RELAY_SHARE_BYTES: usize = MAX_PENDING_ONION_SEND_BYTES_PER_PEER / 2;

/// The longest an endpoint waits in [`OnionLinkSender::send`], for space and for its cell to
/// leave: `Q`, so a session's driver resumes its ticks within one quantum. The bound covers
/// the cells ahead of it too: at the ~290 ms per send of the transport before #887, a cell
/// queued behind a full lane of 128 needs ~37 s and meets this deadline on a healthy lane.
const ONION_LINK_SEND_DEADLINE: Duration = Duration::from_secs(30);

/// One real cell waiting on a link.
struct OverlaySend {
    /// The scope the cell is sent under.
    scope: Scope,
    /// The cell bytes, exactly `b` of them.
    payload: Bytes,
    /// Notified with the send's result, for endpoint callers that wait.
    completion: Option<oneshot::Sender<Result<()>>>,
}

/// Shared endpoint and relay capability for one node's constant-rate onion link traffic.
///
/// Clones share the same lanes and clock. This is the single effect boundary through which
/// cells enter the overlay: the data plane's link facts open and close the lanes, relays and
/// control loops enqueue without waiting, and endpoint adapters wait for queue space and for
/// their own cell to leave.
#[derive(Clone)]
pub(crate) struct OnionLinkSender {
    state: Arc<Mutex<OrderedSendState<OverlaySend>>>,
    /// The idle floor of every link.
    floor: OnionIdleFloor,
    /// The origin of the emission clock.
    origin: Instant,
}

impl Default for OnionLinkSender {
    fn default() -> Self {
        Self::new(OnionIdleFloor::DEFAULT)
    }
}

impl OnionLinkSender {
    /// A sender whose idle links emit at `floor`.
    pub(crate) fn new(floor: OnionIdleFloor) -> Self {
        Self {
            state: Arc::default(),
            floor,
            origin: Instant::now(),
        }
    }

    /// Now on the emission clock, in microseconds.
    fn now_us(&self) -> u128 {
        self.origin.elapsed().as_micros()
    }

    /// The link to `link` is up: it emits at least the floor from now on.
    ///
    /// # Errors
    ///
    /// No runtime to spawn the emitter on, or a poisoned lock.
    pub(crate) fn open(&self, scope: Scope, link: OnionLink) -> Result<()> {
        let spawner = Spawner::current()?;
        let now_us = self.now_us();
        if let Some(epoch) = lock(&self.state)?.open(link.peer, now_us) {
            spawner.spawn(emit(self.clone(), link.peer, epoch, scope));
        }
        Ok(())
    }

    /// The link to `link` is down: its queued cells are dropped and its emitter stops.
    ///
    /// # Errors
    ///
    /// A poisoned lock.
    pub(crate) fn close(&self, link: OnionLink) -> Result<()> {
        let now_us = self.now_us();
        lock(&self.state)?.close(link.peer, now_us);
        Ok(())
    }

    /// The owner is gone: close every lane, which stops every emitter, and open none again,
    /// whatever link facts are still to be applied.
    ///
    /// # Errors
    ///
    /// A poisoned lock.
    pub(crate) fn shutdown(&self) -> Result<()> {
        let now_us = self.now_us();
        lock(&self.state)?.shutdown(now_us);
        Ok(())
    }

    /// Exactly the links `up` are up: close every other lane and open the missing ones.
    ///
    /// # Errors
    ///
    /// As for [`Self::open`].
    pub(crate) fn reconcile(&self, scope: &Scope, up: &[Did]) -> Result<()> {
        let outside = lock(&self.state)?.lanes_outside(up);
        for peer in outside {
            self.close(OnionLink::new(peer))?;
        }
        for peer in up {
            self.open(scope.clone(), OnionLink::new(*peer))?;
        }
        Ok(())
    }

    /// Queue one cell for the up link `link` without waiting, as a relay.
    ///
    /// # Errors
    ///
    /// A payload of no class length, or the queue's refusal: `LinkDown` with no up link, a full
    /// bound, or an endpoint waiting for the lane.
    pub(crate) fn enqueue(&self, scope: Scope, link: OnionLink, payload: Bytes) -> Result<()> {
        let class = class_of(&payload)?;
        let send = OverlaySend {
            scope,
            payload,
            completion: None,
        };
        self.try_enqueue(link, class, send, QueueRole::Relay)?
            .map_err(|refusal| refusal.into_error(link))
    }

    /// Queue one locally originated control cell for the up link `link` without waiting (see
    /// [`QueueRole::Local`]).
    ///
    /// # Errors
    ///
    /// A payload of no class length, or the queue's refusal: `LinkDown` with no up link, or a
    /// full bound.
    pub(crate) fn enqueue_local(
        &self,
        scope: Scope,
        link: OnionLink,
        payload: Bytes,
    ) -> Result<()> {
        let class = class_of(&payload)?;
        let send = OverlaySend {
            scope,
            payload,
            completion: None,
        };
        self.try_enqueue(link, class, send, QueueRole::Local)?
            .map_err(|refusal| refusal.into_error(link))
    }

    /// Queue one cell for the up link `link` as an endpoint, waiting for queue space while a
    /// bound is full, and then wait until its direct-edge send has completed: an endpoint's
    /// backpressure. The whole wait is bounded by [`ONION_LINK_SEND_DEADLINE`].
    ///
    /// # Errors
    ///
    /// A payload of no class length, `LinkDown`, the send's own error, the lane's close before
    /// the send, or the deadline.
    pub(crate) async fn send(&self, scope: Scope, link: OnionLink, payload: Bytes) -> Result<()> {
        let deadline = sleep(ONION_LINK_SEND_DEADLINE).fuse();
        futures::pin_mut!(deadline);
        let timed_out = || Error::OnionRouteError(OnionRouteError::LinkSendTimedOut);
        let class = class_of(&payload)?;
        let (completion, completed) = oneshot::channel();
        let mut send = OverlaySend {
            scope,
            payload,
            completion: Some(completion),
        };
        let mut role = QueueRole::Endpoint { holding: false };
        loop {
            let space = match self.try_enqueue(link, class, send, role)? {
                Ok(()) => break,
                Err(Refusal {
                    item,
                    space: Some(space),
                    ..
                }) => {
                    send = item;
                    space
                }
                Err(refusal) => return Err(refusal.into_error(link)),
            };
            // Registered under the refusal's lock; this future's drop, at the deadline or
            // anywhere else, leaves the wait, and a retry hands it back to `enqueue` instead.
            let registration = OnionLaneWait {
                state: Arc::clone(&self.state),
                peer: link.peer,
                armed: true,
            };
            let space = space.fuse();
            futures::pin_mut!(space);
            futures::select! {
                _ = space => {},
                _ = deadline => return Err(timed_out()),
            }
            registration.hand_back();
            role = QueueRole::Endpoint { holding: true };
        }
        let completed = completed.fuse();
        futures::pin_mut!(completed);
        futures::select! {
            result = completed => result
                .map_err(|_| Error::OnionRouteError(OnionRouteError::LinkSendCancelled))?,
            _ = deadline => Err(timed_out()),
        }
    }

    /// Queue `send`, of class `class`, on `link`'s lane as `role`; a refused cell comes back in
    /// the refusal.
    ///
    /// # Errors
    ///
    /// A poisoned lock (the outer error).
    fn try_enqueue(
        &self,
        link: OnionLink,
        class: OnionLoopClass,
        send: OverlaySend,
        role: QueueRole,
    ) -> Result<std::result::Result<(), Refusal<OverlaySend>>> {
        let bytes = send.payload.len();
        let now_us = self.now_us();
        Ok(lock(&self.state)?.enqueue(link.peer, send, bytes, class, now_us, role))
    }
}

/// An endpoint's registration in a lane's `waiting` count, for as long as it waits: dropped
/// armed, it leaves the wait, so an endpoint whose `send` is abandoned never keeps relays
/// yielding the lane.
struct OnionLaneWait {
    /// The queue state the registration is in.
    state: Arc<Mutex<OrderedSendState<OverlaySend>>>,
    /// The lane waited for.
    peer: Did,
    /// Whether the drop still has to leave the wait.
    armed: bool,
}

impl OnionLaneWait {
    /// The endpoint retries: the retry's `enqueue` leaves the wait under its own lock.
    fn hand_back(mut self) {
        self.armed = false;
    }
}

impl Drop for OnionLaneWait {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut state) = lock(&self.state) {
                state.leave(self.peer);
            }
        }
    }
}

/// The class of a cell payload.
///
/// # Errors
///
/// [`OnionRouteError::InvalidCell`] for a length of no class.
fn class_of(payload: &Bytes) -> Result<OnionLoopClass> {
    OnionLoopClass::from_cell_bytes(payload.len())
        .ok_or(Error::OnionRouteError(OnionRouteError::InvalidCell))
}

impl<T> Refusal<T> {
    /// The refusal as the node's error for `link`.
    fn into_error(self, link: OnionLink) -> Error {
        OnionQueueKind::CircuitData.admission(link.peer, self.reason)
    }
}

/// The emitter of epoch `epoch` of one up link: the effectful shell of [`plan`] (see the module
/// diagram). It returns when the lane is closed or belongs to a newer epoch.
async fn emit(sender: OnionLinkSender, peer: Did, epoch: u64, scope: Scope) {
    loop {
        // The view is read before the lane's lock is taken; a grant after the read is not lost,
        // since the wait below resolves at once when credit is already free. A link with no
        // connection has no credit: its emitter waits until woken (its lane closes).
        let credit = scope.application_credit(peer);
        let now_us = sender.now_us();
        let credited = is_credited(credit.as_ref().and_then(LaneCreditView::free));
        let Ok(step) = lock(&sender.state)
            .map(|mut state| state.next(peer, epoch, sender.floor, credited, now_us))
        else {
            tracing::debug!(%peer, "onion link emitter lost its lane state");
            return;
        };
        match step {
            EmitterStep::Stop => return,
            EmitterStep::WaitCredit(woken) => {
                let woken = woken.fuse();
                futures::pin_mut!(woken);
                match credit {
                    // A gone connection ends the wait too; its emitter then waits until woken.
                    Some(credit) => {
                        let credited = credit.credited().fuse();
                        futures::pin_mut!(credited);
                        futures::select! {
                            gone = credited => if !gone {
                                let _ = woken.await;
                            },
                            _ = woken => {},
                        }
                    }
                    None => {
                        let _ = woken.await;
                    }
                }
            }
            EmitterStep::Wait(due_us, woken) => {
                let delay = Duration::from_micros(
                    u64::try_from(due_us.saturating_sub(now_us)).unwrap_or(u64::MAX),
                );
                let timer = sleep(delay).fuse();
                futures::pin_mut!(timer);
                let woken = woken.fuse();
                futures::pin_mut!(woken);
                let slept = futures::select! {
                    slept = timer => slept,
                    _ = woken => Ok(()),
                };
                if let Err(error) = slept {
                    let cancelled =
                        lock(&sender.state).map(|mut state| state.close(peer, now_us).len());
                    tracing::debug!(%peer, ?error, ?cancelled, "onion link emitter has no timer");
                    return;
                }
            }
            EmitterStep::Emit(Emission::Real(send)) => {
                let OverlaySend {
                    scope,
                    payload,
                    completion,
                } = send;
                // `peer` is the loop's exact next hop: overlay routing could pick another path
                // and break both adjacency and the link's emission shape.
                let result = scope.send_direct(peer, payload.clone()).await;
                // A cell that never left goes back to the head of its lane while its link is up;
                // one whose link is down is lost with the link, whose retirement its sessions
                // observe (`circuit::shell`).
                if result
                    .as_ref()
                    .is_err_and(Error::is_send_refused_before_acceptance)
                    && scope.peer_admitted(peer)
                {
                    let unsent = OverlaySend {
                        scope,
                        payload,
                        completion,
                    };
                    if let Ok(mut state) = lock(&sender.state) {
                        state.requeue(peer, epoch, unsent);
                    }
                    continue;
                }
                match completion {
                    Some(completion) => {
                        let _ = completion.send(result);
                    }
                    None => {
                        if let Err(error) = result {
                            tracing::debug!(%peer, ?error, "onion direct-edge send failed");
                        }
                    }
                }
                if let Ok(mut state) = lock(&sender.state) {
                    state.complete(peer, epoch);
                }
            }
            EmitterStep::Emit(Emission::Cover(class)) => {
                let mut cover = vec![0; class.cell_bytes()];
                rand::thread_rng().fill_bytes(&mut cover);
                if let Err(error) = scope.send_direct(peer, Bytes::from(cover)).await {
                    tracing::debug!(%peer, ?error, "onion link cover send failed");
                }
            }
        }
    }
}
