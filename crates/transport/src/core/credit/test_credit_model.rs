//! Model check of the credit laws over every interleaving of one lane.
//!
//! The state is one lane of one connection generation, closed under the production
//! transitions of [`SendCredit`] and [`ReceiveWindow`]:
//!
//! ```text
//!   LaneState = SendCredit × ℕ (frames still to send) × ℕ (data frames in flight)
//!             × ReceiveWindow × 𝒫(ℕ) (credit frames in flight)
//!             × 𝒫(ℕ) (credits whose send failed, queued again) × 𝔹 (violation seen)
//! ```
//!
//! Data frames travel on the lane's ordered channel, so a count of frames in flight is their
//! whole state. Credit frames travel as datagrams: the in-flight set is delivered in any order,
//! and in the duplicating model a delivery may leave its credit in flight to be delivered again.
//! A send may be abandoned before it is irrevocable (`Cancel`), and the receiver releases
//! admitted frames at any time, advertising at once or deferring the advertisement to any later
//! step, as the node's load throttle does. A credit frame's send may fail, as it does while an
//! outbound channel is closed; the pump queues the credit again and sends it later.
//!
//! Properties, checked for several windows `(W, b)`:
//!
//! - `Always` no violation: an honest sender never exceeds the advertised credit;
//! - `Always` bound: `received − released ≤ W`, for an honest sender and for one that floods
//!   frames without credit alike, since the receiver refuses every frame beyond its credit;
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
//! whose send failed, and a batch larger than the window.
//!
//! Isolation is structural rather than checked: the production state of a connection is the
//! product of independent lanes, and no lane transition reads another lane.

use std::collections::BTreeSet;

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::CreditWindow;
use super::ReceiveWindow;
use super::SendCredit;

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
    /// Credit frames sent and not yet delivered.
    credits: BTreeSet<u64>,
    /// Credits whose send failed, queued again by the pump for a later send.
    failed: BTreeSet<u64>,
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
    /// The send of a credit frame fails; the pump queues the credit again.
    FailCredit(u64),
    /// The pump sends a credit whose earlier send failed.
    ResendCredit(u64),
    /// A dishonest sender puts a frame on the channel without a credit.
    Flood,
    /// A credit frame is delivered, and kept in flight when `duplicate`.
    Grant {
        /// The credit delivered.
        limit: u64,
        /// Whether the credit stays in flight to be delivered again.
        duplicate: bool,
    },
}

/// The lane model: a window, a workload, and whether credits may be delivered more than once.
struct LaneModel {
    /// The credit window under check.
    window: CreditWindow,
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
            || state
                .credits
                .iter()
                .any(|limit| *limit > state.sender.limit)
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
        actions.extend(state.failed.iter().copied().map(LaneAction::ResendCredit));
        for limit in state.credits.iter().copied() {
            actions.push(LaneAction::FailCredit(limit));
            actions.push(LaneAction::Grant {
                limit,
                duplicate: false,
            });
            if self.duplicating {
                actions.push(LaneAction::Grant {
                    limit,
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
                if let Some(limit) = state.receiver.advertise(self.window) {
                    let limit = match self.mutant {
                        Some(Mutant::OverAdvertise) => {
                            state.receiver.advertised = limit.saturating_add(1);
                            limit.saturating_add(1)
                        }
                        _ => limit,
                    };
                    state.credits.insert(limit);
                }
            }
            LaneAction::FailCredit(limit) => {
                state.credits.remove(&limit);
                if self.mutant != Some(Mutant::DropFailedCredit) {
                    state.failed.insert(limit);
                }
            }
            LaneAction::ResendCredit(limit) => {
                state.failed.remove(&limit);
                state.credits.insert(limit);
            }
            LaneAction::Grant { limit, duplicate } => {
                state.sender.grant(limit);
                if !duplicate {
                    state.credits.remove(&limit);
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
    LaneModel {
        window,
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

/// Safety and deadlock freedom with credits reordered and duplicated.
#[test]
fn test_credit_is_safe_and_deadlock_free_with_duplicated_credits() {
    for window in windows() {
        lane_model(window, true, true)
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }
}

/// With every credit delivered once, in any order, safety holds and every path ends with all
/// work done. Liveness is checked on terminating paths; a path that reserves and cancels
/// forever is a cycle, which the checker treats as fair.
#[test]
fn test_credit_completes_every_path_with_reordered_credits() {
    for window in windows() {
        lane_model(window, false, true)
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }
}

/// A sender that floods frames without credit cannot push the receiver past its window: every
/// frame beyond the advertised credit is refused, and that refusal is reachable.
#[test]
fn test_receiver_refuses_a_flooding_sender_beyond_its_window() {
    for window in windows() {
        lane_model(window, false, false)
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
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
/// is refused by the receiver, a violation; a batch larger than the window never advertises, so
/// the honest sender deadlocks.
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
    let oversized_batch = CreditWindow {
        frames: 2,
        batch: 3,
    };
    assert!(refuted(lane_model(oversized_batch, false, true)).contains("no honest deadlock"));
}
