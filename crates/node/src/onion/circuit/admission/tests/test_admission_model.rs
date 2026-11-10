//! Finite exhaustive Stateright model for `OnionAdmissionState` (`charge`, `admit`, expiry,
//! epoch reset, replay filtering, and per-slot/global unit budgets).
//!
//! Every transition replays its bounded action history directly through the production
//! [`OnionAdmissionState`] APIs (`new`, `link_opened`, `link_closed`, `reconcile`, `renew`,
//! `is_rolled_back_at`, `charge`, and `admit`), so the model cannot drift from production.

use std::collections::BTreeSet;

use rings_core::dht::Did;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use super::expiry;
use super::generation;
use super::registry;
use super::units;
use super::Verdict;
use super::EPOCH;
use super::ORIGIN_MS;
use super::Q;
use crate::onion::circuit::admission::OnionAdmissionLayer;
use crate::onion::circuit::admission::OnionAdmissionRejection;
use crate::onion::circuit::admission::OnionAdmissionState;
use crate::onion::circuit::admission::OnionChargeRejection;
use crate::onion::circuit::admission::OnionEpochNotFresh;
use crate::onion::circuit::admission::OnionLinkTableFull;
use crate::onion::circuit::admission::OnionReplayFilterKey;
use crate::onion::circuit::admission::ONION_ADMISSION_SENDER_UNITS;
use crate::onion::circuit::OnionReplayNonce;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionProcessEpoch;

