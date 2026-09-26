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
//! emitter(l):  loop { plan(queue, last_real, last_start, now)           (pure: [`plan`])
//!                       Emit          ⇒ send the next real cell, else uniform cover
//!                       WaitUntil(t)  ⇒ sleep until t, or until woken }
//! active ⇔ queued ∨ now < last_real + D,   D = Q = 30 s;   rate r while active, r_idle idle
//! ```
//!
//! Laws (the pure schedule is tested in `tests` below):
//!
//! - **Budget safety.** An emission starts at least `slot(u)` after the start of the previous one
//!   of `u` units, whatever the phase, so every interval of length `V` carries at most
//!   `r·V + u_max = ρ·B + u_max` units, which is `< B` (`u_max = 768` at 12 MiB,
//!   `(1 − ρ)·B ≈ 1638`). An honest sender is therefore never refused by its receiver's
//!   admission, and no real cell is lost to it.
//! - **Floor.** While a link is up its rate is at least `r_idle`: an idle link emits exactly the
//!   floor.
//! - **Dwell.** A real cell at `t` keeps the link at `r` on `[t, t + D)`; the link returns to the
//!   floor only after `D` without a real cell, so every active period lasts at least `D`.
//! - **Volume hiding.** While a link is active its cell rate is `r / u(b)` whatever the real
//!   volume: real cells replace cover, they are never added to it. What is visible is the
//!   active/idle phase at the resolution `D` (#834 leakage table).
//! - **Order.** Real cells of one link leave in FIFO order, one per slot, so a queued cell waits
//!   at most its queue position times the slot.
//! - **Bound.** The queues hold at most `MAX_PENDING_ONION_SENDS` cells and
//!   `MAX_PENDING_ONION_SEND_BYTES` bytes, with a per-peer share of each.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::oneshot;
use futures::FutureExt;
use rand::RngCore;
use rings_core::dht::Did;
use rings_runtime::sleep;
use rings_runtime::Spawner;
use web_time::Instant;

use super::OnionLink;
use super::ONION_ADMISSION_SENDER_UNITS;
use super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use super::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::error::OnionQueueAdmissionReason;
use crate::error::OnionQueueKind;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::peer_quota::PeerQuota;
use crate::sync_lock::lock;

/// Most cells queued for all links together.
const MAX_PENDING_ONION_SENDS: usize = 1_024;
/// Most cells queued for one link.
const MAX_PENDING_ONION_SENDS_PER_PEER: usize = 128;
/// Most bytes queued for all links together.
const MAX_PENDING_ONION_SEND_BYTES: usize = 64 * 1024 * 1024;
/// Most bytes queued for one link.
const MAX_PENDING_ONION_SEND_BYTES_PER_PEER: usize = 16 * 1024 * 1024;

/// The receiver's per-sender budget `B` in units (#834 L9).
const ONION_LINK_BUDGET_UNITS: u128 = ONION_ADMISSION_SENDER_UNITS as u128;

/// `ρ = 9/10`, as numerator and denominator: the margin below the budget that covers the
/// receiver's window alignment and a clock skew between the two ends.
const EMISSION_RATE_NUMERATOR: u128 = 9;
/// See [`EMISSION_RATE_NUMERATOR`].
const EMISSION_RATE_DENOMINATOR: u128 = 10;

/// `slot(u) = u / r = u·V / (ρ·B)`, in microseconds: the emission clock granularity is one
/// microsecond, and the slot is rounded up, which only lowers the rate.
const fn slot_micros(units: u128) -> u128 {
    (units * ONION_FORWARD_MAX_VALIDITY_MS * 1_000 * EMISSION_RATE_DENOMINATOR)
        .div_ceil(EMISSION_RATE_NUMERATOR * ONION_LINK_BUDGET_UNITS)
}

/// `u(b)` as the schedule's integer type.
fn units_of(class: OnionLoopClass) -> u128 {
    u128::from(class.units())
}

// Budget safety at the largest class: `ρ·B + u_max < B`.
const _: () = assert!(
    ONION_LINK_BUDGET_UNITS * EMISSION_RATE_NUMERATOR / EMISSION_RATE_DENOMINATOR + 768
        < ONION_LINK_BUDGET_UNITS
);

/// `D = Q`, the dwell: an active link stays active for `D` after its last real cell.
const ONION_LINK_DWELL_US: u128 = ONION_FORWARD_EXPIRY_QUANTUM_MS * 1_000;

