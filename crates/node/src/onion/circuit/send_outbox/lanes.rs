//! The pure queue algebra of every link: lanes, their queues, and the emitter's next step.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::VecDeque;

use futures::channel::oneshot;
use rings_core::dht::Did;

use super::schedule::plan;
use super::schedule::units_of;
use super::schedule::Emission;
use super::schedule::LinkClock;
use super::schedule::OnionIdleFloor;
use super::schedule::Plan;
use super::schedule::WINDOW_US;
use super::MAX_PENDING_ONION_SENDS;
use super::MAX_PENDING_ONION_SENDS_PER_PEER;
use super::MAX_PENDING_ONION_SEND_BYTES;
use super::MAX_PENDING_ONION_SEND_BYTES_PER_PEER;
use super::RELAY_SHARE_BYTES;
use super::RELAY_SHARE_CELLS;
use crate::error::OnionQueueAdmissionReason;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::peer_quota::PeerQuota;

/// One queued real cell with its byte size and class: the emission of a real cell is charged
/// its own units.
pub(super) struct Queued<T> {
    /// The cell.
    pub(super) item: T,
    /// Its bytes.
    pub(super) bytes: usize,
    /// Its class.
    pub(super) class: OnionLoopClass,
    /// Whether a relay queued it: it counts against the relay share.
    pub(super) relayed: bool,
}

/// The cells and bytes of one lane that relays hold, queued or in flight.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RelayShare {
    /// Cells.
    pub(super) cells: usize,
    /// Bytes.
    pub(super) bytes: usize,
}

impl RelayShare {
    /// Whether one more relayed cell of `bytes` would exceed half of the lane.
    pub(super) const fn would_exceed_half(self, bytes: usize) -> bool {
        self.cells >= RELAY_SHARE_CELLS || self.bytes.saturating_add(bytes) > RELAY_SHARE_BYTES
    }

    /// The share with one more cell of `bytes`.
    pub(super) const fn with(self, bytes: usize) -> Self {
        Self {
            cells: self.cells.saturating_add(1),
            bytes: self.bytes.saturating_add(bytes),
        }
    }

    /// The share without one cell of `bytes`.
    pub(super) const fn without(self, bytes: usize) -> Self {
        Self {
            cells: self.cells.saturating_sub(1),
            bytes: self.bytes.saturating_sub(bytes),
        }
    }
}

/// The cell in flight on a lane: its bytes, its class, and whether a relay queued it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InFlight {
    /// Its bytes.
    pub(super) bytes: usize,
    /// Its class.
    pub(super) class: OnionLoopClass,
    /// Whether a relay queued it.
    pub(super) relayed: bool,
}

/// The queue and emission state of one up link.
pub(super) struct PeerLane<T> {
    /// The epoch of the lane's one emitter: an emitter of another epoch stops.
    pub(super) epoch: u64,
    /// Cells waiting.
    pub(super) queued: VecDeque<Queued<T>>,
    /// Bytes of `queued` plus the cell in flight.
    pub(super) pending_bytes: usize,
    /// The cell being sent, if any.
    pub(super) in_flight: Option<InFlight>,
    /// What relays hold of the lane.
    pub(super) relayed: RelayShare,
    /// The class of the last real cell: cover takes it.
    pub(super) class: OnionLoopClass,
    /// The link's emission clock.
    pub(super) clock: LinkClock,
    /// The emitter's wake-up while it waits: an enqueue or the link's close fires it.
    pub(super) wake: Option<oneshot::Sender<()>>,
}

impl<T> PeerLane<T> {
    /// A lane of emitter epoch `epoch` with nothing queued and nothing emitted.
    pub(super) fn new(epoch: u64) -> Self {
        Self {
            epoch,
            queued: VecDeque::new(),
            pending_bytes: 0,
            in_flight: None,
            relayed: RelayShare::default(),
            class: OnionLoopClass::DEFAULT,
            clock: LinkClock::default(),
            wake: None,
        }
    }

