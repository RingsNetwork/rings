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
//!   issue), and the first-frame deadline of a detached send
//!   ([`IRREVOCABLE_SEND_COMPLETION_TIMEOUT`], 5 ticks), which a mutant applies to the probe.
//! - **Replica, as transitions:** credit, lanes, the reassembler, handlers and egress. Each rule
//!   below is a law the code keeps by construction or witnesses in a test on the real
//!   transport; the model checks that the laws together judge every subject right, not that the
//!   code keeps them.
//!   - L2, lane-0 service is local: an honest `B` serves its lane-0 head within the tick, and
//!     its report toward `M` is queued without waiting (`ProtocolEgress`, detached handshakes;
//!     `message::effects::test_inbound_locality`). A report that `M` holds without credit keeps
//!     holding `B`'s shared outbound capacity (`report_held`).
//!   - L4, lane isolation: lane-0 credit is granted apart from the data lanes' receive budget,
//!     and lane-0 service never waits on a partly reassembled data message
//!     (`transport::tests::test_inbound::test_application_backpressure`).
//!   - Probe and answer slots: a probe, and an answer, take no shared outbound capacity
//!     (`outbound::sender_bounded`), so frames that other traffic holds cannot refuse them.
//!   - The probe waits for credit until its own deadline, never the shorter first-frame one.
//!   - An unsent probe stays outstanding, charged from its issue (`ProbeClock::unsent`).
//!
//! An urgent action (a due arrival, lane-0 service, a due probe, an expiry, a due drop) is
//! taken before time may pass. A frame takes 0 or [`SLOW_HOP`] ticks per hop, chosen by the
//! checker; an honest round trip stays inside the deadline. Credit that returns lets held
//! frames leave at once.
//!
//! Subjects: three honest ones (fast; under load, with a stalled data lane and an application
//! that never reads; late to grant credit) and two that dodge judgment (withholding lane-0
//! credit; keeping its connection unready at every probe pass).
//!
//! Properties:
//! - `Always` an honest subject is never evicted;
//! - `Eventually` a dodging subject is evicted (every path ends at the horizon, after the first
//!   probe's deadline);
//! - `Sometimes` a probe is answered, and a dodging subject's eviction is reachable, so neither
//!   is vacuous.
//!
//! Each design #913 R5–R10 and #935 refuted is a mutant that breaks one law above, and the
//! checker refutes it by the property it breaks; production refutes nothing.

use std::collections::BTreeSet;

use rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use super::ProbeClock;
use super::ProbeIdentity;
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
    /// Dodging: grants `A` no lane-0 credit; may chatter on its application lane.
    Withholding,
    /// Dodging: keeps its connection unready at every probe pass, so no probe reaches it.
    Flapping,
}

impl Subject {
    /// Whether the subject is honest, so must never be evicted.
    fn honest(self) -> bool {
        matches!(self, Self::Fast | Self::Loaded | Self::LateCredit)
    }
}

/// The discipline under check: production, or one mutant per refuted design.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Discipline {
    /// The production liveness and egress.
    Production,
    /// #913 R6 H1: a credit stall on a data lane is a verdict.
    StallIsVerdict,
    /// #913 R7 H1: the deadline starts when the probe leaves the queue.
    ClockFromDeparture,
    /// #913 R8 H1: a handler waits for its send to the next hop on the inbound lane.
    HandlerWaitsOnRemote,
    /// #913 R8 M1: any inbound payload clears the outstanding probe.
    InboundClears,
    /// #913 R8 M2: a probe takes shared capacity, which other frames to the subject hold; a
    /// refused probe is withdrawn.
    ProbeTakesSharedCapacity,
    /// #913 R9 H1: a due probe replaces an outstanding one, keeping its deadline.
    ReissueOverOutstanding,
    /// #935: lane-0 service awaits the application.
    ApplicationOnControlLane,
    /// #913 R10 H1, M4: lane-0 credit draws on the receive budget the data lanes spend.
    SharedReceiveBudget,
    /// #913 R10 H2: lane-0 service waits behind a partly reassembled data message.
    ReassemblyBarrier,
    /// #913 R10 M5: a probe waiting for credit is dropped at the first-frame deadline.
    ProbeDroppedAtFirstFrame,
    /// #913 R10 M6: an answer takes shared capacity, which other frames of the subject hold.
    AnswerTakesSharedCapacity,
    /// #913 R10 M7: a probe issued while the connection is unready is withdrawn uncharged.
    WithdrawWhenUnready,
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
    discipline: Discipline,
}