const FRESH_EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([9; 16]);
const MAX_HISTORY_LEN: usize = 5;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Mutant {
    /// Skips replay tracking, admitting the same `(epoch, expiry, tag)` twice.
    AllowReplay,
    /// Skips the per-slot sender unit cap `B`.
    IgnoreSenderBudget,
    /// Allows `renew` into the current process epoch, wiping the replay store without rotating
    /// `epoch`.
    RenewSameEpoch,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ExpiryKind {
    /// Admissible expiry `arr_q + 2`.
    Valid,
    /// Already passed at arrival (`arr_q`).
    Passed,
    /// Beyond `arr + V` (`arr_q + 6`).
    TooFarFuture,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Action {
    OpenLink {
        peer: u8,
        gen_id: u8,
    },
    CloseLink {
        peer: u8,
        gen_id: u8,
    },
    Reconcile {
        mask: u8,
    },
    /// Calls production `OnionAdmissionState::charge` on a real `OnionCell` and drops the affine
    /// `Charged<OnionCell>` token (modelling an invalid `α` or failed `γ`).
    ChargeCellDropToken {
        peer: u8,
    },
    /// Runs `charge_units ; admit` with configurable units, epoch freshness, expiry window, and
    /// replay tag.
    SendLayer {
        peer: u8,
        heavy: bool,
        stale_epoch: bool,
        expiry_kind: ExpiryKind,
        tag: u8,
    },
    AdvanceQuanta(u8),
    /// Evaluates a wall-clock rollback behind `clock - X_0` (`is_rolled_back_at`) and renews into
    /// `FRESH_EPOCH` (while also verifying `renew(current_epoch)` fails with `OnionEpochNotFresh`).
    RollbackAndRenew,
}

#[derive(Clone, Default)]
struct OnionAdmissionModel {
    mutant: Option<Mutant>,
    depth_limit: Option<usize>,
}

#[derive(Debug, Default)]
struct ReplaySummary {
    admitted_tuples: Vec<(OnionProcessEpoch, u128, u8)>,
    duplicate_admitted: bool,
    over_sender_budget: bool,
    releasable_slot_retained: bool,
    live_cap_exceeded: bool,
    unlive_charged: bool,
    same_epoch_renew_accepted: bool,
    rollback_refunded_load: bool,
    saw_admitted: bool,
    saw_replayed: bool,
    saw_outside_window: bool,
    saw_stale_epoch: bool,
    saw_sender_budget: bool,
    saw_link_not_live: bool,
    saw_table_full: bool,
    saw_renew: bool,
}

fn candidate_actions() -> &'static [Action] {
    &[
        Action::OpenLink { peer: 0, gen_id: 0 },
        Action::OpenLink { peer: 1, gen_id: 0 },
        Action::OpenLink { peer: 2, gen_id: 0 },
        Action::CloseLink { peer: 0, gen_id: 0 },
        Action::OpenLink { peer: 0, gen_id: 1 },
        Action::Reconcile { mask: 0b111 },
        Action::ChargeCellDropToken { peer: 0 },
        Action::SendLayer {
            peer: 0,
            heavy: false,
            stale_epoch: false,
            expiry_kind: ExpiryKind::Valid,
            tag: 1,
        },
        Action::SendLayer {
            peer: 0,
            heavy: true,
            stale_epoch: false,
            expiry_kind: ExpiryKind::Valid,
            tag: 2,
        },
        Action::SendLayer {
            peer: 0,
            heavy: false,
            stale_epoch: true,
            expiry_kind: ExpiryKind::Valid,
            tag: 3,
        },
        Action::SendLayer {
            peer: 0,
            heavy: false,
            stale_epoch: false,
            expiry_kind: ExpiryKind::Passed,
            tag: 4,
        },
        Action::SendLayer {
            peer: 0,
            heavy: false,
            stale_epoch: false,
            expiry_kind: ExpiryKind::TooFarFuture,
            tag: 5,
        },
        Action::AdvanceQuanta(1),
        Action::AdvanceQuanta(5),
        Action::RollbackAndRenew,
    ]
}

fn evaluate_history(history: &[Action], mutant: Option<Mutant>) -> ReplaySummary {
    let mut summary = ReplaySummary::default();
    // `R = 1` gives capacity `2 * R = 2` connection slots and live links, making `peer = 2`
    // exercise `OnionLinkTableFull` and slot reuse within a small state space.
    let mut state =
        OnionAdmissionState::new(EPOCH, OnionReplayFilterKey::new([3; 32]), registry(1));
    let mut now_ms = ORIGIN_MS;
    let mut current_epoch = EPOCH;

    // Seed time at `ORIGIN_MS` with an empty reconcile.
    let _ = state.reconcile(now_ms, []);

    for &action in history {
        match action {
            Action::OpenLink { peer, gen_id } => {
                let lnk = generation(u32::from(peer), u64::from(gen_id));
                if state.link_opened(now_ms, lnk) == Err(OnionLinkTableFull) {
                    summary.saw_table_full = true;
                }
            }
            Action::CloseLink { peer, gen_id } => {
                let lnk = generation(u32::from(peer), u64::from(gen_id));
                state.link_closed(now_ms, lnk);
            }
            Action::Reconcile { mask } => {
                let mut live = Vec::new();
                for bit in 0..3_u8 {
                    if mask & (1 << bit) != 0 {
                        live.push(generation(u32::from(bit), 0));
                    }
                }
                let refused = state.reconcile(now_ms, live);
                if !refused.links().is_empty() {
                    summary.saw_table_full = true;
                }
            }
            Action::ChargeCellDropToken { peer } => {
                let did = Did::from(u32::from(peer));
                let target_link = state
                    .live_link(did)
                    .unwrap_or_else(|| generation(u32::from(peer), 0));
                let was_live = state.live_links().contains(&target_link);
                let before_units = state.sender_units(did);
                let cell = OnionCell::parse(&vec![0_u8; OnionLoopClass::DEFAULT.cell_bytes()])
                    .expect("canonical cell bytes parse");
                match state.charge(now_ms, target_link, cell) {
                    Ok(charged) => {
                        if !was_live {
                            summary.unlive_charged = true;
                        }
                        // Dropping `charged` without calling `admit` models an invalid header or
                        // MAC: the unit charge stands.
                        drop(charged);
                        if state.sender_units(did) <= before_units {
                            summary.rollback_refunded_load = true;
                        }
                    }
                    Err(OnionChargeRejection::LinkNotLive) => {
                        summary.saw_link_not_live = true;
                        if state.sender_units(did) != before_units {
                            summary.unlive_charged = true;
                        }
                    }
                    Err(
                        OnionChargeRejection::SenderBudget | OnionChargeRejection::GlobalBudget,
                    ) => {
                        summary.saw_sender_budget = true;
                    }
                }
            }
            Action::SendLayer {
                peer,
                heavy,
                stale_epoch,
                expiry_kind,
                tag,
            } => {
                let did = Did::from(u32::from(peer));
                let target_link = state
                    .live_link(did)
                    .unwrap_or_else(|| generation(u32::from(peer), 0));
                let was_live = state.live_links().contains(&target_link);
                let before_units = state.sender_units(did);
                // `heavy` charges `(B / 2) + 1 = 8_193` units, so two heavy cells in one 5-quantum
                // window exceed `B = 16_384` and trigger `SenderBudget`.
                let requested = if heavy {
                    ONION_ADMISSION_SENDER_UNITS / 2 + 1
                } else {
                    1
                };
                let arr_quantum = state.clock_ms.max(now_ms) / Q;
                let exp = match expiry_kind {
                    ExpiryKind::Valid => expiry(arr_quantum + 2),
                    ExpiryKind::Passed => expiry(arr_quantum),
                    ExpiryKind::TooFarFuture => expiry(arr_quantum + 6),
                };
                let effective_tag = if mutant == Some(Mutant::AllowReplay) {
                    u128::from(tag) + (u128::from(summary.admitted_tuples.len() as u8) << 8)
                } else {
                    u128::from(tag)
                };
                let layer_epoch = if stale_epoch {
                    OnionProcessEpoch::new([99; 16])
                } else {
                    current_epoch
                };
                let lyr = OnionAdmissionLayer {
                    epoch: layer_epoch,
                    expiry: exp,
                    tag: OnionReplayNonce::new(effective_tag.to_le_bytes()),
                };

                let verdict = if mutant == Some(Mutant::IgnoreSenderBudget) && was_live {
                    let would_overflow =
                        before_units.saturating_add(requested) > ONION_ADMISSION_SENDER_UNITS;
                    let charged_units = if would_overflow { 1 } else { requested };
                    if would_overflow {
                        summary.over_sender_budget = true;
                    }
                    match state.charge_units(now_ms, target_link, units(charged_units)) {
                        Err(rej) => Verdict::Unpaid(rej),
                        Ok(tok) => match state.admit(now_ms, tok, lyr) {
                            Ok(()) => Verdict::Admitted,
                            Err(rej) => Verdict::Rejected(rej),
                        },
                    }
                } else {
                    match state.charge_units(now_ms, target_link, units(requested)) {
                        Err(rej) => Verdict::Unpaid(rej),
                        Ok(tok) => match state.admit(now_ms, tok, lyr) {
                            Ok(()) => Verdict::Admitted,
                            Err(rej) => Verdict::Rejected(rej),
                        },
                    }
                };

                match verdict {
                    Verdict::Admitted => {
                        summary.saw_admitted = true;
                        let key = (layer_epoch, exp.as_ms(), tag);
                        if summary.admitted_tuples.contains(&key) {
                            summary.duplicate_admitted = true;
                        } else {
                            summary.admitted_tuples.push(key);
                        }
                    }
                    Verdict::Unpaid(OnionChargeRejection::LinkNotLive) => {
                        summary.saw_link_not_live = true;
                        if state.sender_units(did) != before_units {
                            summary.unlive_charged = true;
                        }
                    }
                    Verdict::Unpaid(
                        OnionChargeRejection::SenderBudget | OnionChargeRejection::GlobalBudget,
                    ) => {
                        summary.saw_sender_budget = true;
                    }
                    Verdict::Rejected(OnionAdmissionRejection::Replayed) => {
                        summary.saw_replayed = true;
                    }
                    Verdict::Rejected(OnionAdmissionRejection::OutsideWindow) => {
                        summary.saw_outside_window = true;
                    }
                    Verdict::Rejected(OnionAdmissionRejection::StaleEpoch) => {
                        summary.saw_stale_epoch = true;
                    }
                }
                if !was_live
                    && !matches!(verdict, Verdict::Unpaid(OnionChargeRejection::LinkNotLive))
                {
                    summary.unlive_charged = true;
                }
            }
            Action::AdvanceQuanta(q) => {
                now_ms = now_ms.saturating_add(u128::from(q) * Q);
                // Trigger quantum sweep via an idempotent close of an unused link.
                state.link_closed(now_ms, generation(99, 0));
            }
            Action::RollbackAndRenew => {
                // Simulate a wall-clock rollback of `5Q > X_0 = 4Q`: monotone clock must not
                // refund existing sender units before `renew`, and `is_rolled_back_at` must hold.
                let rolled_back_ms = state.clock_ms.saturating_sub(5 * Q);
                let pre_units = state.sender_units(Did::from(0_u32));
                state.link_closed(rolled_back_ms, generation(99, 0));
                if state.sender_units(Did::from(0_u32)) < pre_units {
                    summary.rollback_refunded_load = true;
                }
                if state.is_rolled_back_at(rolled_back_ms) {
                    if mutant == Some(Mutant::RenewSameEpoch)
                        || state.renew(current_epoch, OnionReplayFilterKey::new([5; 32]))
                            != Err(OnionEpochNotFresh)
                    {
                        summary.same_epoch_renew_accepted = true;
                    }
                    let next_epoch = if current_epoch == EPOCH {
                        FRESH_EPOCH
                    } else {
                        EPOCH
                    };
                    let live_before = state.live_links();
                    state
                        .renew(next_epoch, OnionReplayFilterKey::new([6; 32]))
                        .expect("distinct epoch renews cleanly");
                    assert_eq!(
                        state.live_links(),
                        live_before,
                        "renew preserves live links"
                    );
                    current_epoch = next_epoch;
                    now_ms = ORIGIN_MS;
                    let _ = state.reconcile(now_ms, live_before);
                    summary.saw_renew = true;
                }
            }
        }

        // Check structural invariants after every step.
        if state.live_links().len() > state.capacity {
            summary.live_cap_exceeded = true;
        }
        let quantum = state.clock_ms / Q;
        for peer in 0..3_u32 {
            if state.sender_units(Did::from(peer)) > ONION_ADMISSION_SENDER_UNITS {
                summary.over_sender_budget = true;
            }
        }
        if state
            .slots
            .values()
            .any(|slot| slot.is_releasable_at(quantum))
        {
            summary.releasable_slot_retained = true;
        }
    }

    summary
}

impl Model for OnionAdmissionModel {
    type Action = Action;
    type State = Vec<Action>;

    fn init_states(&self) -> Vec<Self::State> {
        if self.mutant.is_some() {
            return vec![vec![Action::OpenLink { peer: 0, gen_id: 0 }]];
        }
        vec![Vec::new(), vec![Action::OpenLink { peer: 0, gen_id: 0 }]]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        let limit = self.depth_limit.unwrap_or(MAX_HISTORY_LEN);
        if state.len() >= limit {
            return;
        }
        actions.extend_from_slice(candidate_actions());
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = last.clone();
        next.push(action);
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "each (epoch, expiry, tag) is admitted at most once",
                |model, history| !evaluate_history(history, model.mutant).duplicate_admitted,
            ),
            Property::<Self>::always(
                "per-slot window budget B and non-releasable ledger invariant hold",
                |model, history| {
                    let s = evaluate_history(history, model.mutant);
                    !s.over_sender_budget && !s.releasable_slot_retained
                },
            ),
            Property::<Self>::always(
                "live-link cap holds and unlive links are never charged",
                |model, history| {
                    let s = evaluate_history(history, model.mutant);
                    !s.live_cap_exceeded && !s.unlive_charged
                },
            ),
            Property::<Self>::always(
                "wall-clock rollback never refunds load and renew rejects current epoch",
                |model, history| {
                    let s = evaluate_history(history, model.mutant);
                    !s.rollback_refunded_load && !s.same_epoch_renew_accepted
                },
            ),
            Property::<Self>::sometimes("a valid layer is admitted", |model, history| {
                evaluate_history(history, model.mutant).saw_admitted
            }),
            Property::<Self>::sometimes(
                "a duplicate tag is rejected as Replayed",
                |model, history| evaluate_history(history, model.mutant).saw_replayed,
            ),
            Property::<Self>::sometimes(
                "expired and far-future layers fail OutsideWindow",
                |model, history| evaluate_history(history, model.mutant).saw_outside_window,
            ),
            Property::<Self>::sometimes(
                "stale process epoch fails StaleEpoch",
                |model, history| evaluate_history(history, model.mutant).saw_stale_epoch,
            ),
            Property::<Self>::sometimes("sender unit cap B rejects overflow", |model, history| {
                evaluate_history(history, model.mutant).saw_sender_budget
            }),
            Property::<Self>::sometimes(
                "link table full and unlive link are exercised",
                |model, history| {
                    let s = evaluate_history(history, model.mutant);
                    s.saw_table_full && s.saw_link_not_live
                },
            ),
            Property::<Self>::sometimes(
                "clock rollback triggers fresh epoch renew",
                |model, history| evaluate_history(history, model.mutant).saw_renew,
            ),
        ]
    }
}