    /// Wake the emitter if it waits.
    pub(super) fn wake(&mut self) {
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(());
        }
    }
}

/// What the emitter of a link does next; the output of [`OrderedSendState::next`].
#[derive(Debug)]
pub(super) enum EmitterStep<T> {
    /// Send this, now.
    Emit(Emission<T>),
    /// Sleep until this instant, or until the wake-up fires.
    Wait(u128, oneshot::Receiver<()>),
    /// Wait for the link's credit, or until the wake-up fires.
    WaitCredit(oneshot::Receiver<()>),
    /// The link is down, or the lane belongs to a newer emitter: the emitter returns.
    Stop,
}

/// Who queues a cell: a relay forwards without waiting, an endpoint waits for queue space and,
/// while it waits, keeps relays to half of the lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueueRole {
    /// A relayed cell; refused beyond half of the lane while an endpoint waits for it.
    Relay,
    /// A locally originated control cell (a client's credit, keep-alive or `abort`, an exit's
    /// answer to an unheld session): queued without waiting, like a relay's, but never counted
    /// in the relay share, so a node's own control loops do not yield to relayed traffic.
    Local,
    /// An endpoint's cell; `holding` once it retries after a wake-up, when it hands back the
    /// place it held.
    Endpoint {
        /// Whether this attempt follows a refusal that registered it.
        holding: bool,
    },
}

/// A cell the state refused, handed back with the reason and, for an endpoint refused by a full
/// bound, its registered wake-up, taken under the same lock as the refusal.
pub(super) struct Refusal<T> {
    /// Why.
    pub(super) reason: OnionQueueAdmissionReason,
    /// The cell.
    pub(super) item: T,
    /// The wake-up of an endpoint registered to wait for space.
    pub(super) space: Option<oneshot::Receiver<()>>,
}

/// The pure queue algebra of every link; see the module laws.
///
/// Invariants:
/// - `quota.total()` and `pending_bytes` equal the queued cells plus the in-flight cell of every
///   lane, and a lane has at most one cell in flight, the witness of per-link order;
/// - a lane exists exactly while its link is up: [`Self::open`] is its only constructor and
///   [`Self::close`] its only destructor, so a cell for a peer with no up link is refused, and
///   after [`Self::shutdown`] no lane opens again;
/// - every lane has exactly one emitter, the one of its epoch: an emitter of a closed lane's
///   epoch meets `Stop` even after the lane is reopened;
/// - `waiting[p]` counts the endpoints that wait for, or retry into, `p`'s lane; while it is
///   positive a relayed cell for `p` is refused once relays hold half of the lane, so under
///   contention each role keeps half (Fairness), and each freed place wakes one waiter;
/// - a lane's `relayed` share equals the relayed cells and bytes it holds, queued or in flight;
/// - a reopened lane keeps its emission clock for `V`, so closing and reopening a link never
///   lets an emission start early (Budget safety holds across generations).
pub(super) struct OrderedSendState<T> {
    pub(super) quota: PeerQuota,
    pub(super) pending_bytes: usize,
    pub(super) lanes: HashMap<Did, PeerLane<T>>,
    /// The epoch of the next lane opened.
    pub(super) next_epoch: u64,
    /// The wake-ups of the endpoints waiting for space, per lane, in arrival order.
    pub(super) space: HashMap<Did, VecDeque<oneshot::Sender<()>>>,
    /// The endpoints waiting for, or retrying into, each lane.
    pub(super) waiting: HashMap<Did, usize>,
    /// The clocks of closed lanes, kept for a reopen within `V`.
    pub(super) retired: HashMap<Did, LinkClock>,
    /// Whether the sender is shut down: no lane opens again.
    pub(super) shut: bool,
}

