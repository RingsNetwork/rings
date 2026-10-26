//! Composed model check of liveness over credit, lanes, receive budgets, egress and a third peer.
//!
//! One prober `A` judges one subject `B`; `B` routes the report of a request `A` relayed through
//! a third peer `M`. Time advances in ticks of [`TICK_MS`].
//!
//! ```text
//!   A ──lane 0: Request, Probe(n)─────▶ B ──report──▶ M     (M may withhold B's credit)
//!   A ──data lane: Chunk(0), Chunk(1)─▶ B                   (one chunked message)
//!   A ◀──lane 0: Answer(n)──────────── B
//!   A ◀──app lane: Chatter──────────── B                    (a withholding B may chatter)
//! ```
//!
//! What is production and what is a replica:
//! - **Production, run unchanged:** [`ProbeClock`] (period 3 ticks, deadline 9 ticks from
//!   issue), keyed by the production challenge [`ProbeRequest`].
//! - **Production constant, replica code:** the first-frame deadline of a detached send
//!   ([`IRREVOCABLE_SEND_COMPLETION_TIMEOUT`], 5 ticks), which only a mutant applies to the
//!   probe; no production deadline code runs here.
//! - **Replica, as transitions:** credit, lanes, the reassembler, handlers and egress. Each rule
//!   below is a law the code keeps by construction or witnesses in a test on the real
//!   transport; the model checks that the laws together judge every subject right, not that the
//!   code keeps them.
//!   - Lane-0 service is local: an honest `B` serves its lane-0 head within the tick, and
//!     its report toward `M` is queued without waiting (`Egress<Protocol>`, detached handshakes;
//!     `message::effects::test_inbound_locality`). A report that `M` holds without credit keeps
//!     holding `B`'s shared outbound capacity (`report_held`).
//!   - Lane isolation: lane-0 credit is granted apart from the data lanes' receive budget,
//!     and lane-0 service never waits on a partly reassembled data message
//!     (`transport::tests::test_inbound::test_application_backpressure`). A lane hands its next
//!     frame to `B`'s actor only once the previous one completed (`InboundCompletion::Await`),
//!     so each lane holds at most one event of the per-peer share, [`PEER_EVENTS`], which a
//!     lane-0 event needs a free slot of. Hand-off is a transition; the exception for frames
//!     released from a hold (`LogHeld`) and the node's frame pool (`NodeFramePool`), which
//!     lane 0 never borrows from, are not modelled.
//!   - Probe and answer slots: a probe, and a direct answer, take no shared outbound capacity
//!     (`outbound::sender_bounded`), so frames that other traffic holds cannot refuse them. An
//!     answer `B` relays for another origin takes shared capacity, never the slot, so a flood of
//!     relayed probes cannot occupy it.
//!   - The probe waits for credit until its own deadline, never the shorter first-frame one.
//!   - An unsent probe stays outstanding, charged from its issue, and the next pass that finds
//!     the connection ready sends it again (`ProbeClock::unsent`, `ProbeWork::Resend`).
//!
//! An urgent action (a due arrival, lane-0 service, a due probe, an expiry, a due drop) is
//! taken before time may pass. A frame takes 0 or [`SLOW_HOP`] ticks per hop, chosen by the
//! checker; an honest round trip stays inside the deadline. Credit that returns lets held
//! frames leave at once.
//!
//! Subjects: five honest ones (fast; under load, with a stalled data lane and an application
//! that never reads; late to grant credit; slow to handle a data-lane backlog; unready at the
//! first probe only) and two that dodge judgment (withholding lane-0 credit; keeping its
//! connection unready at every probe pass). The environment may also flood `B` with probes of
//! other origins whose answers `B` relays through `A`.
//!
//! Properties:
//! - `Always` an honest subject is never evicted;
//! - `Eventually` a dodging subject is evicted (every path ends at the horizon, after the first
//!   probe's deadline);
//! - `Sometimes` a probe is answered, and a dodging subject's eviction is reachable, so neither
//!   is vacuous.
//!
//! Each refuted design is a mutant that breaks one law above, and the checker refutes it by the
//! property it breaks; production refutes nothing.

