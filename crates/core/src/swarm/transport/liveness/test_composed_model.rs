//! Composed model check of liveness over credit, lanes, the application inbox and a third peer.
//!
//! One prober `A` judges one subject `B`; `B` routes the report of a request `A` relayed through
//! a third peer `M`. Time advances in ticks of [`TICK_MS`], so the production [`ProbeClock`]
//! runs unchanged (period 3 ticks, deadline 9 ticks).
//!
//! ```text
//!   A ──lane 0: Request, Probe(n)──▶ B ──report──▶ M        (M may withhold B's credit)
//!   A ◀──lane 0: Answer(n)───────── B
//!   A ◀──app lane: Chatter───────── B                       (a withholding B may chatter)
//! ```
//!
//! The subject is honest-fast, honest with a slow application (its inbox full, never read),
//! or withholding (it grants `A` no lane-0 credit, so `A`'s lane-0 frames never leave). An
//! honest node serves lane 0 within the tick, from local computation alone (L2): a step of
//! time is enabled only when no urgent action is (a due arrival, lane-0 service, a due probe,
//! an expiry). A frame takes 0 or [`SLOW_HOP`] ticks per hop, chosen by the checker; an honest
//! round trip stays inside the deadline.
//!
//! Properties:
//! - `Always` an honest subject is never evicted;
//! - `Eventually` a withholding subject is evicted (every path ends at the horizon, after the
//!   first probe's deadline);
//! - `Sometimes` a probe is answered, and a withholding subject's eviction is reachable, so
//!   neither is vacuous.
//!
//! Each High of #913 R5–R9, and the application coupling of #935, is a mutant that the checker
//! refutes; production refutes nothing.

use std::collections::BTreeSet;

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

const _: () = assert!(PEER_LIVENESS_PERIOD_MS % TICK_MS == 0);
const _: () = assert!(PEER_LIVENESS_TIMEOUT_MS % TICK_MS == 0);
const _: () = assert!(2 * (SLOW_HOP as i64) * TICK_MS < PEER_LIVENESS_TIMEOUT_MS);