impl<T> Default for OrderedSendState<T> {
    fn default() -> Self {
        Self {
            quota: PeerQuota::new(MAX_PENDING_ONION_SENDS, MAX_PENDING_ONION_SENDS_PER_PEER),
            pending_bytes: 0,
            lanes: HashMap::new(),
            next_epoch: 0,
            space: HashMap::new(),
            waiting: HashMap::new(),
            retired: HashMap::new(),
            shut: false,
        }
    }
}

impl<T> OrderedSendState<T> {
    /// The link to `peer` is up at `now`: give it a lane, with the clock it had if it closed
    /// less than `V` ago, and return the epoch of the emitter to spawn if it had none. Nothing
    /// opens after a shutdown.
    pub(super) fn open(&mut self, peer: Did, now_us: u128) -> Option<u64> {
        if self.shut {
            return None;
        }
        self.prune_retired(now_us);
        let clock = self.retired.remove(&peer).unwrap_or_default();
        match self.lanes.entry(peer) {
            Entry::Occupied(_) => None,
            Entry::Vacant(lane) => {
                let epoch = self.next_epoch;
                self.next_epoch = self.next_epoch.wrapping_add(1);
                let mut fresh = PeerLane::new(epoch);
                fresh.clock = clock;
                lane.insert(fresh);
                Some(epoch)
            }
        }
    }

    /// Queue one cell of `class` for `peer` at `now` as `role`; the cell makes the link active.
    ///
    /// # Errors
    ///
    /// `LinkDown` if `peer` has no up link, the bound the cell would exceed, or, for a relay,
    /// `PeerFull` beyond half of the lane while an endpoint waits for it; the cell is handed
    /// back, with an endpoint's wake-up when a bound refused it.
    pub(super) fn enqueue(
        &mut self,
        peer: Did,
        item: T,
        item_bytes: usize,
        class: OnionLoopClass,
        now_us: u128,
        role: QueueRole,
    ) -> std::result::Result<(), Refusal<T>> {
        let refuse = |reason, item| Refusal {
            reason,
            item,
            space: None,
        };
        match role {
            QueueRole::Relay if self.relay_yields(peer, item_bytes) => {
                return Err(refuse(OnionQueueAdmissionReason::PeerFull, item));
            }
            QueueRole::Endpoint { holding: true } => self.leave(peer),
            QueueRole::Relay | QueueRole::Local | QueueRole::Endpoint { holding: false } => {}
        }
        if let Err(reason) = self.admit(peer, item_bytes) {
            let waits = matches!(role, QueueRole::Endpoint { .. }) && reason.is_full();
            let space = waits.then(|| self.register(peer));
            return Err(Refusal {
                reason,
                item,
                space,
            });
        }
        let Some(lane) = self.lanes.get_mut(&peer) else {
            return Err(refuse(OnionQueueAdmissionReason::LinkDown, item));
        };
        let relayed = role == QueueRole::Relay;
        if relayed {
            lane.relayed = lane.relayed.with(item_bytes);
        }
        lane.queued.push_back(Queued {
            item,
            bytes: item_bytes,
            class,
            relayed,
        });
        lane.class = class;
        lane.clock.last_real_us = Some(now_us);
        lane.wake();
        Ok(())
    }

    /// Whether a relayed cell of `item_bytes` yields `peer`'s lane: an endpoint waits for it and
    /// relays already hold half of it (Fairness).
    pub(super) fn relay_yields(&self, peer: Did, item_bytes: usize) -> bool {
        self.waiting.get(&peer).is_some_and(|count| *count > 0)
            && self
                .lanes
                .get(&peer)
                .is_some_and(|lane| lane.relayed.would_exceed_half(item_bytes))
    }

