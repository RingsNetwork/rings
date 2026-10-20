//! Law tests of session liveness (#975; D2′ Liveness; paper, Prop. Liveness of failure): the
//! exit's echo, the client's deadline, and the Proposition's three cases on the two machines
//! joined by a modelled route.
//!
//! Every instant is an argument of a pure machine, so the clock is the test's own.

use bytes::Bytes;

use super::acked;
use super::arguments;
use super::kinds;
use super::opening;
use super::surbs;
use super::NOW_MS;
use super::QUANTUM_MS;
use super::TARGET;
use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::onion::session::client::OnionClientFailure;
use crate::onion::session::client::OnionClientSession;
use crate::onion::session::client::ONION_SESSION_DEADLINE_MS;
use crate::onion::session::exit::OnionExitEffect;
use crate::onion::session::exit::OnionExitSession;
use crate::onion::session::exit::OnionWorldRead;
use crate::onion::session::exit::ONION_SESSION_ECHO_HORIZON_MS;
use crate::onion::session::frame::OnionFrame;
use crate::onion::session::frame::OnionSequence;
use crate::onion::sphinx::cell::OnionSurb;

/// `V`, the forward validity.
const V: u128 = ONION_FORWARD_MAX_VALIDITY_MS;

/// One reply block admissible at `at`: its expiry `(3 + ⌊at / Q⌋)·Q` lies in `(at, at + V]`.
fn block_at(seed: u64, at_ms: u128) -> OnionSurb {
    surbs(seed, 1, at_ms / QUANTUM_MS).pop().expect("one block")
}

/// A credit-only forward frame with no further blocks: the keep-alive's shape, for the tests.
fn bare_credit() -> OnionFrame {
    OnionFrame::Credit(Vec::new())
}

/// The echo `data(n, 0, ε)` of `h` at sequence `n`.
fn echo(sequence: u32) -> OnionFrame {
    OnionFrame::Data {
        sequence: OnionSequence::new(sequence),
        target: None,
        payload: Bytes::new(),
    }
}

/// A client session opened at `NOW`: its ack has arrived.
fn opened_client() -> OnionClientSession {
    let mut client = OnionClientSession::new(Bytes::from_static(TARGET), NOW_MS);
    assert!(client.reply(NOW_MS, echo(0)).is_ok());
    client
}

// ---- exit: the echo ---------------------------------------------------------------------------