/// The idle floor `r_idle`: the cover rate of an up link that carries no real cell, as the
/// period of one unit. The default is one unit per second; a node may lower the rate (a longer
/// period, for a browser), never raise it, so `r_idle ≤ 1 unit/s < r` by construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionIdleFloor {
    /// `1 / r_idle` in microseconds per unit.
    micros_per_unit: u128,
}

impl OnionIdleFloor {
    /// `r_idle = 1` unit/s (#880).
    pub const DEFAULT: Self = Self {
        micros_per_unit: 1_000_000,
    };

    /// The floor of one unit per `period`, or `None` if `period` is shorter than one second:
    /// the floor is configurable downwards only.
    pub fn per_unit(period: Duration) -> Option<Self> {
        let micros_per_unit = period.as_micros();
        (micros_per_unit >= Self::DEFAULT.micros_per_unit).then_some(Self { micros_per_unit })
    }

    /// The idle slot after `units` units: `units / r_idle`.
    const fn slot_micros(self, units: u128) -> u128 {
        units * self.micros_per_unit
    }
}

impl Default for OnionIdleFloor {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a link's emitter sends in a slot.
#[derive(Debug, Eq, PartialEq)]
enum Emission<T> {
    /// Send this queued real cell.
    Real(T),
    /// Send a uniform cover cell of this class.
    Cover(OnionLoopClass),
}

/// The emission clock of one link: what the pure schedule [`plan`] reads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LinkClock {
    /// When the last real cell was enqueued, in microseconds on the sender's clock.
    last_real_us: Option<u128>,
    /// When the last emission started, and its units.
    last_start: Option<(u128, u128)>,
}

/// The schedule's answer at one instant.
#[derive(Debug, Eq, PartialEq)]
enum Plan {
    /// Emit now: a real cell if one is queued, else cover.
    Emit,
    /// Emit nothing before this instant (microseconds).
    WaitUntil(u128),
}

/// Whether a link is active at `now`: a real cell is queued, or the last one was enqueued less
/// than `D` ago.
const fn is_active(clock: LinkClock, queued: bool, now_us: u128) -> bool {
    queued
        || match clock.last_real_us {
            Some(last_real_us) => now_us < last_real_us + ONION_LINK_DWELL_US,
            None => false,
        }
}

/// The pure schedule of one up link (#880 option C):
///
/// ```text
/// active(now) = queued ∨ now < last_real + D
/// due(now)    = last_start + (active(now) ? u/r : u/r_idle)      (0 before the first emission)
/// plan(now)   = now ≥ due(now) ? Emit : WaitUntil(due(now))
/// ```
///
/// Laws: every emission starts at least `u/r` after the previous one (`u/r_idle ≥ u/r`), so the
/// budget bound of the module holds across phase changes; while active the rate is `r`, while
/// idle it is `r_idle`, so the rate is `≥ r_idle` while the link is up; and an active period,
/// begun by a real cell at `t`, lasts at least until `t + D`.
fn plan(clock: LinkClock, queued: bool, floor: OnionIdleFloor, now_us: u128) -> Plan {
    let Some((start_us, units)) = clock.last_start else {
        return Plan::Emit;
    };
    let slot_us = if is_active(clock, queued, now_us) {
        slot_micros(units)
    } else {
        floor.slot_micros(units)
    };
    let due_us = start_us + slot_us;
    if now_us >= due_us {
        Plan::Emit
    } else {
        Plan::WaitUntil(due_us)
    }
}

/// One real cell waiting on a link.
struct OverlaySend {
    /// The scope the cell is sent under.
    scope: Scope,
    /// The cell bytes, exactly `b` of them.
    payload: Bytes,
    /// Notified with the send's result, for endpoint callers that wait.
    completion: Option<oneshot::Sender<Result<()>>>,
}

/// The queue and emission state of one up link.
struct PeerLane<T> {
    /// Cells waiting, with their byte sizes.
    queued: VecDeque<(T, usize)>,
    /// Bytes of `queued` plus the cell in flight.
    pending_bytes: usize,
    /// The byte size of the cell being sent, if any.
    in_flight_bytes: Option<usize>,
    /// The class of the last real cell: cover takes it.
    class: OnionLoopClass,
    /// The link's emission clock.
    clock: LinkClock,
    /// The emitter's wake-up while it waits: an enqueue or the link's close fires it.
    wake: Option<oneshot::Sender<()>>,
}

