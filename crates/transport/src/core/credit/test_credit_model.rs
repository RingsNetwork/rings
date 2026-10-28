//! Model check of the credit laws over every interleaving of one lane.
//!
//! The state is one lane of one connection generation and its share of the node's pool, closed
//! under the production transitions of [`SendCredit`] and [`ReceiveWindow`]:
//!
//! ```text
//!   LaneState = SendCredit × ℕ (frames still to send) × ℕ (data frames in flight)
//!             × ReceiveWindow × 𝒫(Signal) (credits and shrinks in flight to the sender)
//!             × 𝒫(Signal) (returns in flight to the receiver)
//!             × 𝒫(Signal) (signals whose send failed, queued again) × 𝔹 (violation seen)
//! ```
//!
//! Data frames travel on the lane's ordered channel, so a count of frames in flight is their
//! whole state. Signals travel as datagrams: an in-flight set is delivered in any order, and in
//! the duplicating model a delivery may leave its signal in flight to be delivered again. A send
//! may be abandoned before it is irrevocable (`Cancel`), and the receiver releases admitted
//! frames at any time, advertising at once or deferring the advertisement to any later step. A
//! pooled lane borrows from the pool whenever it wants and the pool has room (`Lend`), and is
//! shrunk by its idle check whenever it holds a share (`IdleCheck`), or by one frame when the
//! pool rebalances (`Rebalance`), each of which the sender answers with a return. A signal's
//! send may fail, as it does while an outbound channel is closed; the pump queues the signal
//! again and sends it later.
//!
//! Properties, checked for several windows `(W, b)`:
//!
//! - `Always` no violation: an honest sender never exceeds the advertised credit;
//! - `Always` bound: `received − released ≤ W`, for an honest sender and for one that floods
//!   frames without credit alike, since the receiver refuses every frame beyond its credit;
//! - `Always` cap and pool: the counted liability `advertised − released` never exceeds `W`
//!   (the per-lane cap that keeps the rate floor's grace sound, load-bounds paper, Prop. 4), and
//!   the lane's charge never exceeds the pool's capacity (Law 3);
//! - `Sometimes` refusal: a flooding sender is refused, so the bound is enforced rather than
//!   merely never tested;
//! - `Always` no deadlock: for an honest sender, unless all work is done, some transition that
//!   makes progress is enabled (a stale credit, which grants nothing, does not count);
//! - `Eventually` done, in the model that delivers each credit once: every path ends with
//!   every frame sent, received and released.
//!
//! Each law is load-bearing: a broken algebra, applied on the test side of the production
//! transitions, is refuted by the property that states it (`test_each_credit_mutant_is_refuted`):
//! a receiver that over-advertises, a sender that ignores its limit, a pump that loses a credit
//! whose send failed, a sender that applies a credit of an older
//! generation after a shrink, and a sender that returns the shrink's limit without what it
//! already committed.
//!
//! Isolation is structural rather than checked: the production state of a connection is the
//! product of independent lanes, and no lane transition reads another lane.

use std::collections::BTreeSet;

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::CreditWindow;
use super::LaneSignal;
use super::ReceiveWindow;
use super::SendCredit;

/// The shrinks a model path makes at most: one, so a credit of the older generation crosses
/// it, which is what the generation guards, while the state space stays finite.
const MAX_SHRINKS: u64 = 1;

/// One lane of one connection generation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LaneState {
    /// The sending end.
    sender: SendCredit,
    /// Frames the sender has yet to send.
    to_send: u64,
    /// Data frames sent and not yet arrived, in channel order.
    in_flight: u64,
    /// The receiving end.
    receiver: ReceiveWindow,
    /// Credits and shrinks sent to the sender and not yet delivered.
    credits: BTreeSet<LaneSignal>,
    /// Returns sent to the receiver and not yet delivered.
    returns: BTreeSet<LaneSignal>,
    /// Credits and shrinks whose send failed, queued again by the pump for a later send.
    failed: BTreeSet<LaneSignal>,
    /// Whether an arrival ever exceeded the advertised credit.
    violated: bool,
}