use std::collections::BTreeSet;

use rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use super::Outstanding;
use super::ProbeClock;
use super::PEER_LIVENESS_PERIOD_MS;
use super::PEER_LIVENESS_TIMEOUT_MS;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;

/// Milliseconds per model tick.
const TICK_MS: i64 = 5_000;
/// The slow per-hop latency, in ticks: two slow hops stay inside the deadline.
const SLOW_HOP: u8 = 4;
/// The last tick: past the first probe's deadline (issued at the period, 3 ticks).
const HORIZON: u8 = 13;
/// How many times a withholding subject may chatter.
const CHATTER: u8 = 3;
/// The first-frame deadline of a detached send, in milliseconds.
const FIRST_FRAME_MS: u128 = IRREVOCABLE_SEND_COMPLETION_TIMEOUT.as_millis();
/// The events of `B`'s per-peer actor share, which reserves none per lane: one per lane of the
/// model (lane 0 and the data lane), as production's share exceeds its lanes.
const PEER_EVENTS: u8 = 2;
/// The frames `A` has queued on a slow-handler subject's data lane: more than the share.
const DATA_BACKLOG: u8 = 3;
/// The tick from which a transiently unready subject's connection is ready: after the first
/// probe's issue, early enough that a fast round trip of its resend beats the deadline.
const READY_AT: u8 = 5;
/// The tick from which a late subject grants credit: after the first probe's first-frame
/// deadline, before its answer deadline.
const CREDIT_RETURNS: u8 = 9;

const _: () = assert!(PEER_LIVENESS_PERIOD_MS % TICK_MS == 0);
const _: () = assert!(PEER_LIVENESS_TIMEOUT_MS % TICK_MS == 0);
const _: () = assert!(FIRST_FRAME_MS.is_multiple_of(TICK_MS as u128));
const _: () = assert!(2 * (SLOW_HOP as i64) * TICK_MS < PEER_LIVENESS_TIMEOUT_MS);
const _: () =
    assert!((CREDIT_RETURNS as i64) * TICK_MS > PEER_LIVENESS_PERIOD_MS + (FIRST_FRAME_MS as i64));
const _: () =
    assert!((CREDIT_RETURNS as i64) * TICK_MS < PEER_LIVENESS_PERIOD_MS + PEER_LIVENESS_TIMEOUT_MS);
const _: () = assert!((READY_AT as i64) * TICK_MS > PEER_LIVENESS_PERIOD_MS);
const _: () =
    assert!((READY_AT as i64) * TICK_MS < PEER_LIVENESS_PERIOD_MS + PEER_LIVENESS_TIMEOUT_MS);

/// How the subject behaves.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Subject {
    /// Honest: grants credit and reads its inbox.
    Fast,
    /// Honest under load: its receive budget takes one frame of `A`'s data lane, so the rest
    /// of `A`'s chunked message stalls for good, and its application never reads.
    Loaded,
    /// Honest, over a fast link, but grants `A` no credit before [`CREDIT_RETURNS`].
    LateCredit,
    /// Honest: its handler on the data lane runs past the horizon (a long storage batch), with
    /// [`DATA_BACKLOG`] frames of `A` queued on that lane.
    SlowHandler,
    /// Honest, over a fast link, but its connection is unready before [`READY_AT`], so the
    /// first probe does not reach its queue when issued.
    TransientlyUnready,
    /// Dodging: grants `A` no lane-0 credit; may chatter on its application lane.
    Withholding,
    /// Dodging: keeps its connection unready at every probe pass, so no probe reaches it.
    Flapping,
}

impl Subject {
    /// Whether the subject is honest, so must never be evicted.
    fn honest(self) -> bool {
        matches!(
            self,
            Self::Fast
                | Self::Loaded
                | Self::LateCredit
                | Self::SlowHandler
                | Self::TransientlyUnready
        )
    }
}