impl<T> PeerLane<T> {
    /// A lane with nothing queued and nothing emitted.
    fn new() -> Self {
        Self {
            queued: VecDeque::new(),
            pending_bytes: 0,
            in_flight_bytes: None,
            class: OnionLoopClass::DEFAULT,
            clock: LinkClock::default(),
            wake: None,
        }
    }

    /// Wake the emitter if it waits.
    fn wake(&mut self) {
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(());
        }
    }
}

/// What the emitter of a link does next; the output of [`OrderedSendState::next`].
#[derive(Debug)]
enum EmitterStep<T> {
    /// Send this, now.
    Emit(Emission<T>),
    /// Sleep until this instant, or until the wake-up fires.
    Wait(u128, oneshot::Receiver<()>),
    /// The link is down: its lane is gone and the emitter returns.
    Stop,
}

/// The pure queue algebra of every link; see the module laws.
///
/// Invariant: `quota.total()` and `pending_bytes` equal the queued cells plus the in-flight cell
/// of every lane, and a lane has at most one cell in flight, the witness of per-link order.
/// A lane exists exactly while its link is up (opened and not closed), or since a cell was
/// queued for it.
struct OrderedSendState<T> {
    quota: PeerQuota,
    pending_bytes: usize,
    lanes: HashMap<Did, PeerLane<T>>,
}

impl<T> Default for OrderedSendState<T> {
    fn default() -> Self {
        Self {
            quota: PeerQuota::new(MAX_PENDING_ONION_SENDS, MAX_PENDING_ONION_SENDS_PER_PEER),
            pending_bytes: 0,
            lanes: HashMap::new(),
        }
    }
}

impl<T> OrderedSendState<T> {
    /// The link to `peer` is up: give it a lane, and return whether it needs an emitter (it had
    /// no lane).
    fn open(&mut self, peer: Did) -> bool {
        match self.lanes.entry(peer) {
            Entry::Occupied(_) => false,
            Entry::Vacant(lane) => {
                lane.insert(PeerLane::new());
                true
            }
        }
    }

    /// Queue one cell of `class` for `peer` at `now` and return whether the link needs a new
    /// emitter (it had no lane). The cell makes the link active.
    fn enqueue(
        &mut self,
        peer: Did,
        item: T,
        item_bytes: usize,
        class: OnionLoopClass,
        now_us: u128,
    ) -> std::result::Result<bool, OnionQueueAdmissionReason> {
        let peer_pending_bytes = self.lanes.get(&peer).map_or(0, |lane| lane.pending_bytes);
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
        self.quota.reserve(peer)?;
        self.pending_bytes = next_pending_bytes;
        let spawn = self.open(peer);
        let Some(lane) = self.lanes.get_mut(&peer) else {
            return Err(OnionQueueAdmissionReason::CounterOverflow);
        };
        lane.queued.push_back((item, item_bytes));
        lane.pending_bytes = next_peer_bytes;
        lane.class = class;
        lane.clock.last_real_us = Some(now_us);
        lane.wake();
        Ok(spawn)
    }

    /// The emitter's next step at `now` under `floor`: [`plan`] over the lane, taking the next
    /// queued cell when it emits. A wait arms the lane's wake-up under the same lock, so no
    /// enqueue is missed. A lane with a cell in flight emits cover until [`Self::complete`].
    fn next(&mut self, peer: Did, floor: OnionIdleFloor, now_us: u128) -> EmitterStep<T> {
        let Some(lane) = self.lanes.get_mut(&peer) else {
            return EmitterStep::Stop;
        };
        let queued = !lane.queued.is_empty() || lane.in_flight_bytes.is_some();
        match plan(lane.clock, queued, floor, now_us) {
            Plan::WaitUntil(due_us) => {
                let (wake, woken) = oneshot::channel();
                lane.wake = Some(wake);
                EmitterStep::Wait(due_us, woken)
            }
            Plan::Emit => {
                let emission = match lane.in_flight_bytes {
                    None => lane.queued.pop_front().map(|(item, item_bytes)| {
                        lane.in_flight_bytes = Some(item_bytes);
                        Emission::Real(item)
                    }),
                    Some(_) => None,
                }
                .unwrap_or(Emission::Cover(lane.class));
                let class = match &emission {
                    Emission::Real(_) => lane.class,
                    Emission::Cover(class) => *class,
                };
                lane.clock.last_start = Some((now_us, units_of(class)));
                EmitterStep::Emit(emission)
            }
        }
    }