    /// Reserve the quota and bytes of one cell of `item_bytes` for `peer`'s up lane.
    pub(super) fn admit(
        &mut self,
        peer: Did,
        item_bytes: usize,
    ) -> std::result::Result<(), OnionQueueAdmissionReason> {
        let peer_pending_bytes = self
            .lanes
            .get(&peer)
            .ok_or(OnionQueueAdmissionReason::LinkDown)?
            .pending_bytes;
        self.quota.can_reserve(peer)?;
        let next_peer_bytes = peer_pending_bytes
            .checked_add(item_bytes)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        let next_pending_bytes = self
            .pending_bytes
            .checked_add(item_bytes)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        if next_pending_bytes > MAX_PENDING_ONION_SEND_BYTES {
            return Err(OnionQueueAdmissionReason::GlobalFull);
        }
        if next_peer_bytes > MAX_PENDING_ONION_SEND_BYTES_PER_PEER {
            return Err(OnionQueueAdmissionReason::PeerFull);
        }
        let Some(lane) = self.lanes.get_mut(&peer) else {
            return Err(OnionQueueAdmissionReason::LinkDown);
        };
        self.quota.reserve(peer)?;
        self.pending_bytes = next_pending_bytes;
        lane.pending_bytes = next_peer_bytes;
        Ok(())
    }

    /// Register an endpoint waiting for `peer`'s lane: its wake-up fires when a place frees.
    pub(super) fn register(&mut self, peer: Did) -> oneshot::Receiver<()> {
        let (waiter, woken) = oneshot::channel();
        self.space.entry(peer).or_default().push_back(waiter);
        *self.waiting.entry(peer).or_default() += 1;
        woken
    }

    /// An endpoint no longer waits for `peer`'s lane (it retries, or it gave up).
    pub(super) fn leave(&mut self, peer: Did) {
        if let Entry::Occupied(mut count) = self.waiting.entry(peer) {
            *count.get_mut() = count.get().saturating_sub(1);
            if *count.get() == 0 {
                count.remove();
            }
        }
    }

    /// Wake one waiter for a place freed on `peer`'s lane, else one waiting for another lane (a
    /// freed place also frees the global bound).
    pub(super) fn wake_one(&mut self, peer: Did) {
        let mut candidates = std::iter::once(peer)
            .chain(self.space.keys().copied().filter(|other| *other != peer))
            .collect::<Vec<_>>();
        candidates.dedup();
        for candidate in candidates {
            let Some(queue) = self.space.get_mut(&candidate) else {
                continue;
            };
            while let Some(waiter) = queue.pop_front() {
                if waiter.send(()).is_ok() {
                    return;
                }
                // A waiter that is gone abandoned its place.
                if let Entry::Occupied(mut count) = self.waiting.entry(candidate) {
                    *count.get_mut() = count.get().saturating_sub(1);
                }
            }
            self.space.remove(&candidate);
        }
    }

    /// The next step at `now` under `floor` of the emitter of epoch `epoch`, with the link
    /// `credited` or not: [`plan`] over the lane, taking the next queued cell when it emits and
    /// charging the emission the units of what it sends. A wait arms the lane's wake-up under
    /// the same lock, so no enqueue is missed. While a real cell is in flight nothing is taken
    /// from the queue, so real cells leave in order.
    pub(super) fn next(
        &mut self,
        peer: Did,
        epoch: u64,
        floor: OnionIdleFloor,
        credited: bool,
        now_us: u128,
    ) -> EmitterStep<T> {
        let Some(lane) = self.lanes.get_mut(&peer).filter(|lane| lane.epoch == epoch) else {
            return EmitterStep::Stop;
        };
        let queued = !lane.queued.is_empty() || lane.in_flight.is_some();
        match plan(lane.clock, queued, credited, floor, now_us) {
            Plan::WaitUntil(due_us) => {
                let (wake, woken) = oneshot::channel();
                lane.wake = Some(wake);
                EmitterStep::Wait(due_us, woken)
            }
            Plan::WaitCredit => {
                let (wake, woken) = oneshot::channel();
                lane.wake = Some(wake);
                EmitterStep::WaitCredit(woken)
            }
            Plan::Emit => {
                let (emission, class) = match lane.in_flight {
                    None => lane.queued.pop_front().map(|queued| {
                        lane.in_flight = Some(InFlight {
                            bytes: queued.bytes,
                            class: queued.class,
                            relayed: queued.relayed,
                        });
                        (Emission::Real(queued.item), queued.class)
                    }),
                    Some(_) => None,
                }
                .unwrap_or((Emission::Cover(lane.class), lane.class));
                lane.clock.last_start = Some((now_us, units_of(class)));
                EmitterStep::Emit(emission)
            }
        }
    }