/// The discipline under check: production, or one mutant per refuted design.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Discipline {
    /// The production liveness and egress.
    Production,
    /// A credit stall on a data lane is a verdict.
    StallIsVerdict,
    /// The deadline starts when the probe leaves the queue.
    ClockFromDeparture,
    /// A handler waits for its send to the next hop on the inbound lane.
    HandlerWaitsOnRemote,
    /// Any inbound payload clears the outstanding probe.
    InboundClears,
    /// A probe takes shared capacity, which other frames to the subject hold; a
    /// refused probe is withdrawn.
    ProbeTakesSharedCapacity,
    /// A due probe replaces an outstanding one, keeping its deadline.
    ReissueOverOutstanding,
    /// Lane-0 service awaits the application (#935).
    ApplicationOnControlLane,
    /// Lane-0 credit draws on the receive budget the data lanes spend.
    SharedReceiveBudget,
    /// Lane-0 service waits behind a partly reassembled data message.
    ReassemblyBarrier,
    /// A probe waiting for credit is dropped at the first-frame deadline.
    ProbeDroppedAtFirstFrame,
    /// An answer takes shared capacity, which other frames of the subject hold.
    AnswerTakesSharedCapacity,
    /// A probe issued while the connection is unready is withdrawn uncharged.
    WithdrawWhenUnready,
    /// A lane hands every frame over without awaiting the previous one, so one
    /// lane's backlog fills the per-peer share.
    HandOffWithoutAwait,
    /// An unsent probe is kept outstanding but never sent again.
    NeverResend,
    /// An answer `B` relays for another origin takes `B`'s answer slot toward the next hop, so
    /// a flood of relayed answers pushes `B`'s own answer onto shared capacity.
    RelayedAnswerTakesSlot,
}

/// A frame of the model.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Frame {
    /// A request whose report the subject routes through `M`.
    Request,
    /// The probe of the given number.
    Probe(u8),
    /// The answer to the probe of the given number.
    Answer(u8),
    /// The chunk of the given index of `A`'s two-chunk data message.
    Chunk(u8),
    /// Application traffic from the subject.
    Chatter,
}

/// The composed state.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct State {
    now: u8,
    /// `A`'s clock of `B`.
    clock: ProbeClock,
    /// The next probe number.
    next_probe: u8,
    /// Probes issued but not yet started (the departure mutant starts them on departure).
    unstarted: Vec<u8>,
    /// `A`'s lane-0 frames that have no credit to leave.
    held: Vec<Frame>,
    /// `A`'s data-lane frames that have no credit to leave.
    data_held: Vec<Frame>,
    /// The tick since which `A`'s data lane toward `B` has been stalled, if it is.
    stalled_since: Option<u8>,
    /// Frames in flight, with their arrival tick, toward `B` and toward `A`.
    to_b: Vec<(Frame, u8)>,
    to_a: Vec<(Frame, u8)>,
    /// `B`'s lane-0 frames awaiting service, in order.
    lane0: Vec<Frame>,
    /// Whether `B` holds part of `A`'s chunked message.
    partial: bool,
    /// Whether `B`'s report toward `M` waits without credit, holding `B`'s shared capacity.
    report_held: bool,
    /// `A`'s data-lane frames queued at `B`'s transport, not yet handed to `B`'s actor.
    data_backlog: u8,
    /// The events of `B`'s per-peer share its data lane holds, which never complete.
    actor_data: u8,
    /// Whether `B`'s lane-0 service is blocked for good.
    blocked: bool,
    chatter_left: u8,
    answered: u8,
    evicted: bool,
}