    /// Retire the cell in flight on `peer`'s lane, releasing its quota and bytes.
    fn complete(&mut self, peer: Did) {
        let Some(lane) = self.lanes.get_mut(&peer) else {
            return;
        };
        let Some(bytes) = lane.in_flight_bytes.take() else {
            return;
        };
        if self.quota.release(peer) {
            lane.pending_bytes = lane.pending_bytes.saturating_sub(bytes);
            self.pending_bytes = self.pending_bytes.saturating_sub(bytes);
        }
    }

    /// The link to `peer` is down, or its emitter cannot run: drop its lane and every queued
    /// cell, releasing their quota and bytes, and wake its emitter so it stops.
    fn close(&mut self, peer: Did) -> Vec<T> {
        let Some(mut lane) = self.lanes.remove(&peer) else {
            return Vec::new();
        };
        lane.wake();
        self.quota.release_peer(peer);
        self.pending_bytes = self.pending_bytes.saturating_sub(lane.pending_bytes);
        lane.queued.into_iter().map(|(item, _)| item).collect()
    }

    /// The links whose lanes are not in `up`.
    fn lanes_outside(&self, up: &[Did]) -> Vec<Did> {
        self.lanes
            .keys()
            .filter(|peer| !up.contains(peer))
            .copied()
            .collect()
    }
}

/// Shared endpoint and relay capability for one node's constant-rate onion link traffic.
///
/// Clones share the same lanes and clock. This is the single effect boundary through which
/// cells enter the overlay: relays enqueue without waiting, endpoint adapters may await the send
/// of their own cell, and the data plane's link facts open and close the lanes.
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
    pub(crate) fn open(&self, scope: Scope, link: OnionLink) -> Result<()> {
        let spawner = Spawner::current()?;
        if lock(&self.state)?.open(link.peer) {
            spawner.spawn(emit(self.clone(), link.peer, scope));
        }
        Ok(())
    }

    /// The link to `link` is down: its queued cells are dropped and its emitter stops.
    pub(crate) fn close(&self, link: OnionLink) -> Result<()> {
        lock(&self.state)?.close(link.peer);
        Ok(())
    }

    /// Exactly the links `up` are up: close every other lane and open the missing ones.
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

    /// Queue one cell for `link` without waiting for it to leave.
    pub(crate) fn enqueue(&self, scope: Scope, link: OnionLink, payload: Bytes) -> Result<()> {
        self.enqueue_with(scope, link, payload, None)
    }

    /// Queue one cell for `link` and wait until its direct-edge send has completed.
    pub(crate) async fn send(&self, scope: Scope, link: OnionLink, payload: Bytes) -> Result<()> {
        let (completion, completed) = oneshot::channel();
        self.enqueue_with(scope, link, payload, Some(completion))?;
        completed.await.map_err(|_| {
            crate::error::Error::OnionRouteError(crate::onion::OnionRouteError::LinkSendCancelled)
        })?
    }

    /// Queue one cell, spawning the link's emitter if the link had none.
    fn enqueue_with(
        &self,
        scope: Scope,
        link: OnionLink,
        payload: Bytes,
        completion: Option<oneshot::Sender<Result<()>>>,
    ) -> Result<()> {
        let class = OnionLoopClass::from_cell_bytes(payload.len()).ok_or(
            crate::error::Error::OnionRouteError(crate::onion::OnionRouteError::InvalidCell),
        )?;
        // Acquired before the lane is claimed, so a missing runtime leaves no lane unemitted.
        let spawner = Spawner::current()?;
        let item_bytes = payload.len();
        let peer = link.peer;
        let should_spawn = lock(&self.state)?
            .enqueue(
                peer,
                OverlaySend {
                    scope: scope.clone(),
                    payload,
                    completion,
                },
                item_bytes,
                class,
                self.now_us(),
            )
            .map_err(|reason| OnionQueueKind::CircuitData.admission(peer, reason))?;
        if should_spawn {
            spawner.spawn(emit(self.clone(), peer, scope));
        }
        Ok(())
    }
}