/// One transition of [`LaneState`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum LaneAction {
    /// The sender reserves a credit for its next frame.
    Reserve,
    /// A reserved send becomes irrevocable and its frame enters the channel.
    Commit,
    /// A reserved send is abandoned before it is irrevocable.
    Cancel,
    /// The next data frame arrives and is admitted.
    Arrive,
    /// The receiver releases one admitted frame and advertises what that completes.
    Release,
    /// The receiver releases one admitted frame and defers its advertisement (the node is under
    /// load).
    ReleaseDeferred,
    /// The receiver makes a deferred advertisement (the node's load fell).
    Advertise,
    /// The send of a credit or shrink fails; the pump queues it again.
    FailCredit(LaneSignal),
    /// The pump sends a signal whose earlier send failed.
    ResendCredit(LaneSignal),
    /// The pool lends the lane what it wants, as far as it has room.
    Lend,
    /// The receiver's idle check.
    IdleCheck,
    /// The pool rebalances: the lane's share is shrunk by one, short of the floor.
    Rebalance,
    /// A return is delivered to the receiver, and kept in flight when `duplicate`.
    Return {
        /// The return delivered.
        signal: LaneSignal,
        /// Whether it stays in flight to be delivered again.
        duplicate: bool,
    },
    /// A dishonest sender puts a frame on the channel without a credit.
    Flood,
    /// A credit or shrink is delivered to the sender, and kept in flight when `duplicate`.
    Grant {
        /// The signal delivered.
        signal: LaneSignal,
        /// Whether it stays in flight to be delivered again.
        duplicate: bool,
    },
}

/// The lane model: a window, a workload, and whether credits may be delivered more than once.
struct LaneModel {
    /// The credit window under check.
    window: CreditWindow,
    /// The pool's capacity, which a pooled window borrows from.
    pool: u64,
    /// Frames the sender sends in all.
    frames: u64,
    /// Whether a delivered credit may stay in flight.
    duplicating: bool,
    /// Whether the sender respects credit; a dishonest one may also flood.
    honest: bool,
    /// A deliberately broken algebra, which a property must refute.
    mutant: Option<Mutant>,
}

/// A broken credit algebra, applied on the test side of the production transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mutant {
    /// The receiver advertises one frame more than it released plus its window.
    OverAdvertise,
    /// The sender reserves whatever its limit says.
    IgnoreLimit,
    /// A credit whose send failed is lost, not queued again.
    DropFailedCredit,
    /// The sender applies a credit of an older generation.
    IgnoreGeneration,
    /// The sender returns the shrink's limit, not what it already committed.
    ReturnLimit,
}

impl LaneModel {
    /// Whether every frame was sent, received and released, with no send pending.
    fn done(state: &LaneState) -> bool {
        state.to_send == 0
            && state.in_flight == 0
            && state.receiver.occupancy() == 0
            && state.sender.reserved == 0
    }

    /// Whether a deferred advertisement is due: advertising now would raise the credit.
    fn advertisement_due(&self, state: &LaneState) -> bool {
        let mut receiver = state.receiver;
        receiver.advertise(self.window).is_some()
    }

    /// Whether the pool would lend the lane something now.
    fn lend_due(&self, state: &LaneState) -> bool {
        state.receiver.want(self.window) > 0 && state.receiver.charge(self.window) < self.pool
    }

    /// Whether `signal`, delivered to the sender, would change it.
    fn changes_sender(&self, state: &LaneState, signal: LaneSignal) -> bool {
        let mut next = state.clone();
        self.deliver_to_sender(&mut next, signal);
        next != *state
    }