/// One transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Action {
    /// One tick passes.
    Tick,
    /// The frame at the given index of `to_b` or `to_a` arrives.
    ArriveAtB(usize),
    ArriveAtA(usize),
    /// `B` serves its lane-0 head, replying after the given hop latency.
    Serve(u8),
    /// `A` issues a due probe, sent after the given hop latency.
    Issue(u8),
    /// `A` sends its unsent outstanding probe again, after the given hop latency.
    Resend(u8),
    /// `B`'s data lane hands its next queued frame to `B`'s actor.
    HandOff,
    /// `A` evicts `B` on an expired probe or (a mutant) a stall verdict.
    Evict,
    /// (A mutant) `A` drops its held probe at the first-frame deadline.
    DropProbe,
    /// A withholding `B` chatters, arriving after the given hop latency.
    Chatter(u8),
}

/// The model of one subject under one discipline.
struct LivenessModel {
    subject: Subject,
    third_withholds: bool,
    /// Whether other origins keep `B` relaying answers through `A`.
    relayed_flood: bool,
    discipline: Discipline,
}

/// The production challenge of probe `n`.
fn challenge(n: u8) -> ProbeRequest {
    ProbeRequest {
        epoch: ProvisionalEpoch::from_unix_seconds(0),
        nonce: [n; 32],
    }
}

/// The probe number of `request`, the inverse of [`challenge`].
fn probe_number(request: ProbeRequest) -> u8 {
    request.nonce[0]
}

/// Milliseconds at tick `now`.
fn ms(now: u8) -> i64 {
    i64::from(now) * TICK_MS
}

impl LivenessModel {
    /// Whether `B` grants `A` lane-0 credit now.
    fn lane0_credit(&self, state: &State) -> bool {
        match self.subject {
            Subject::Withholding => false,
            Subject::LateCredit => state.now >= CREDIT_RETURNS,
            Subject::Loaded => {
                self.discipline != Discipline::SharedReceiveBudget || state.data_held.is_empty()
            }
            Subject::Fast
            | Subject::SlowHandler
            | Subject::TransientlyUnready
            | Subject::Flapping => true,
        }
    }

    /// Whether `B` grants `A` credit for data-lane `frame` now.
    fn data_credit(&self, state: &State, frame: Frame) -> bool {
        match self.subject {
            Subject::Loaded => frame == Frame::Chunk(0),
            Subject::LateCredit => state.now >= CREDIT_RETURNS,
            Subject::Fast
            | Subject::SlowHandler
            | Subject::TransientlyUnready
            | Subject::Withholding
            | Subject::Flapping => true,
        }
    }