/// How the subject behaves.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Subject {
    /// Grants credit and reads its inbox.
    Fast,
    /// Grants credit; its application never reads its full inbox.
    SlowApplication,
    /// Grants `A` no lane-0 credit; may chatter on its application lane.
    Withholding,
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
    /// #913 R8 M2: a probe takes shared capacity, which other frames to the subject hold.
    ProbeTakesSharedCapacity,
    /// #913 R9 H1: a due probe replaces an outstanding one, keeping its deadline.
    ReissueOverOutstanding,
    /// #935: lane-0 service awaits the application.
    ApplicationOnControlLane,
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
    /// Frames in flight, with their arrival tick, toward `B` and toward `A`.
    to_b: Vec<(Frame, u8)>,
    to_a: Vec<(Frame, u8)>,
    /// `B`'s lane-0 frames awaiting service, in order.
    lane0: Vec<Frame>,
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
    /// Whether `B` grants `A` lane-0 credit.
    fn grants(&self) -> bool {
        self.subject != Subject::Withholding
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

    /// Whether `A` evicts `B` now.
    fn evicts(&self, state: &State) -> bool {
        let stalled = self.discipline == Discipline::StallIsVerdict
            && self.subject != Subject::Fast
            && ms(state.now) >= PEER_LIVENESS_TIMEOUT_MS;
        !state.evicted && (state.clock.expiry(ms(state.now)).is_some() || stalled)
    }

    /// Whether `B` can serve its lane-0 head now.
    fn serves(&self, state: &State) -> bool {
        !state.lane0.is_empty() && !state.blocked && !state.evicted
    }

    /// Whether some action must happen before time may pass.
    fn urgent(&self, state: &State) -> bool {
        let due = |queue: &[(Frame, u8)]| queue.iter().any(|(_, at)| *at <= state.now);
        !state.evicted
            && (due(&state.to_b)
                || due(&state.to_a)
                || self.serves(state)
                || self.probe_due(state)
                || self.evicts(state))
    }

    /// Send `frame` from `A` to `B` on lane 0, leaving after `hop` ticks if it has credit.
    fn send_to_b(&self, state: &mut State, frame: Frame, hop: u8) {
        if self.grants() {
            state.to_b.push((frame, state.now + hop));
            if let (Discipline::ClockFromDeparture, Frame::Probe(n)) = (self.discipline, frame) {
                state.unstarted.retain(|unstarted| *unstarted != n);
                state.clock.issue(identity(n), ms(state.now));
            }
        } else {
            state.held.push(frame);
        }
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
                state.clock.outstanding = Some((identity(n), first));
                state.clock.last_issued_ms = now;
            }
            _ => {
                state.clock.issue(identity(n), now);
            }
        }
        let refused =
            self.discipline == Discipline::ProbeTakesSharedCapacity && !state.held.is_empty();
        if refused {
            state.clock.withdraw(identity(n));
        } else {
            self.send_to_b(state, Frame::Probe(n), hop);
        }
    }

    /// `B` serves its lane-0 head.
    fn serve(&self, state: &mut State, hop: u8) {
        let head = state.lane0.remove(0);
        let waits_on_application = self.discipline == Discipline::ApplicationOnControlLane
            && self.subject == Subject::SlowApplication;
        if waits_on_application {
            state.lane0.insert(0, head);
            state.blocked = true;
            return;
        }
        match head {
            Frame::Request => {
                if self.discipline == Discipline::HandlerWaitsOnRemote && self.third_withholds {
                    state.blocked = true;
                }
            }
            Frame::Probe(n) => state.to_a.push((Frame::Answer(n), state.now + hop)),
            Frame::Answer(_) | Frame::Chatter => {}
        }
    }

    /// `A` receives `frame`.
    fn receive(&self, state: &mut State, frame: Frame) {
        match frame {
            Frame::Answer(n) => {
                if state.clock.answer(identity(n)) {
                    state.answered += 1;
                }
            }
            Frame::Chatter if self.discipline == Discipline::InboundClears => {
                if let Some((outstanding, _)) = state.clock.outstanding {
                    state.clock.answer(outstanding);
                }
            }
            Frame::Request | Frame::Probe(_) | Frame::Chatter => {}
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
            to_b: Vec::new(),
            to_a: Vec::new(),
            lane0: Vec::new(),
            blocked: false,
            chatter_left: if self.subject == Subject::Withholding {
                CHATTER
            } else {
                0
            },
            answered: 0,
            evicted: false,
        };
        self.send_to_b(&mut state, Frame::Request, 0);
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
        actions.extend(arrived(&state.to_b).into_iter().map(Action::ArriveAtB));
        actions.extend(arrived(&state.to_a).into_iter().map(Action::ArriveAtA));
        for hop in [0, SLOW_HOP] {
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
    }

    fn next_state(&self, last: &State, action: Action) -> Option<State> {
        let mut state = last.clone();
        match action {
            Action::Tick => state.now += 1,
            Action::ArriveAtB(index) => {
                let (frame, _) = state.to_b.remove(index);
                state.lane0.push(frame);
            }
            Action::ArriveAtA(index) => {
                let (frame, _) = state.to_a.remove(index);
                self.receive(&mut state, frame);
            }
            Action::Serve(hop) => self.serve(&mut state, hop),
            Action::Issue(hop) => self.issue(&mut state, hop),
            Action::Evict => state.evicted = true,
            Action::Chatter(hop) => {
                state.chatter_left -= 1;
                state.to_a.push((Frame::Chatter, state.now + hop));
            }
        }
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always("an honest subject is never evicted", |model, state| {
                model.subject == Subject::Withholding || !state.evicted
            }),
            Property::<Self>::eventually("a withholding subject is evicted", |model, state| {
                model.subject != Subject::Withholding || state.evicted
            }),
            Property::<Self>::sometimes("a probe is answered", |model, state| {
                model.subject == Subject::Withholding || state.answered > 0
            }),
            Property::<Self>::sometimes(
                "a withholding subject's eviction is reachable",
                |model, state| model.subject != Subject::Withholding || state.evicted,
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
/// withholds credit.
#[test]
fn test_production_liveness_satisfies_the_composed_laws() {
    for subject in [
        Subject::Fast,
        Subject::SlowApplication,
        Subject::Withholding,
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

/// Every design #913 R5–R9 and #935 refuted is refuted by the property it breaks.
#[test]
fn test_each_refuted_design_is_refuted_by_the_composed_model() {
    let never = "an honest subject is never evicted";
    let eventually = "a withholding subject is evicted";
    for (discipline, subject, third_withholds, law) in [
        (
            Discipline::StallIsVerdict,
            Subject::SlowApplication,
            false,
            never,
        ),
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
            Subject::SlowApplication,
            false,
            never,
        ),
    ] {
        assert!(
            refuted(discipline, subject, third_withholds).contains(law),
            "{discipline:?} with {subject:?} must be refuted by `{law}`"
        );
    }
}