/// The emitter of one up link: the effectful shell of [`plan`] (see the module diagram).
async fn emit(sender: OnionLinkSender, peer: Did, scope: Scope) {
    loop {
        let now_us = sender.now_us();
        let Ok(step) = lock(&sender.state).map(|mut state| state.next(peer, sender.floor, now_us))
        else {
            tracing::debug!(%peer, "onion link emitter lost its lane state");
            return;
        };
        match step {
            EmitterStep::Stop => return,
            EmitterStep::Wait(due_us, woken) => {
                let delay = Duration::from_micros(
                    u64::try_from(due_us.saturating_sub(now_us)).unwrap_or(u64::MAX),
                );
                let timer = sleep(delay).fuse();
                futures::pin_mut!(timer);
                let woken = woken.fuse();
                futures::pin_mut!(woken);
                futures::select! {
                    slept = timer => if let Err(error) = slept {
                        let cancelled = lock(&sender.state).map(|mut state| state.close(peer).len());
                        tracing::debug!(%peer, ?error, ?cancelled, "onion link emitter has no timer");
                        return;
                    },
                    _ = woken => {},
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
                let result = scope.send_direct(peer, payload).await;
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
                    state.complete(peer);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rings_core::dht::Did;

    use super::is_active;
    use super::plan;
    use super::slot_micros;
    use super::units_of;
    use super::Emission;
    use super::EmitterStep;
    use super::LinkClock;
    use super::OnionIdleFloor;
    use super::OrderedSendState;
    use super::Plan;
    use super::MAX_PENDING_ONION_SENDS_PER_PEER;
    use super::MAX_PENDING_ONION_SEND_BYTES_PER_PEER;
    use super::ONION_LINK_BUDGET_UNITS;
    use super::ONION_LINK_DWELL_US;
    use crate::error::OnionQueueAdmissionReason;
    use crate::error::Result;
    use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
    use crate::onion::sphinx::class::OnionLoopClass;

    /// `V` in microseconds.
    const WINDOW_US: u128 = ONION_FORWARD_MAX_VALIDITY_MS * 1_000;

    /// The emission starts of one link over `[0, horizon)` under the pure schedule, with real
    /// cells enqueued at `reals` (each one emitted in the first slot it is due), as the emitter
    /// produces them: at each due instant, or at an enqueue that wakes it.
    fn schedule(reals: &[u128], floor: OnionIdleFloor, horizon_us: u128) -> Vec<(u128, bool)> {
        let units = units_of(OnionLoopClass::DEFAULT);
        let mut clock = LinkClock::default();
        let mut queued = 0_usize;
        let mut pending = reals.iter().copied().peekable();
        let mut starts = Vec::new();
        let mut now_us = 0;
        while now_us < horizon_us {
            while let Some(real_us) = pending.next_if(|real_us| *real_us <= now_us) {
                clock.last_real_us = Some(real_us);
                queued += 1;
            }
            match plan(clock, queued > 0, floor, now_us) {
                Plan::Emit => {
                    let real = queued > 0;
                    queued = queued.saturating_sub(1);
                    clock.last_start = Some((now_us, units));
                    starts.push((now_us, real));
                }
                Plan::WaitUntil(due_us) => {
                    now_us = pending
                        .peek()
                        .map_or(due_us, |real_us| due_us.min((*real_us).max(now_us)));
                }
            }
        }
        starts
    }

    /// The emissions of `starts` within `[from, to)`.
    fn count_in(starts: &[(u128, bool)], from_us: u128, to_us: u128) -> u128 {
        let count = starts
            .iter()
            .filter(|(start_us, _)| (from_us..to_us).contains(start_us))
            .count();
        u128::try_from(count).unwrap_or(u128::MAX)
    }

    #[test]
    fn test_slot_is_the_reciprocal_of_the_emission_rate() {
        assert_eq!(slot_micros(units_of(OnionLoopClass::DEFAULT)), 10_173);
        for units in [1, 4, 16, 64, 256, 768] {
            assert_eq!(
                slot_micros(units),
                (units * 150_000_000 * 10).div_ceil(9 * 16_384)
            );
        }
    }

    /// The floor is configurable downwards only, so `r_idle ≤ 1 unit/s < r`.
    #[test]
    fn test_the_idle_floor_is_configurable_downwards_only() {
        assert_eq!(
            OnionIdleFloor::per_unit(Duration::from_secs(1)),
            Some(OnionIdleFloor::DEFAULT)
        );
        assert!(OnionIdleFloor::per_unit(Duration::from_secs(4)).is_some());
        assert_eq!(OnionIdleFloor::per_unit(Duration::from_millis(999)), None);
        assert!(OnionIdleFloor::DEFAULT.slot_micros(1) > slot_micros(1));
    }

    /// Floor: an idle link emits exactly `r_idle`, one cover per unit period.
    #[test]
    fn test_an_idle_link_emits_exactly_the_floor() {
        let floor = OnionIdleFloor::DEFAULT;
        let starts = schedule(&[], floor, 10_000_000);

        assert_eq!(
            starts,
            (0..10).map(|k| (k * 1_000_000, false)).collect::<Vec<_>>()
        );
    }

    /// Dwell: a real cell at `t` keeps the rate at `r` on `[t, t + D)`, and the link returns to
    /// the floor after `D` without a real cell.
    #[test]
    fn test_a_real_cell_keeps_the_link_active_for_the_dwell() {
        let floor = OnionIdleFloor::DEFAULT;
        let t = 5_000_000;
        let starts = schedule(&[t], floor, t + ONION_LINK_DWELL_US + 5_000_000);
        let slot_us = slot_micros(1);

        // The real cell leaves at `t`, woken from the idle wait.
        assert!(starts.contains(&(t, true)));
        // Active on [t, t + D): one emission per slot.
        let active = count_in(&starts, t, t + ONION_LINK_DWELL_US);
        assert!(
            active.abs_diff(ONION_LINK_DWELL_US / slot_us) <= 1,
            "{active}"
        );
        // After t + D: the floor again, spaced a unit period apart.
        let after = starts
            .iter()
            .filter(|(start_us, _)| *start_us >= t + ONION_LINK_DWELL_US)
            .map(|(start_us, _)| *start_us)
            .collect::<Vec<_>>();
        assert!(!after.is_empty());
        assert!(after
            .windows(2)
            .all(|pair| pair[1] - pair[0] == floor.slot_micros(1)));
        assert!(!is_active(
            LinkClock {
                last_real_us: Some(t),
                last_start: None,
            },
            false,
            t + ONION_LINK_DWELL_US,
        ));
    }

    /// Dwell extension: real cells arriving within `D` of each other keep the link active until
    /// `D` after the last one.
    #[test]
    fn test_real_cells_extend_the_dwell() {
        let floor = OnionIdleFloor::DEFAULT;
        let reals = [1_000_000, 20_000_000, 40_000_000];
        let end_us = 40_000_000 + ONION_LINK_DWELL_US;
        let starts = schedule(&reals, floor, end_us + 3_000_000);
        let slot_us = slot_micros(1);

        let active = count_in(&starts, 1_000_000, end_us);
        assert!(
            active.abs_diff((end_us - 1_000_000) / slot_us) <= 2,
            "{active}"
        );
    }

    /// Budget safety across phases: for every mix of idle and active periods, every window of
    /// length `V` carries at most `ρ·B + u` units, below `B`, and consecutive starts are never
    /// closer than one slot.
    #[test]
    fn test_no_window_of_v_exceeds_the_budget_across_phases() {
        let floor = OnionIdleFloor::DEFAULT;
        let reals = [
            0,
            3_000_000,
            31_000_000,
            97_000_000,
            97_000_500,
            160_000_000,
            250_000_000,
        ];
        let starts = schedule(&reals, floor, 3 * WINDOW_US);
        let slot_us = slot_micros(1);

        assert!(starts
            .windows(2)
            .all(|pair| pair[1].0 - pair[0].0 >= slot_us));
        for offset in (0..2 * WINDOW_US).step_by(1_000_000) {
            let units = count_in(&starts, offset, offset + WINDOW_US);
            assert!(
                units <= ONION_LINK_BUDGET_UNITS * 9 / 10 + 1,
                "offset {offset}"
            );
            assert!(units < ONION_LINK_BUDGET_UNITS);
        }
    }

    /// Substitution: queued real cells take the slots in FIFO order and cover fills the others,
    /// one cell in flight at a time; a closed link stops its emitter and drops its lane.
    #[test]
    fn test_state_substitutes_real_cells_and_stops_on_close() {
        let peer = Did::from(1_u32);
        let class = OnionLoopClass::DEFAULT;
        let floor = OnionIdleFloor::DEFAULT;
        let mut state = OrderedSendState::default();

        assert_eq!(state.enqueue(peer, 1, 7, class, 0), Ok(true));
        assert_eq!(state.enqueue(peer, 2, 7, class, 0), Ok(false));
        assert!(matches!(
            state.next(peer, floor, 0),
            EmitterStep::Emit(Emission::Real(1))
        ));
        // One slot later, with the first cell still in flight: cover.
        let slot_us = slot_micros(1);
        assert!(matches!(
            state.next(peer, floor, 1),
            EmitterStep::Wait(due, _) if due == slot_us
        ));
        assert!(matches!(
            state.next(peer, floor, slot_us),
            EmitterStep::Emit(Emission::Cover(_))
        ));
        state.complete(peer);
        assert!(matches!(
            state.next(peer, floor, 2 * slot_us),
            EmitterStep::Emit(Emission::Real(2))
        ));
        state.complete(peer);

        assert_eq!(state.close(peer), Vec::<i32>::new());
        assert!(matches!(
            state.next(peer, floor, 3 * slot_us),
            EmitterStep::Stop
        ));
        assert_eq!(state.quota.total(), 0);
        assert_eq!(state.pending_bytes, 0);
        assert!(state.open(peer));
        assert!(!state.open(peer));
    }

    /// No lost wake-up: an enqueue during the emitter's wait fires its wake-up, and the cell is
    /// due one active slot after the last emission.
    #[test]
    fn test_an_enqueue_wakes_the_waiting_emitter() {
        let peer = Did::from(5_u32);
        let class = OnionLoopClass::DEFAULT;
        let floor = OnionIdleFloor::DEFAULT;
        let mut state = OrderedSendState::<u32>::default();
        assert!(state.open(peer));
        assert!(matches!(
            state.next(peer, floor, 0),
            EmitterStep::Emit(Emission::Cover(_))
        ));
        let EmitterStep::Wait(due, mut woken) = state.next(peer, floor, 1) else {
            panic!("an idle link waits for its floor");
        };
        assert_eq!(due, floor.slot_micros(1));

        assert_eq!(state.enqueue(peer, 9, 1, class, 20_000), Ok(false));
        assert_eq!(woken.try_recv(), Ok(Some(())));
        assert!(matches!(
            state.next(peer, floor, 20_000),
            EmitterStep::Emit(Emission::Real(9))
        ));
    }

    /// Cover takes the class of the link's last real cell.
    #[test]
    fn test_cover_takes_the_class_of_the_last_real_cell() -> Result<()> {
        let peer = Did::from(2_u32);
        let large = OnionLoopClass::try_from(crate::onion::circuit::OnionCellBucket::MiB1)
            .map_err(|_| crate::error::Error::InvalidData)?;
        let floor = OnionIdleFloor::DEFAULT;
        let mut state = OrderedSendState::default();
        state
            .enqueue(peer, 1, 1, large, 0)
            .map_err(|_| crate::error::Error::InvalidData)?;
        assert!(matches!(
            state.next(peer, floor, 0),
            EmitterStep::Emit(Emission::Real(1))
        ));
        state.complete(peer);
        assert!(matches!(
            state.next(peer, floor, slot_micros(units_of(large))),
            EmitterStep::Emit(Emission::Cover(class)) if class == large
        ));
        Ok(())
    }

    /// One link cannot exceed its queue share, and the global byte bound holds exactly.
    #[test]
    fn test_queue_bounds_hold_per_link_and_globally() {
        let class = OnionLoopClass::DEFAULT;
        let peer = Did::from(3_u32);
        let other = Did::from(4_u32);
        let mut state = OrderedSendState::default();
        for value in 0..MAX_PENDING_ONION_SENDS_PER_PEER {
            assert!(state.enqueue(peer, value, 1, class, 0).is_ok());
        }
        assert_eq!(
            state.enqueue(peer, 200, 1, class, 0),
            Err(OnionQueueAdmissionReason::PeerFull)
        );
        assert_eq!(state.enqueue(other, 201, 1, class, 0), Ok(true));

        let mut global = OrderedSendState::default();
        for peer_id in 20_u32..24 {
            assert!(global
                .enqueue(
                    Did::from(peer_id),
                    peer_id,
                    MAX_PENDING_ONION_SEND_BYTES_PER_PEER,
                    class,
                    0,
                )
                .is_ok());
        }
        assert_eq!(
            global.enqueue(Did::from(24_u32), 24, 1, class, 0),
            Err(OnionQueueAdmissionReason::GlobalFull)
        );
        assert_eq!(global.close(Did::from(20_u32)), vec![20]);
        assert_eq!(global.enqueue(Did::from(24_u32), 24, 1, class, 0), Ok(true));
    }
}