    /// The hop latencies the checker may choose.
    fn hops(&self) -> &'static [u8] {
        match self.subject {
            Subject::LateCredit | Subject::TransientlyUnready => &[0],
            _ => &[0, SLOW_HOP],
        }
    }

    /// Whether `A`'s probe is due.
    fn probe_due(&self, state: &State) -> bool {
        match self.discipline {
            Discipline::ReissueOverOutstanding => {
                ms(state.now).saturating_sub(state.clock.last_issued_ms) >= PEER_LIVENESS_PERIOD_MS
            }
            Discipline::ClockFromDeparture => {
                state.unstarted.is_empty() && state.clock.due(ms(state.now))
            }
            _ => state.clock.due(ms(state.now)),
        }
    }

    /// Whether `A`'s connection to `B` is ready, so a probe sent now reaches the queue.
    fn ready(&self, state: &State) -> bool {
        match self.subject {
            Subject::Flapping => false,
            Subject::TransientlyUnready => state.now >= READY_AT,
            _ => true,
        }
    }

    /// Whether the probe pass sends `A`'s unsent outstanding probe again now.
    fn resends(&self, state: &State) -> bool {
        self.discipline != Discipline::NeverResend
            && self.ready(state)
            && state.clock.unsent().is_some()
    }

    /// Whether `B`'s data lane may hand its next queued frame to the actor now: the share has
    /// a free slot, and (unless a mutant) the lane's previous event completed.
    fn hands_off(&self, state: &State) -> bool {
        let awaited = self.discipline == Discipline::HandOffWithoutAwait || state.actor_data == 0;
        state.data_backlog > 0 && state.actor_data < PEER_EVENTS && awaited
    }

    /// Whether `B`'s own answer to `A` takes shared outbound capacity rather than its slot.
    fn answer_shares_capacity(&self) -> bool {
        self.discipline == Discipline::AnswerTakesSharedCapacity
            || (self.discipline == Discipline::RelayedAnswerTakesSlot && self.relayed_flood)
    }

    /// Whether `A` evicts `B` now: on an expired probe, or (a mutant) on a data-lane stall as
    /// old as the deadline.
    fn evicts(&self, state: &State) -> bool {
        let stalled = self.discipline == Discipline::StallIsVerdict
            && state.stalled_since.is_some_and(|since| {
                ms(state.now).saturating_sub(ms(since)) >= PEER_LIVENESS_TIMEOUT_MS
            });
        !state.evicted && (state.clock.expiry(ms(state.now)).is_some() || stalled)
    }

    /// Whether (a mutant) `A` drops its held probe now, at the first-frame deadline.
    fn drops_probe(&self, state: &State) -> bool {
        self.discipline == Discipline::ProbeDroppedAtFirstFrame
            && state
                .held
                .iter()
                .any(|frame| matches!(frame, Frame::Probe(_)))
            && state.clock.outstanding_since_ms().is_some_and(|since| {
                u128::try_from(ms(state.now).saturating_sub(since))
                    .is_ok_and(|waited| waited >= FIRST_FRAME_MS)
            })
    }

    /// Whether `B` can serve its lane-0 head now.
    fn serves(&self, state: &State) -> bool {
        let barrier = self.discipline == Discipline::ReassemblyBarrier && state.partial;
        let share_full = state.actor_data >= PEER_EVENTS;
        !state.lane0.is_empty() && !state.blocked && !state.evicted && !barrier && !share_full
    }

    /// Whether some action must happen before time may pass.
    fn urgent(&self, state: &State) -> bool {
        let due = |queue: &[(Frame, u8)]| queue.iter().any(|(_, at)| *at <= state.now);
        !state.evicted
            && (due(&state.to_b)
                || due(&state.to_a)
                || self.serves(state)
                || self.probe_due(state)
                || self.resends(state)
                || self.hands_off(state)
                || self.evicts(state)
                || self.drops_probe(state))
    }

    /// `frame` leaves `A` toward `B`, arriving after `hop` ticks.
    fn leave(&self, state: &mut State, frame: Frame, hop: u8) {
        state.to_b.push((frame, state.now + hop));
        if let (Discipline::ClockFromDeparture, Frame::Probe(n)) = (self.discipline, frame) {
            state.unstarted.retain(|unstarted| *unstarted != n);
            state.clock.issue(challenge(n), ms(state.now));
        }
    }

    /// Send `frame` from `A` to `B` on lane 0, leaving after `hop` ticks if it has credit.
    fn send_to_b(&self, state: &mut State, frame: Frame, hop: u8) {
        if state.held.is_empty() && self.lane0_credit(state) {
            self.leave(state, frame, hop);
        } else {
            state.held.push(frame);
        }
    }

    /// Send `frame` from `A` to `B` on the data lane, if it has credit.
    fn send_data(&self, state: &mut State, frame: Frame) {
        if state.data_held.is_empty() && self.data_credit(state, frame) {
            self.leave(state, frame, 0);
        } else {
            state.data_held.push(frame);
        }
    }

    /// Held frames whose credit returned leave at once, in order; the stall clock follows.
    fn depart(&self, state: &mut State) {
        while let Some(&frame) = state.held.first() {
            if !self.lane0_credit(state) {
                break;
            }
            state.held.remove(0);
            self.leave(state, frame, 0);
        }
        while let Some(&frame) = state.data_held.first() {
            if !self.data_credit(state, frame) {
                break;
            }
            state.data_held.remove(0);
            self.leave(state, frame, 0);
        }
        state.stalled_since = match (state.data_held.is_empty(), state.stalled_since) {
            (true, _) => None,
            (false, since) => since.or(Some(state.now)),
        };
    }

    /// `A` issues probe `next_probe` at the current tick, under the discipline.
    fn issue(&self, state: &mut State, hop: u8) {
        let n = state.next_probe;
        state.next_probe += 1;
        let now = ms(state.now);
        match self.discipline {
            Discipline::ClockFromDeparture => {
                state.clock.last_issued_ms = now;
                state.unstarted.push(n);
            }
            Discipline::ReissueOverOutstanding => {
                let first = state.clock.outstanding_since_ms().unwrap_or(now);
                state.clock.outstanding = Some(Outstanding {
                    request: challenge(n),
                    issued_ms: first,
                    queued: false,
                });
                state.clock.last_issued_ms = now;
            }
            _ => {
                state.clock.issue(challenge(n), now);
            }
        }
        self.dispatch(state, n, hop);
    }

    /// `A` sends its unsent outstanding probe again.
    fn resend(&self, state: &mut State, hop: u8) {
        if let Some(request) = state.clock.unsent() {
            self.dispatch(state, probe_number(request), hop);
        }
    }

    /// `A` sends probe `n` toward `B`: it reaches the queue only over a ready connection, and
    /// (a mutant) only when shared capacity admits it.
    fn dispatch(&self, state: &mut State, n: u8, hop: u8) {
        if !self.ready(state) {
            if self.discipline == Discipline::WithdrawWhenUnready {
                state.clock.outstanding = None;
            }
            return;
        }
        let refused =
            self.discipline == Discipline::ProbeTakesSharedCapacity && !state.held.is_empty();
        if refused {
            state.clock.outstanding = None;
            return;
        }
        self.send_to_b(state, Frame::Probe(n), hop);
        state.clock.queued(challenge(n));
    }

    /// `B` serves its lane-0 head.
    fn serve(&self, state: &mut State, hop: u8) {
        let waits_on_application = self.discipline == Discipline::ApplicationOnControlLane
            && self.subject == Subject::Loaded;
        if waits_on_application {
            state.blocked = true;
            return;
        }
        let head = state.lane0.remove(0);
        match head {
            Frame::Request => {
                if self.third_withholds {
                    state.report_held = true;
                    state.blocked |= self.discipline == Discipline::HandlerWaitsOnRemote;
                }
            }
            Frame::Probe(n) => {
                let refused = self.answer_shares_capacity() && state.report_held;
                if !refused {
                    state.to_a.push((Frame::Answer(n), state.now + hop));
                }
            }
            Frame::Answer(_) | Frame::Chunk(_) | Frame::Chatter => {}
        }
    }

    /// `B` receives `frame`: lane-0 frames queue for service, chunks go to the reassembler.
    fn receive_at_b(&self, state: &mut State, frame: Frame) {
        match frame {
            Frame::Chunk(index) => state.partial = index == 0,
            Frame::Request | Frame::Probe(_) | Frame::Answer(_) | Frame::Chatter => {
                state.lane0.push(frame)
            }
        }
    }

    /// `A` receives `frame`.
    fn receive_at_a(&self, state: &mut State, frame: Frame) {
        match frame {
            Frame::Answer(n) => {
                if state.clock.answer(challenge(n)) {
                    state.answered += 1;
                }
            }
            Frame::Chatter if self.discipline == Discipline::InboundClears => {
                if let Some(outstanding) = state.clock.outstanding {
                    state.clock.answer(outstanding.request);
                }
            }
            Frame::Request | Frame::Probe(_) | Frame::Chunk(_) | Frame::Chatter => {}
        }
    }
}