    /// Deliver `signal` to the sender: a credit is granted, a shrink answered with a return.
    fn deliver_to_sender(&self, state: &mut LaneState, signal: LaneSignal) {
        match signal {
            LaneSignal::Credit { limit, generation } => {
                if self.mutant == Some(Mutant::IgnoreGeneration) {
                    state.sender.limit = state.sender.limit.max(limit);
                } else {
                    state.sender.grant(limit, generation);
                }
            }
            LaneSignal::Shrink { limit, generation } => {
                let bound = match self.mutant {
                    Some(Mutant::ReturnLimit) if generation > state.sender.generation => {
                        state.sender.shrink(limit, generation);
                        state.sender.limit = limit;
                        state.sender.returned = Some(limit);
                        Some(limit)
                    }
                    _ => state.sender.shrink(limit, generation),
                };
                if let Some(bound) = bound {
                    state
                        .returns
                        .insert(LaneSignal::Returned { bound, generation });
                }
            }
            LaneSignal::Returned { .. } => {}
        }
    }

    /// Whether a transition that makes progress is enabled: a reservation, a pending send, an
    /// arrival, a release, a due advertisement, or a credit above the sender's limit.
    fn progress_enabled(&self, state: &LaneState) -> bool {
        let mut sender = state.sender;
        let reservable = state.to_send > state.sender.reserved && sender.try_reserve();
        reservable
            || state.sender.reserved > 0
            || state.in_flight > 0
            || state.receiver.occupancy() > 0
            || self.advertisement_due(state)
            || !state.failed.is_empty()
            || self.lend_due(state)
            || state
                .credits
                .iter()
                .any(|signal| self.changes_sender(state, *signal))
            || state.returns.iter().any(|signal| {
                let LaneSignal::Returned { bound, generation } = *signal else {
                    return false;
                };
                state.receiver.clone().returned(bound, generation)
            })
    }
}

impl Model for LaneModel {
    type Action = LaneAction;
    type State = LaneState;