/// Law Echo: an arrival is echoed exactly when no reply has left for `V/2`, each echo is
/// `data(n, 0, ε)` at the next reply sequence, a burst right after a reply is not echoed, and a
/// world reply restarts the horizon as an echo does.
#[test]
fn test_exit_echoes_an_arrival_after_half_a_window_without_a_reply() {
    let mut blocks = surbs(70, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    let horizon = NOW_MS + ONION_SESSION_ECHO_HORIZON_MS;

    let early = horizon - 1;
    assert!(kinds(&session.forward(early, bare_credit(), block_at(71, early))).is_empty());
    assert_eq!(
        kinds(&session.forward(horizon, bare_credit(), block_at(72, horizon))),
        ["data 1 0"]
    );
    for (offset, seed) in [(1, 73), (2, 74)] {
        let burst = horizon + offset;
        assert!(
            kinds(&session.forward(burst, bare_credit(), block_at(seed, burst))).is_empty(),
            "a burst after a reply is not echoed"
        );
    }

    let replied = horizon + 10;
    let world = OnionWorldRead::Bytes(Bytes::from_static(b"x"));
    assert_eq!(kinds(&session.world(replied, world)), ["data 2 1"]);
    let quiet = replied + ONION_SESSION_ECHO_HORIZON_MS;
    assert!(kinds(&session.forward(quiet - 1, bare_credit(), block_at(75, quiet))).is_empty());
    assert_eq!(
        kinds(&session.forward(quiet, bare_credit(), block_at(76, quiet))),
        ["data 3 0"]
    );
}

/// Law Echo, credit neutrality: an echoed arrival leaves `|Q_{h,ς}|` as it found it plus the
/// blocks its frame carried, since the echo spends exactly the block the loop brought.
#[test]
fn test_exit_echo_is_credit_neutral() {
    let mut blocks = surbs(80, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    // Past the horizon the ack's cohort has expired, so the pool is empty.
    let at = NOW_MS + V;
    let carried = surbs(81, 3, at / QUANTUM_MS);
    let count = carried.len();
    assert_eq!(
        kinds(&session.forward(at, OnionFrame::Credit(carried), block_at(82, at))),
        ["data 1 0"]
    );
    let wide = vec![0x5a; OnionFrame::data_capacity(super::CLASS) * (count + 2)];
    let replies = session.world(at, OnionWorldRead::Bytes(Bytes::from(wide)));
    assert_eq!(
        replies
            .iter()
            .filter(|effect| matches!(effect, OnionExitEffect::Reply { .. }))
            .count(),
        count,
        "the pool held exactly the carried blocks"
    );
}

/// No echo precedes the open's decision: an unbound or opening session answers nothing however
/// late its loops arrive, the ack is still `data(0, 0, ε)`, and a refusal still `abort(0)`.
#[test]
fn test_exit_does_not_echo_before_the_open_is_decided() {
    let late = NOW_MS + ONION_SESSION_ECHO_HORIZON_MS;

    let mut unbound = OnionExitSession::new(arguments().digest, NOW_MS);
    assert!(kinds(&unbound.forward(NOW_MS, bare_credit(), block_at(90, NOW_MS))).is_empty());
    assert!(kinds(&unbound.forward(late, bare_credit(), block_at(91, late))).is_empty());

    for (opened, answer, seed) in [
        (true, ["data 0 0"].as_slice(), 92),
        (false, &["abort 0", "close"], 94),
    ] {
        let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
        assert_eq!(
            kinds(&session.forward(NOW_MS, opening(0, b""), block_at(seed, NOW_MS))),
            ["open example.com:443"]
        );
        assert!(
            kinds(&session.forward(late, bare_credit(), block_at(seed + 1, late))).is_empty(),
            "no echo while the world is opening"
        );
        assert_eq!(kinds(&session.opened(late, opened)), answer);
    }
}

/// No echo follows the world's `fin`: the reply direction is over, and a frame after it would
/// read as stream bytes after the end.
#[test]
fn test_exit_does_not_echo_after_the_worlds_fin() {
    let mut blocks = surbs(100, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    assert_eq!(kinds(&session.world(NOW_MS, OnionWorldRead::Eof)), [
        "fin 1"
    ]);
    let late = NOW_MS + ONION_SESSION_ECHO_HORIZON_MS;
    assert!(kinds(&session.forward(late, bare_credit(), block_at(101, late))).is_empty());
    assert!(!session.is_closed(), "the client's direction is still open");
}

// ---- client: the deadline ---------------------------------------------------------------------

/// Law Deadline: frames `7V/4` apart, wider than the live bound `3V/2`, never fail the session,
/// and each echo is heard without an event.
#[test]
fn test_client_never_fails_frames_seven_quarter_windows_apart() {
    let mut client = opened_client();
    let spacing = 7 * V / 4;
    for sequence in 1..8_u32 {
        let at = NOW_MS + u128::from(sequence) * spacing;
        assert_eq!(client.expire(at - 1), Ok(()));
        assert_eq!(client.reply(at, echo(sequence)), Ok(Vec::new()));
        assert_eq!(client.deadline_ms(), Some(at + ONION_SESSION_DEADLINE_MS));
    }
}

/// Law Deadline: a reply direction silent for `Δ = 2V` fails the session, whose shell then
/// gives it up with `abort(n)` at its next forward sequence.
#[test]
fn test_client_fails_a_reply_direction_silent_for_two_windows() {
    let mut client = opened_client();
    assert_eq!(ONION_SESSION_DEADLINE_MS, 2 * V);
    assert!(client.data(Bytes::from_static(b"up")).is_ok());
    assert_eq!(client.expire(NOW_MS + 2 * V - 1), Ok(()));
    assert_eq!(
        client.expire(NOW_MS + 2 * V),
        Err(OnionClientFailure::Silent)
    );
    assert!(matches!(
        client.abort(),
        Ok(OnionFrame::Abort { sequence }) if sequence.value() == 1
    ));
}

/// Law Deadline: an empty `data` after the open, `h`'s echo, yields no event but moves the
/// deadline, so a session whose only frames are echoes stays open.
#[test]
fn test_client_hears_an_echo_without_an_event() {
    let mut client = opened_client();
    let at = NOW_MS + V;
    assert_eq!(client.reply(at, echo(1)), Ok(Vec::new()));
    assert_eq!(client.expire(NOW_MS + 2 * V), Ok(()), "the echo was heard");
    assert_eq!(client.expire(at + 2 * V), Err(OnionClientFailure::Silent));
}

/// Law Deadline: the deadline ends with the reply direction, since no frame follows the world's
/// `fin` or `h`'s `abort`.
#[test]
fn test_client_deadline_ends_with_the_reply_direction() {
    let fin = OnionFrame::Fin {
        sequence: OnionSequence::new(1),
        target: None,
    };
    let abort = OnionFrame::Abort {
        sequence: OnionSequence::new(1),
    };
    for end in [fin, abort] {
        let mut client = opened_client();
        assert!(client.reply(NOW_MS, end).is_ok());
        assert_eq!(client.deadline_ms(), None);
        assert_eq!(client.expire(NOW_MS + 10 * V), Ok(()));
    }
}

// ---- the Proposition, on a modelled route ------------------------------------------------------

/// The least latency of a modelled segment crossing.
const BASE_MS: u128 = 2_000;

/// The longest return-segment latency `L` of the model.
const L_MS: u128 = BASE_MS + V / 4;

/// The latency of a segment's `index`-th crossing, `BASE + (7919·index mod V/4)`: it varies by
/// less than `V/4`, the Proposition's premise.
fn latency(index: u128) -> u128 {
    BASE_MS + (index * 7_919) % (V / 4)
}

/// Where the modelled route stops delivering: from `t₀` on, no crossing of that segment arrives.
#[derive(Clone, Copy, Debug)]
enum OnionRouteCut {
    /// Both segments deliver.
    Live,
    /// The forward segment stops at `t₀`.
    Forward(u128),
    /// The return segment stops at `t₀`.
    Return(u128),
}

/// An event of the model.
enum OnionModelEvent {
    /// The client's keep-alive is due.
    KeepAlive,
    /// A forward loop reaches `h` with its block.
    Arrive(OnionFrame, Box<OnionSurb>),
    /// A reply reaches the client.
    Reply(OnionFrame),
    /// `h`'s `V`-without-a-forward-loop rule may fire.
    Tick,
}

/// What each endpoint observed, and when.
#[derive(Debug, Default)]
struct OnionModelOutcome {
    /// When the client failed `ς`.
    client_failed_ms: Option<u128>,
    /// When `h` dropped `ς`.
    exit_closed_ms: Option<u128>,
    /// When the client's `abort` reached `h`.
    abort_arrived_ms: Option<u128>,
    /// The widest gap between consecutive frames heard by the client.
    widest_silence_ms: u128,
    /// The echoes `h` sent.
    echoes: usize,
}

/// The client and `h` joined by a modelled route: the client sends its open, then its
/// keep-alive every `V/2`; `h` opens at once; each crossing takes [`latency`].
struct OnionLivenessModel {
    /// Where the route stops delivering.
    cut: OnionRouteCut,
    /// Pending events, with their instants and their order of scheduling.
    events: Vec<(u128, u64, OnionModelEvent)>,
    /// The next scheduling order, which breaks ties between equal instants.
    order: u64,
    /// The forward crossings so far.
    forwards: u128,
    /// The return crossings so far.
    returns: u128,
    /// The client's machine.
    client: OnionClientSession,
    /// `h`'s machine.
    exit: OnionExitSession,
    /// The last frame the client heard.
    heard_ms: u128,
    /// The observations.
    outcome: OnionModelOutcome,
}

impl OnionLivenessModel {
    /// Run the model under `cut` until `horizon`.
    fn run(cut: OnionRouteCut, horizon_ms: u128) -> OnionModelOutcome {
        let mut model = Self {
            cut,
            events: Vec::new(),
            order: 0,
            forwards: 0,
            returns: 0,
            client: OnionClientSession::new(Bytes::from_static(TARGET), NOW_MS),
            exit: OnionExitSession::new(arguments().digest, NOW_MS),
            heard_ms: NOW_MS,
            outcome: OnionModelOutcome::default(),
        };
        let open = model.client.data(Bytes::new()).expect("a sequence");
        model.forward(NOW_MS, open);
        model.schedule(NOW_MS + V / 2, OnionModelEvent::KeepAlive);
        while let Some(at) = model.next_instant() {
            if at > horizon_ms {
                break;
            }
            model.step(at);
        }
        model.outcome
    }

    /// The instant of the next step: the next event, or the client's deadline if it is due first
    /// (the driver wakes at it).
    fn next_instant(&self) -> Option<u128> {
        let event = self.events.iter().map(|(at, _, _)| *at).min();
        let deadline = self
            .outcome
            .client_failed_ms
            .is_none()
            .then(|| self.client.deadline_ms())
            .flatten();
        event.into_iter().chain(deadline).min()
    }

    /// Fail the client at its deadline `at`, or take the next event at `at`.
    fn step(&mut self, at: u128) {
        if self.outcome.client_failed_ms.is_none() && self.client.deadline_ms() == Some(at) {
            assert_eq!(self.client.expire(at - 1), Ok(()));
            assert_eq!(self.client.expire(at), Err(OnionClientFailure::Silent));
            self.outcome.client_failed_ms = Some(at);
            let abort = self.client.abort().expect("a sequence");
            self.forward(at, abort);
            return;
        }
        let index = self
            .events
            .iter()
            .enumerate()
            .min_by_key(|(_, (at, order, _))| (*at, *order))
            .map(|(index, _)| index)
            .expect("an event at the next instant");
        let (at, _, event) = self.events.remove(index);
        match event {
            OnionModelEvent::KeepAlive if self.outcome.client_failed_ms.is_none() => {
                self.forward(at, OnionFrame::Credit(Vec::new()));
                self.schedule(at + V / 2, OnionModelEvent::KeepAlive);
            }
            OnionModelEvent::KeepAlive => {}
            OnionModelEvent::Arrive(frame, surb) => {
                if matches!(frame, OnionFrame::Abort { .. }) {
                    self.outcome.abort_arrived_ms = Some(at);
                }
                let effects = self.exit.forward(at, frame, *surb);
                self.perform(at, effects);
                self.schedule(at + V, OnionModelEvent::Tick);
            }
            OnionModelEvent::Reply(frame) if self.outcome.client_failed_ms.is_none() => {
                self.outcome.widest_silence_ms =
                    self.outcome.widest_silence_ms.max(at - self.heard_ms);
                self.heard_ms = at;
                assert!(self.client.reply(at, frame).is_ok(), "no gap in the model");
            }
            OnionModelEvent::Reply(_) => {}
            OnionModelEvent::Tick => {
                let effects = self.exit.tick(at);
                self.perform(at, effects);
            }
        }
    }

    /// Queue `event` at `at`.
    fn schedule(&mut self, at: u128, event: OnionModelEvent) {
        self.events.push((at, self.order, event));
        self.order += 1;
    }

    /// Send `frame` toward `h` at `at` in a loop with a fresh block, unless the cut loses it.
    fn forward(&mut self, at: u128, frame: OnionFrame) {
        let arrival = at + latency(self.forwards);
        self.forwards += 1;
        if matches!(self.cut, OnionRouteCut::Forward(cut) if arrival >= cut) {
            return;
        }
        let seed = u64::try_from(self.forwards).expect("few loops") + 1_000;
        let surb = Box::new(block_at(seed, arrival));
        self.schedule(arrival, OnionModelEvent::Arrive(frame, surb));
    }

    /// Perform `h`'s effects at `at`: replies cross the return segment, an open succeeds at
    /// once, and a close is observed.
    fn perform(&mut self, at: u128, effects: Vec<OnionExitEffect>) {
        for effect in effects {
            match effect {
                OnionExitEffect::Reply { frame, .. } => {
                    if matches!(&frame, OnionFrame::Data { sequence, payload, .. }
                        if payload.is_empty() && sequence.value() > 0)
                    {
                        self.outcome.echoes += 1;
                    }
                    let arrival = at + latency(self.returns);
                    self.returns += 1;
                    if !matches!(self.cut, OnionRouteCut::Return(cut) if arrival >= cut) {
                        self.schedule(arrival, OnionModelEvent::Reply(frame));
                    }
                }
                OnionExitEffect::Open { .. } => {
                    let opened = self.exit.opened(at, true);
                    self.perform(at, opened);
                }
                OnionExitEffect::Close => {
                    self.outcome.exit_closed_ms.get_or_insert(at);
                }
                OnionExitEffect::Write(_) | OnionExitEffect::ShutdownWrite => {}
            }
        }
    }
}

/// Prop. Liveness of failure (i): while both segments deliver, an idle session is held open by
/// echoes alone, the client hears a frame at least every `3V/2 < Δ`, and neither endpoint
/// fails it.
#[test]
fn test_liveness_keeps_an_idle_live_session_open() {
    let outcome = OnionLivenessModel::run(OnionRouteCut::Live, NOW_MS + 12 * V);
    assert_eq!(outcome.client_failed_ms, None);
    assert_eq!(outcome.exit_closed_ms, None);
    assert!(outcome.echoes >= 12, "{outcome:?}");
    assert!(outcome.widest_silence_ms < 3 * V / 2, "{outcome:?}");
}

/// Prop. Liveness of failure (ii), return loss at `t₀`: the client fails `ς` by `t₀ + 2V + L`,
/// and `h`, which still receives loops, drops it exactly when the client's `abort` arrives.
#[test]
fn test_liveness_observes_a_return_loss_at_both_endpoints() {
    let cut = NOW_MS + 3 * V + 7;
    let outcome = OnionLivenessModel::run(OnionRouteCut::Return(cut), NOW_MS + 12 * V);
    let failed = outcome.client_failed_ms.expect("the client fails ς");
    assert!(failed <= cut + 2 * V + L_MS, "{outcome:?}");
    assert!(outcome.abort_arrived_ms > Some(failed), "{outcome:?}");
    assert_eq!(
        outcome.exit_closed_ms, outcome.abort_arrived_ms,
        "{outcome:?}"
    );
}

/// Prop. Liveness of failure (ii), forward loss at `t₀`: `h` drops `ς` by `t₀ + V`, and the
/// client, whose `abort` is lost with the segment, fails it by `t₀ + 3V + L`.
#[test]
fn test_liveness_observes_a_forward_loss_at_both_endpoints() {
    let cut = NOW_MS + 3 * V + 7;
    let outcome = OnionLivenessModel::run(OnionRouteCut::Forward(cut), NOW_MS + 12 * V);
    let closed = outcome.exit_closed_ms.expect("h drops ς");
    assert!(closed <= cut + V, "{outcome:?}");
    let failed = outcome.client_failed_ms.expect("the client fails ς");
    assert!(failed <= cut + 3 * V + L_MS, "{outcome:?}");
    assert_eq!(outcome.abort_arrived_ms, None, "{outcome:?}");
}