impl Model for LivenessModel {
    type Action = Action;
    type State = State;

    fn init_states(&self) -> Vec<State> {
        let mut state = State {
            now: 0,
            clock: ProbeClock::admitted_at(0),
            next_probe: 0,
            unstarted: Vec::new(),
            held: Vec::new(),
            data_held: Vec::new(),
            stalled_since: None,
            to_b: Vec::new(),
            to_a: Vec::new(),
            lane0: Vec::new(),
            partial: false,
            report_held: false,
            data_backlog: if self.subject == Subject::SlowHandler {
                DATA_BACKLOG
            } else {
                0
            },
            actor_data: 0,
            blocked: false,
            chatter_left: if self.subject == Subject::Withholding {
                CHATTER
            } else {
                0
            },
            answered: 0,
            evicted: false,
        };
        self.send_data(&mut state, Frame::Chunk(0));
        self.send_data(&mut state, Frame::Chunk(1));
        self.send_to_b(&mut state, Frame::Request, 0);
        self.depart(&mut state);
        vec![state]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Action>) {
        if state.evicted {
            return;
        }
        if !self.urgent(state) && state.now < HORIZON {
            actions.push(Action::Tick);
        }
        let arrived = |queue: &[(Frame, u8)]| {
            queue
                .iter()
                .enumerate()
                .filter(|(_, (_, at))| *at <= state.now)
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        // The data lane delivers in order: a chunk arrives only after every earlier one.
        let first_chunk = state
            .to_b
            .iter()
            .position(|(frame, _)| matches!(frame, Frame::Chunk(_)));
        actions.extend(
            arrived(&state.to_b)
                .into_iter()
                .filter(|index| {
                    let chunk = state
                        .to_b
                        .get(*index)
                        .is_some_and(|(frame, _)| matches!(frame, Frame::Chunk(_)));
                    !chunk || first_chunk == Some(*index)
                })
                .map(Action::ArriveAtB),
        );
        actions.extend(arrived(&state.to_a).into_iter().map(Action::ArriveAtA));
        for &hop in self.hops() {
            if self.serves(state) {
                actions.push(Action::Serve(hop));
            }
            if self.probe_due(state) {
                actions.push(Action::Issue(hop));
            }
            if state.chatter_left > 0 {
                actions.push(Action::Chatter(hop));
            }
            if self.resends(state) {
                actions.push(Action::Resend(hop));
            }
        }
        if self.hands_off(state) {
            actions.push(Action::HandOff);
        }
        if self.evicts(state) {
            actions.push(Action::Evict);
        }
        if self.drops_probe(state) {
            actions.push(Action::DropProbe);
        }
    }

    fn next_state(&self, last: &State, action: Action) -> Option<State> {
        let mut state = last.clone();
        match action {
            Action::Tick => state.now += 1,
            Action::ArriveAtB(index) => {
                let (frame, _) = state.to_b.remove(index);
                self.receive_at_b(&mut state, frame);
            }
            Action::ArriveAtA(index) => {
                let (frame, _) = state.to_a.remove(index);
                self.receive_at_a(&mut state, frame);
            }
            Action::Serve(hop) => self.serve(&mut state, hop),
            Action::Issue(hop) => self.issue(&mut state, hop),
            Action::Resend(hop) => self.resend(&mut state, hop),
            Action::HandOff => {
                state.data_backlog -= 1;
                state.actor_data += 1;
            }
            Action::Evict => state.evicted = true,
            Action::DropProbe => state.held.retain(|frame| !matches!(frame, Frame::Probe(_))),
            Action::Chatter(hop) => {
                state.chatter_left -= 1;
                state.to_a.push((Frame::Chatter, state.now + hop));
            }
        }
        self.depart(&mut state);
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always("an honest subject is never evicted", |model, state| {
                !model.subject.honest() || !state.evicted
            }),
            Property::<Self>::eventually("a dodging subject is evicted", |model, state| {
                model.subject.honest() || state.evicted
            }),
            Property::<Self>::sometimes("a probe is answered", |model, state| {
                !model.subject.honest() || state.answered > 0
            }),
            Property::<Self>::sometimes(
                "a dodging subject's eviction is reachable",
                |model, state| model.subject.honest() || state.evicted,
            ),
        ]
    }
}