/// The production identity of probe `n`.
fn identity(n: u8) -> ProbeIdentity {
    ProbeIdentity {
        tx_id: uuid::Uuid::from_u128(u128::from(n)),
        request: ProbeRequest {
            epoch: ProvisionalEpoch::from_unix_seconds(0),
            nonce: [n; 32],
        },
    }
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
            Subject::Fast | Subject::Flapping => true,
        }
    }

    /// Whether `B` grants `A` credit for data-lane `frame` now.
    fn data_credit(&self, state: &State, frame: Frame) -> bool {
        match self.subject {
            Subject::Loaded => frame == Frame::Chunk(0),
            Subject::LateCredit => state.now >= CREDIT_RETURNS,
            Subject::Fast | Subject::Withholding | Subject::Flapping => true,
        }
    }

    /// The hop latencies the checker may choose.
    fn hops(&self) -> &'static [u8] {
        match self.subject {
            Subject::LateCredit => &[0],
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
        !state.lane0.is_empty() && !state.blocked && !state.evicted && !barrier
    }

    /// Whether some action must happen before time may pass.
    fn urgent(&self, state: &State) -> bool {
        let due = |queue: &[(Frame, u8)]| queue.iter().any(|(_, at)| *at <= state.now);
        !state.evicted
            && (due(&state.to_b)
                || due(&state.to_a)
                || self.serves(state)
                || self.probe_due(state)
                || self.evicts(state)
                || self.drops_probe(state))
    }

    /// `frame` leaves `A` toward `B`, arriving after `hop` ticks.
    fn leave(&self, state: &mut State, frame: Frame, hop: u8) {
        state.to_b.push((frame, state.now + hop));
        if let (Discipline::ClockFromDeparture, Frame::Probe(n)) = (self.discipline, frame) {
            state.unstarted.retain(|unstarted| *unstarted != n);
            state.clock.issue(identity(n), ms(state.now));
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
                state.clock.outstanding = Some((identity(n), first, true));
                state.clock.last_issued_ms = now;
            }
            _ => {
                state.clock.issue(identity(n), now);
            }
        }
        if self.subject == Subject::Flapping {
            // No ready connection: the probe does not reach the queue.
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
        state.clock.queued(identity(n));
        self.send_to_b(state, Frame::Probe(n), hop);
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
                let refused =
                    self.discipline == Discipline::AnswerTakesSharedCapacity && state.report_held;
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
                if state.clock.answer(identity(n)) {
                    state.answered += 1;
                }
            }
            Frame::Chatter if self.discipline == Discipline::InboundClears => {
                if let Some((outstanding, ..)) = state.clock.outstanding {
                    state.clock.answer(outstanding);
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

/// The properties `discipline` violates for `subject` with the third peer withholding or not.
fn refuted(
    discipline: Discipline,
    subject: Subject,
    third_withholds: bool,
) -> BTreeSet<&'static str> {
    let checker = LivenessModel {
        subject,
        third_withholds,
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
/// withholds credit (and so holds the subject's shared outbound capacity).
#[test]
fn test_production_liveness_satisfies_the_composed_laws() {
    for subject in [
        Subject::Fast,
        Subject::Loaded,
        Subject::LateCredit,
        Subject::Withholding,
        Subject::Flapping,
    ] {
        for third_withholds in [false, true] {
            assert_eq!(
                refuted(Discipline::Production, subject, third_withholds),
                BTreeSet::new(),
                "{subject:?}, third withholds: {third_withholds}"
            );
        }
    }
}

/// Every design #913 R5–R10 and #935 refuted is refuted by the property it breaks.
#[test]
fn test_each_refuted_design_is_refuted_by_the_composed_model() {
    let never = "an honest subject is never evicted";
    let eventually = "a dodging subject is evicted";
    for (discipline, subject, third_withholds, law) in [
        (Discipline::StallIsVerdict, Subject::Loaded, false, never),
        (
            Discipline::ClockFromDeparture,
            Subject::Withholding,
            false,
            eventually,
        ),
        (Discipline::HandlerWaitsOnRemote, Subject::Fast, true, never),
        (
            Discipline::InboundClears,
            Subject::Withholding,
            false,
            eventually,
        ),
        (
            Discipline::ProbeTakesSharedCapacity,
            Subject::Withholding,
            false,
            eventually,
        ),
        (
            Discipline::ReissueOverOutstanding,
            Subject::Fast,
            false,
            never,
        ),
        (
            Discipline::ApplicationOnControlLane,
            Subject::Loaded,
            false,
            never,
        ),
        (
            Discipline::SharedReceiveBudget,
            Subject::Loaded,
            false,
            never,
        ),
        (Discipline::ReassemblyBarrier, Subject::Loaded, false, never),
        (
            Discipline::ProbeDroppedAtFirstFrame,
            Subject::LateCredit,
            false,
            never,
        ),
        (
            Discipline::AnswerTakesSharedCapacity,
            Subject::Fast,
            true,
            never,
        ),
        (
            Discipline::WithdrawWhenUnready,
            Subject::Flapping,
            false,
            eventually,
        ),
    ] {
        assert!(
            refuted(discipline, subject, third_withholds).contains(law),
            "{discipline:?} with {subject:?} must be refuted by `{law}`"
        );
    }
}