    /// The cell in flight on `peer`'s lane of epoch `epoch` never left (its send was refused
    /// before acceptance, #895 R4-M1): put it back at the head of the queue, keeping its quota,
    /// bytes and relay share, so it leaves next, in order, and no real cell is lost to a
    /// refusal. Returns whether it was requeued; a closed lane drops it.
    pub(super) fn requeue(&mut self, peer: Did, epoch: u64, item: T) -> bool {
        let Some(lane) = self.lanes.get_mut(&peer).filter(|lane| lane.epoch == epoch) else {
            return false;
        };
        let Some(InFlight {
            bytes,
            class,
            relayed,
        }) = lane.in_flight.take()
        else {
            return false;
        };
        lane.queued.push_front(Queued {
            item,
            bytes,
            class,
            relayed,
        });
        true
    }

    /// Retire the cell in flight on `peer`'s lane of epoch `epoch`, releasing its quota and
    /// bytes, and wake one waiter for the freed place.
    pub(super) fn complete(&mut self, peer: Did, epoch: u64) {
        let Some(lane) = self.lanes.get_mut(&peer).filter(|lane| lane.epoch == epoch) else {
            return;
        };
        let Some(InFlight { bytes, relayed, .. }) = lane.in_flight.take() else {
            return;
        };
        if relayed {
            lane.relayed = lane.relayed.without(bytes);
        }
        if self.quota.release(peer) {
            lane.pending_bytes = lane.pending_bytes.saturating_sub(bytes);
            self.pending_bytes = self.pending_bytes.saturating_sub(bytes);
        }
        self.wake_one(peer);
    }

    /// Forget the clocks of lanes closed so long ago that a reopen could not emit early: their
    /// last emission started `V` or more before `now`.
    pub(super) fn prune_retired(&mut self, now_us: u128) {
        self.retired.retain(|_, clock| {
            clock
                .last_start
                .is_some_and(|(start_us, _)| start_us + WINDOW_US > now_us)
        });
    }

    /// The link to `peer` is down at `now`, or its emitter cannot run: drop its lane and every
    /// queued cell, releasing their quota and bytes, keep its clock for a reopen (pruning the
    /// lapsed ones), wake its emitter so it stops, and wake every endpoint waiting for it (it
    /// retries into `LinkDown`).
    pub(super) fn close(&mut self, peer: Did, now_us: u128) -> Vec<T> {
        let Some(mut lane) = self.lanes.remove(&peer) else {
            return Vec::new();
        };
        lane.wake();
        self.prune_retired(now_us);
        self.retired.insert(peer, lane.clock);
        self.quota.release_peer(peer);
        self.pending_bytes = self.pending_bytes.saturating_sub(lane.pending_bytes);
        if let Some(waiters) = self.space.remove(&peer) {
            waiters.into_iter().for_each(|waiter| {
                let _ = waiter.send(());
            });
        }
        self.wake_one(peer);
        lane.queued.into_iter().map(|queued| queued.item).collect()
    }

    /// Close every lane at `now`, and open none again: the sender's owner is gone.
    pub(super) fn shutdown(&mut self, now_us: u128) {
        self.shut = true;
        for peer in self.lanes_outside(&[]) {
            self.close(peer, now_us);
        }
    }

    /// The links whose lanes are not in `up`.
    pub(super) fn lanes_outside(&self, up: &[Did]) -> Vec<Did> {
        self.lanes
            .keys()
            .filter(|peer| !up.contains(peer))
            .copied()
            .collect()
    }
}