/// The properties `discipline` violates for `subject` under an environment: whether the third
/// peer withholds credit, and whether other origins flood `B` with relayed probes.
fn refuted(
    discipline: Discipline,
    subject: Subject,
    (third_withholds, relayed_flood): (bool, bool),
) -> BTreeSet<&'static str> {
    let checker = LivenessModel {
        subject,
        third_withholds,
        relayed_flood,
        discipline,
    }
    .checker()
    .spawn_bfs()
    .join();
    checker
        .model()
        .properties()
        .into_iter()
        .filter(|property| {
            let discovered = checker.discovery(property.name).is_some();
            match property.expectation {
                Expectation::Sometimes => !discovered,
                _ => discovered,
            }
        })
        .map(|property| property.name)
        .collect()
}

/// Production satisfies every property for every subject, whether or not the third peer
/// withholds credit (and so holds the subject's shared outbound capacity), and whether or not
/// other origins flood the subject with relayed probes.
#[test]
fn test_production_liveness_satisfies_the_composed_laws() {
    for subject in [
        Subject::Fast,
        Subject::Loaded,
        Subject::LateCredit,
        Subject::SlowHandler,
        Subject::TransientlyUnready,
        Subject::Withholding,
        Subject::Flapping,
    ] {
        for environment in [(false, false), (true, false), (false, true), (true, true)] {
            assert_eq!(
                refuted(Discipline::Production, subject, environment),
                BTreeSet::new(),
                "{subject:?}, third withholds and relayed flood: {environment:?}"
            );
        }
    }
}