    fn init_states(&self) -> Vec<Self::State> {
        vec![LaneState {
            sender: SendCredit::new(self.window),
            to_send: self.frames,
            in_flight: 0,
            receiver: ReceiveWindow::new(self.window),
            credits: BTreeSet::new(),
            returns: BTreeSet::new(),
            failed: BTreeSet::new(),
            violated: false,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.to_send > state.sender.reserved {
            actions.push(LaneAction::Reserve);
        }
        if state.sender.reserved > 0 {
            actions.push(LaneAction::Commit);
            actions.push(LaneAction::Cancel);
        }
        if !self.honest && state.to_send > 0 {
            actions.push(LaneAction::Flood);
        }
        if state.in_flight > 0 {
            actions.push(LaneAction::Arrive);
        }
        if state.receiver.occupancy() > 0 {
            actions.push(LaneAction::Release);
            actions.push(LaneAction::ReleaseDeferred);
        }
        if self.advertisement_due(state) {
            actions.push(LaneAction::Advertise);
        }
        if self.lend_due(state) {
            actions.push(LaneAction::Lend);
        }
        if state.receiver.charge(self.window) > 0 && state.receiver.generation < MAX_SHRINKS {
            actions.push(LaneAction::IdleCheck);
        }
        if state.receiver.lent() > 1 && state.receiver.generation < MAX_SHRINKS {
            actions.push(LaneAction::Rebalance);
        }
        actions.extend(state.failed.iter().copied().map(LaneAction::ResendCredit));
        for signal in state.credits.iter().copied() {
            actions.push(LaneAction::FailCredit(signal));
            actions.push(LaneAction::Grant {
                signal,
                duplicate: false,
            });
            if self.duplicating {
                actions.push(LaneAction::Grant {
                    signal,
                    duplicate: true,
                });
            }
        }
        for signal in state.returns.iter().copied() {
            actions.push(LaneAction::Return {
                signal,
                duplicate: false,
            });
            if self.duplicating {
                actions.push(LaneAction::Return {
                    signal,
                    duplicate: true,
                });
            }
        }
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = last_state.clone();
        match action {
            LaneAction::Reserve => {
                if self.mutant == Some(Mutant::IgnoreLimit) {
                    state.sender.reserved = state.sender.reserved.saturating_add(1);
                } else if !state.sender.try_reserve() {
                    return None;
                }
            }
            LaneAction::Commit => {
                state.sender.commit();
                state.to_send = state.to_send.checked_sub(1)?;
                state.in_flight = state.in_flight.saturating_add(1);
            }
            LaneAction::Cancel => state.sender.cancel(),
            LaneAction::Flood => {
                state.to_send = state.to_send.checked_sub(1)?;
                state.in_flight = state.in_flight.saturating_add(1);
            }
            LaneAction::Arrive => {
                state.in_flight = state.in_flight.checked_sub(1)?;
                if state.receiver.admit().is_err() {
                    state.violated = true;
                }
            }
            LaneAction::ReleaseDeferred => state.receiver.release(),
            LaneAction::Release | LaneAction::Advertise => {
                if action == LaneAction::Release {
                    state.receiver.release();
                }
                if let Some((limit, generation)) = state.receiver.advertise(self.window) {
                    let limit = match self.mutant {
                        Some(Mutant::OverAdvertise) => {
                            state.receiver.advertised = limit.saturating_add(1);
                            limit.saturating_add(1)
                        }
                        _ => limit,
                    };
                    state
                        .credits
                        .insert(LaneSignal::Credit { limit, generation });
                }
            }
            LaneAction::FailCredit(signal) => {
                state.credits.remove(&signal);
                if self.mutant != Some(Mutant::DropFailedCredit) {
                    state.failed.insert(signal);
                }
            }
            LaneAction::ResendCredit(signal) => {
                state.failed.remove(&signal);
                state.credits.insert(signal);
            }
            LaneAction::Lend => {
                let free = self.pool.saturating_sub(state.receiver.charge(self.window));
                state.receiver.lend(self.window, free);
            }
            LaneAction::IdleCheck => {
                if let Some(shrink) = state.receiver.idle_shrink(self.window) {
                    state.credits.insert(shrink);
                }
            }
            LaneAction::Rebalance => {
                let lent = state.receiver.lent().saturating_sub(1);
                if let Some(shrink) = state.receiver.shrink_to(self.window, lent) {
                    state.credits.insert(shrink);
                }
            }
            LaneAction::Return { signal, duplicate } => {
                if let LaneSignal::Returned { bound, generation } = signal {
                    state.receiver.returned(bound, generation);
                }
                if !duplicate {
                    state.returns.remove(&signal);
                }
            }
            LaneAction::Grant { signal, duplicate } => {
                self.deliver_to_sender(&mut state, signal);
                if !duplicate {
                    state.credits.remove(&signal);
                }
            }
        }
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always("no honest violation", |model, state: &LaneState| {
                !model.honest || !state.violated
            }),
            Property::sometimes("a flood is refused", |model, state: &LaneState| {
                model.honest || state.violated
            }),
            Property::always("occupancy within the window", |model, state: &LaneState| {
                state.receiver.occupancy() <= model.window.frames()
            }),
            Property::always("liability within the cap", |model, state: &LaneState| {
                state.receiver.liability() <= model.window.frames()
            }),
            Property::always("charge within the pool", |model, state: &LaneState| {
                state.receiver.charge(model.window) <= model.pool
            }),
            Property::always("no honest deadlock", |model, state: &LaneState| {
                !model.honest || Self::done(state) || model.progress_enabled(state)
            }),
            Property::eventually("all frames delivered", |model, state: &LaneState| {
                // Checked where every credit is delivered once by an honest sender; a
                // duplicated credit can be re-delivered forever, and a flood is refused.
                model.duplicating || !model.honest || Self::done(state)
            }),
        ]
    }
}

/// The model of `window` with a workload of two windows and one frame, so every path crosses
/// at least two credit rounds.
fn lane_model(window: CreditWindow, duplicating: bool, honest: bool) -> LaneModel {
    unpooled_model(window, 0, duplicating, honest)
}

/// The model of `window` borrowing from a pool of `pool` frames, with a workload of one window
/// and one frame, so every path crosses a credit round, and a shrink and its return may fall
/// anywhere in it.
fn pooled_model(window: CreditWindow, pool: u64, duplicating: bool, honest: bool) -> LaneModel {
    LaneModel {
        frames: window.frames().saturating_add(1),
        ..unpooled_model(window, pool, duplicating, honest)
    }
}

/// The model of `window` with the given pool and the workload of [`lane_model`].
fn unpooled_model(window: CreditWindow, pool: u64, duplicating: bool, honest: bool) -> LaneModel {
    LaneModel {
        window,
        pool,
        frames: window.frames().saturating_mul(2).saturating_add(1),
        duplicating,
        honest,
        mutant: None,
    }
}

/// The windows under check: the smallest, unbatched and half-batched, and a whole-window
/// batch, which advertises only once the window is entirely released.
fn windows() -> [CreditWindow; 5] {
    [
        CreditWindow::new(1, 1),
        CreditWindow::new(2, 1),
        CreditWindow::new(3, 2),
        CreditWindow::new(4, 2),
        CreditWindow::new(4, 4),
    ]
}

/// The pooled windows under check, with their pools: a floor of one frame below a cap of two
/// or three, and a pool smaller than, or as large as, what the lane may borrow.
fn pooled_windows() -> [(CreditWindow, u64); 3] {
    [
        (CreditWindow::pooled(2, 1, 1), 1),
        (CreditWindow::pooled(3, 1, 1), 1),
        (CreditWindow::pooled(3, 1, 2), 2),
    ]
}

/// Every property of the module documentation holds, for every window, in each of the three
/// models: signals reordered and duplicated, signals each delivered once, and a sender that
/// floods frames without credit; each property names the models it binds. A path that reserves
/// and cancels, or lends and shrinks, forever is a cycle, which the checker treats as fair.
#[test]
fn test_credit_laws_hold_in_every_model() {
    for (duplicating, honest) in [(true, true), (false, true), (false, false)] {
        for window in windows() {
            lane_model(window, duplicating, honest)
                .checker()
                .spawn_bfs()
                .join()
                .assert_properties();
        }
        for (window, pool) in pooled_windows() {
            pooled_model(window, pool, duplicating, honest)
                .checker()
                .spawn_bfs()
                .join()
                .assert_properties();
        }
    }
}

/// The names of the properties `model` refutes or leaves unwitnessed.
fn refuted(model: LaneModel) -> BTreeSet<&'static str> {
    let checker = model.checker().spawn_bfs().join();
    checker
        .model()
        .properties()
        .into_iter()
        .filter(|property| {
            let discovered = checker.discovery(property.name).is_some();
            match property.expectation {
                stateright::Expectation::Sometimes => !discovered,
                _ => discovered,
            }
        })
        .map(|property| property.name)
        .collect()
}