fn refuted(model: OnionAdmissionModel) -> BTreeSet<&'static str> {
    let checker = model.checker().spawn_bfs().join();
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

/// Exhaustive bounded Stateright check over `OnionAdmissionState` (`charge`, `admit`, expiry,
/// epoch reset, replay, and slot/global unit budgets).
#[test]
fn test_onion_admission_stateright_model() {
    OnionAdmissionModel {
        mutant: None,
        depth_limit: Some(3),
    }
    .checker()
    .spawn_bfs()
    .join()
    .assert_properties();
}

/// Every safety property of the onion admission model is refuted when its corresponding guard is
/// mutated.
#[test]
fn test_each_onion_admission_mutant_is_refuted() {
    for (mutant, law) in [
        (
            Mutant::AllowReplay,
            "each (epoch, expiry, tag) is admitted at most once",
        ),
        (
            Mutant::IgnoreSenderBudget,
            "per-slot window budget B and non-releasable ledger invariant hold",
        ),
        (
            Mutant::RenewSameEpoch,
            "wall-clock rollback never refunds load and renew rejects current epoch",
        ),
    ] {
        let model = OnionAdmissionModel {
            mutant: Some(mutant),
            depth_limit: Some(3),
        };
        assert!(
            refuted(model).contains(law),
            "{mutant:?} must be refuted by `{law}`"
        );
    }
}