/// Every refuted design is refuted by the property it breaks.
#[test]
fn test_each_refuted_design_is_refuted_by_the_composed_model() {
    let never = "an honest subject is never evicted";
    let eventually = "a dodging subject is evicted";
    for (discipline, subject, environment, law) in [
        (
            Discipline::StallIsVerdict,
            Subject::Loaded,
            (false, false),
            never,
        ),
        (
            Discipline::ClockFromDeparture,
            Subject::Withholding,
            (false, false),
            eventually,
        ),
        (
            Discipline::HandlerWaitsOnRemote,
            Subject::Fast,
            (true, false),
            never,
        ),
        (
            Discipline::InboundClears,
            Subject::Withholding,
            (false, false),
            eventually,
        ),
        (
            Discipline::ProbeTakesSharedCapacity,
            Subject::Withholding,
            (false, false),
            eventually,
        ),
        (
            Discipline::ReissueOverOutstanding,
            Subject::Fast,
            (false, false),
            never,
        ),
        (
            Discipline::ApplicationOnControlLane,
            Subject::Loaded,
            (false, false),
            never,
        ),
        (
            Discipline::SharedReceiveBudget,
            Subject::Loaded,
            (false, false),
            never,
        ),
        (
            Discipline::ReassemblyBarrier,
            Subject::Loaded,
            (false, false),
            never,
        ),
        (
            Discipline::ProbeDroppedAtFirstFrame,
            Subject::LateCredit,
            (false, false),
            never,
        ),
        (
            Discipline::AnswerTakesSharedCapacity,
            Subject::Fast,
            (true, false),
            never,
        ),
        (
            Discipline::WithdrawWhenUnready,
            Subject::Flapping,
            (false, false),
            eventually,
        ),
        (
            Discipline::HandOffWithoutAwait,
            Subject::SlowHandler,
            (false, false),
            never,
        ),
        (
            Discipline::NeverResend,
            Subject::TransientlyUnready,
            (false, false),
            never,
        ),
        (
            Discipline::RelayedAnswerTakesSlot,
            Subject::Fast,
            (true, true),
            never,
        ),
    ] {
        assert!(
            refuted(discipline, subject, environment).contains(law),
            "{discipline:?} with {subject:?} must be refuted by `{law}`"
        );
    }
}