/// Each law is load-bearing: a broken algebra is refuted by the property that states it. An
/// over-advertising receiver lets occupancy exceed the window; a sender that ignores its limit
/// is refused by the receiver, a violation; a pump that drops a credit whose send failed,
/// instead of resending it, loses the last advertisement, so the honest sender deadlocks; and a
/// sender that applies a credit of an older generation, or returns a shrink's limit without the
/// frames it already committed, sends beyond what the receiver counts, a violation. A batch
/// larger than the grant, which would never advertise, is not constructible: the batch is taken
/// of the grant (`CreditWindow::batch_of`).
#[test]
fn test_each_credit_mutant_is_refuted() {
    let window = CreditWindow::new(2, 1);
    for (mutant, law) in [
        (Mutant::OverAdvertise, "occupancy within the window"),
        (Mutant::IgnoreLimit, "no honest violation"),
        (Mutant::DropFailedCredit, "no honest deadlock"),
    ] {
        let model = LaneModel {
            mutant: Some(mutant),
            ..lane_model(window, false, true)
        };
        assert!(
            refuted(model).contains(law),
            "{mutant:?} must be refuted by `{law}`"
        );
    }
    let (pooled, pool) = (CreditWindow::pooled(2, 1, 1), 1);
    for mutant in [Mutant::IgnoreGeneration, Mutant::ReturnLimit] {
        let model = LaneModel {
            mutant: Some(mutant),
            ..pooled_model(pooled, pool, true, true)
        };
        assert!(
            refuted(model).contains("no honest violation"),
            "{mutant:?} must be refuted by `no honest violation`"
        );
    }
}
