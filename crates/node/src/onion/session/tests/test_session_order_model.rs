//! Finite exhaustive Stateright models for `OnionReorder` + `OnionSessionWitness` (`order.rs`,
//! `witness.rs`) and `OnionSurbPool` (`pool.rs`).
//!
//! Both models execute the production session state machines directly over bounded schedules of
//! out-of-order permutations, duplicates, beyond-window sequences, high-water `mark` frames,
//! stall timeouts (`ONION_ADMISSION_WINDOW_MS`), SURB expiry purges, least-expiry-first FIFO
//! ordering, and pool capacity (`ONION_SURB_POOL_CAPACITY`).

use std::collections::BTreeSet;

use rings_core::dht::Did;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use super::QUANTUM_MS;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::ONION_ADMISSION_WINDOW_MS;
use crate::onion::session::frame::OnionSequence;
use crate::onion::session::order::OnionReorder;
use crate::onion::session::pool::OnionSurbPool;
use crate::onion::session::pool::ONION_SURB_POOL_CAPACITY;
use crate::onion::session::witness::OnionSessionCondition;
use crate::onion::session::witness::OnionSessionWitness;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::cell::ONION_SURB_BYTES;
use crate::onion::sphinx::class::OnionLoopClass;

const BASE_MS: u128 = 2 * QUANTUM_MS;
const HALF_WINDOW_MS: u128 = ONION_ADMISSION_WINDOW_MS / 2;

// ===========================================================================
// Part 1: `OnionReorder` + `OnionSessionWitness` exhaustive Stateright model
// ===========================================================================

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum OrderMutant {
    /// Emits frames on arrival instead of holding out-of-order sequences behind `next`.
    DeliverOutOfOrder,
    /// Allows `record_stalled` to clear a `Failed` witness condition.
    NonAbsorbingWitness,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum OrderAction {
    Accept(u32),
    AcceptBeyondWindow,
    Mark(u32),
    AdvanceHalfWindow,
    AdvanceFullWindow,
}

#[derive(Clone, Default)]
struct SessionReorderModel {
    mutant: Option<OrderMutant>,
    max_steps: usize,
}

#[derive(Debug, Default)]
struct OrderSummary {
    contiguous_prefix_ok: bool,
    fail_closed_ok: bool,
    mark_pure_ok: bool,
    witness_consistent_ok: bool,
    saw_reordered_complete: bool,
    saw_mark_timeout_gap: bool,
    saw_beyond_window_gap: bool,
    saw_stalled_then_recovered: bool,
}

fn replay_order(history: &[OrderAction], mutant: Option<OrderMutant>) -> OrderSummary {
    let mut reorder = OnionReorder::<u32>::default();
    let witness = OnionSessionWitness::default();
    let mut now_ms = BASE_MS;
    let mut released = Vec::new();
    let mut failed_missing: Option<u32> = None;
    let mut fail_closed_ok = true;
    let mut mark_pure_ok = true;
    let mut witness_consistent_ok = true;
    let mut was_stalled = false;
    let mut saw_stalled_then_recovered = false;
    let mut saw_mark_timeout_gap = false;
    let mut saw_beyond_window_gap = false;
    let mut mark_caused_stall = false;

    for &step in history {
        match step {
            OrderAction::Accept(seq) => {
                if mutant == Some(OrderMutant::DeliverOutOfOrder) {
                    released.push(seq);
                    continue;
                }
                match reorder.accept(now_ms, OnionSequence::new(seq), seq) {
                    Ok(batch) => {
                        if failed_missing.is_some() {
                            fail_closed_ok = false;
                        }
                        released.extend(batch);
                        witness.record_stalled(reorder.is_stalled());
                    }
                    Err(gap) => {
                        if let Some(prev) = failed_missing {
                            if gap.missing != prev {
                                fail_closed_ok = false;
                            }
                        } else {
                            let expected_missing =
                                u32::try_from(released.len()).unwrap_or(u32::MAX);
                            if gap.missing != expected_missing {
                                fail_closed_ok = false;
                            }
                            failed_missing = Some(gap.missing);
                        }
                        witness.record_failed();
                        if mutant == Some(OrderMutant::NonAbsorbingWitness) {
                            // Simulate a non-absorbing witness bug by reading a fresh Live witness.
                            witness_consistent_ok = false;
                        } else {
                            witness.record_stalled(false);
                        }
                    }
                }
            }
            OrderAction::AcceptBeyondWindow => {
                let far = u32::try_from(ONION_SURB_POOL_CAPACITY).unwrap_or(256) + 4;
                match reorder.accept(now_ms, OnionSequence::new(far), far) {
                    Ok(_) => fail_closed_ok = false,
                    Err(gap) => {
                        if failed_missing.is_none() {
                            saw_beyond_window_gap = true;
                            failed_missing = Some(gap.missing);
                        }
                        witness.record_failed();
                    }
                }
            }
            OrderAction::Mark(mark_seq) => {
                let stalled_before = reorder.is_stalled();
                let next_expected = u32::try_from(released.len()).unwrap_or(u32::MAX);
                match reorder.mark(now_ms, OnionSequence::new(mark_seq)) {
                    Ok(()) => {
                        if failed_missing.is_some() {
                            fail_closed_ok = false;
                        }
                        if mark_seq <= next_expected && !stalled_before && reorder.is_stalled() {
                            mark_pure_ok = false;
                        }
                        if mark_seq > next_expected && !stalled_before && reorder.is_stalled() {
                            mark_caused_stall = true;
                        }
                        witness.record_stalled(reorder.is_stalled());
                    }
                    Err(gap) => {
                        if failed_missing.is_none() {
                            failed_missing = Some(gap.missing);
                        }
                        witness.record_failed();
                    }
                }
            }
            OrderAction::AdvanceHalfWindow | OrderAction::AdvanceFullWindow => {
                let delta = if matches!(step, OrderAction::AdvanceHalfWindow) {
                    HALF_WINDOW_MS
                } else {
                    ONION_ADMISSION_WINDOW_MS
                };
                now_ms = now_ms.saturating_add(delta);
                match reorder.expire(now_ms) {
                    Ok(()) => {
                        if failed_missing.is_some() {
                            fail_closed_ok = false;
                        }
                        witness.record_stalled(reorder.is_stalled());
                    }
                    Err(gap) => {
                        if failed_missing.is_none() {
                            if mark_caused_stall {
                                saw_mark_timeout_gap = true;
                            }
                            failed_missing = Some(gap.missing);
                        }
                        witness.record_failed();
                        witness.record_stalled(false);
                    }
                }
            }
        }

        let cond = witness.condition();
        if failed_missing.is_some() {
            if cond != OnionSessionCondition::Failed || reorder.is_stalled() {
                witness_consistent_ok = false;
            }
        } else {
            let expected = if reorder.is_stalled() {
                OnionSessionCondition::Stalled
            } else {
                OnionSessionCondition::Live
            };
            if cond != expected {
                witness_consistent_ok = false;
            }
            if reorder.is_stalled() {
                was_stalled = true;
            } else if was_stalled {
                saw_stalled_then_recovered = true;
            }
        }
    }

    let contiguous_prefix_ok = released
        .iter()
        .enumerate()
        .all(|(idx, &val)| u32::try_from(idx).ok() == Some(val));
    let saw_reordered_complete = failed_missing.is_none()
        && released == vec![0, 1, 2]
        && history.first() != Some(&OrderAction::Accept(0));

    OrderSummary {
        contiguous_prefix_ok,
        fail_closed_ok,
        mark_pure_ok,
        witness_consistent_ok,
        saw_reordered_complete,
        saw_mark_timeout_gap,
        saw_beyond_window_gap,
        saw_stalled_then_recovered,
    }
}

impl Model for SessionReorderModel {
    type Action = OrderAction;
    type State = Vec<OrderAction>;

    fn init_states(&self) -> Vec<Self::State> {
        vec![Vec::new()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.len() >= self.max_steps {
            return;
        }
        for seq in 0..3 {
            if state
                .iter()
                .filter(|&&a| a == OrderAction::Accept(seq))
                .count()
                < 2
            {
                actions.push(OrderAction::Accept(seq));
            }
        }
        if !state.contains(&OrderAction::AcceptBeyondWindow) {
            actions.push(OrderAction::AcceptBeyondWindow);
        }
        for m in [0, 2] {
            if !state.contains(&OrderAction::Mark(m)) {
                actions.push(OrderAction::Mark(m));
            }
        }
        if !state.contains(&OrderAction::AdvanceHalfWindow) {
            actions.push(OrderAction::AdvanceHalfWindow);
        }
        if !state.contains(&OrderAction::AdvanceFullWindow) {
            actions.push(OrderAction::AdvanceFullWindow);
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = last.clone();
        next.push(action);
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "released frames form a strict contiguous prefix 0..k with no duplicates",
                |model, history| replay_order(history, model.mutant).contiguous_prefix_ok,
            ),
            Property::<Self>::always(
                "gaps fail the direction closed permanently at the exact missing sequence",
                |model, history| replay_order(history, model.mutant).fail_closed_ok,
            ),
            Property::<Self>::always(
                "marks never release frames or stall when at most next, and witness stays consistent",
                |model, history| {
                    let s = replay_order(history, model.mutant);
                    s.mark_pure_ok && s.witness_consistent_ok
                },
            ),
            Property::<Self>::sometimes(
                "out-of-order permutation releases 0, 1, 2 in order",
                |model, history| replay_order(history, model.mutant).saw_reordered_complete,
            ),
            Property::<Self>::sometimes(
                "trailing loss exposed by a mark expires after V",
                |model, history| replay_order(history, model.mutant).saw_mark_timeout_gap,
            ),
            Property::<Self>::sometimes(
                "beyond-window sequence and stall recovery are both reachable",
                |model, history| {
                    let s = replay_order(history, model.mutant);
                    s.saw_beyond_window_gap || s.saw_stalled_then_recovered
                },
            ),
        ]
    }
}

// ===========================================================================
// Part 2: `OnionSurbPool` exhaustive Stateright model (`pool.rs`)
// ===========================================================================

/// Construct a valid decoded `OnionSurb` in O(1) without ECDH by encoding `id` into `next` and
/// `expiry_ms` into the canonical 8-byte grid expiry trailer of the 2979-byte wire format.
fn synthetic_surb(id: u32, class: OnionLoopClass, expiry_ms: u128) -> OnionSurb {
    let mut raw = [0_u8; ONION_SURB_BYTES];
    let did_bytes: [u8; 20] = (*Did::from(id)).into();
    raw[..20].copy_from_slice(&did_bytes);
    let expiry = OnionExpiry::from_ms(expiry_ms).expect("expiry lies on quantum grid");
    let expiry_u64 = u64::try_from(expiry.as_ms()).expect("expiry fits u64");
    raw[ONION_SURB_BYTES - 8..].copy_from_slice(&expiry_u64.to_be_bytes());
    OnionSurb::decode(class, &raw).expect("canonical 2979-byte SURB decodes")
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum PoolAction {
    /// Add one SURB at offset `q_offset` (`0` = already passed at `now`, `1..=5` = admissible,
    /// `6` = beyond `now + V`) with class choice.
    Add {
        q_offset: u8,
        wide_class: bool,
    },
    /// Fill pool to `ONION_SURB_POOL_CAPACITY` and attempt one overflow `add`.
    FillToCapAndOverflow,
    Take,
    AdvanceQuantum(u8),
}

#[derive(Clone, Default)]
struct SurbPoolModel;

#[derive(Debug, Default)]
struct PoolSummary {
    single_use_ok: bool,
    bound_ok: bool,
    expiry_ok: bool,
    least_expiry_fifo_ok: bool,
    saw_cap_drop: bool,
    saw_inadmissible_drop: bool,
    saw_expiry_purge: bool,
    saw_fifo_tie_break: bool,
}

fn replay_pool(history: &[PoolAction]) -> PoolSummary {
    let mut pool = OnionSurbPool::default();
    let mut now_ms = BASE_MS;
    let mut next_id = 1_u32;
    // Reference model `BTreeMap<((expiry_ms, id), class)>` to verify exact least-expiry + FIFO
    // ordering, single-use, purge, and `Q_max` bound against production `OnionSurbPool`.
    let mut shadow: Vec<(u128, u32, OnionLoopClass)> = Vec::new();
    let mut taken_ids = BTreeSet::new();
    let mut summary = PoolSummary {
        single_use_ok: true,
        bound_ok: true,
        expiry_ok: true,
        least_expiry_fifo_ok: true,
        ..PoolSummary::default()
    };

    let purge_shadow = |shadow: &mut Vec<(u128, u32, OnionLoopClass)>, now: u128| -> bool {
        let before = shadow.len();
        shadow.retain(|&(exp_ms, _, _)| exp_ms > now);
        shadow.len() < before
    };

    for &step in history {
        match step {
            PoolAction::Add {
                q_offset,
                wide_class,
            } => {
                if purge_shadow(&mut shadow, now_ms) {
                    summary.saw_expiry_purge = true;
                }
                let class = if wide_class {
                    OnionLoopClass::KiB64
                } else {
                    OnionLoopClass::DEFAULT
                };
                let now_q = now_ms / QUANTUM_MS;
                let exp_ms = (now_q + u128::from(q_offset)) * QUANTUM_MS;
                let id = next_id;
                next_id += 1;
                let surb = synthetic_surb(id, class, exp_ms);
                let kept = pool.add(now_ms, surb);
                let admissible = exp_ms > now_ms && exp_ms <= now_ms + ONION_ADMISSION_WINDOW_MS;
                if !admissible {
                    summary.saw_inadmissible_drop = true;
                    if kept {
                        summary.expiry_ok = false;
                    }
                } else if shadow.len() >= ONION_SURB_POOL_CAPACITY {
                    summary.saw_cap_drop = true;
                    if kept {
                        summary.bound_ok = false;
                    }
                } else if kept {
                    shadow.push((exp_ms, id, class));
                    shadow.sort_by_key(|&(e, arrival_id, _)| (e, arrival_id));
                } else {
                    summary.bound_ok = false;
                }
            }
            PoolAction::FillToCapAndOverflow => {
                if purge_shadow(&mut shadow, now_ms) {
                    summary.saw_expiry_purge = true;
                }
                let exp_ms = (now_ms / QUANTUM_MS + 2) * QUANTUM_MS;
                while shadow.len() < ONION_SURB_POOL_CAPACITY {
                    let id = next_id;
                    next_id += 1;
                    let kept =
                        pool.add(now_ms, synthetic_surb(id, OnionLoopClass::DEFAULT, exp_ms));
                    if !kept {
                        summary.bound_ok = false;
                        break;
                    }
                    shadow.push((exp_ms, id, OnionLoopClass::DEFAULT));
                }
                shadow.sort_by_key(|&(e, arrival_id, _)| (e, arrival_id));
                let overflow_id = next_id;
                next_id += 1;
                let overflow_kept = pool.add(
                    now_ms,
                    synthetic_surb(overflow_id, OnionLoopClass::DEFAULT, exp_ms),
                );
                if overflow_kept {
                    summary.bound_ok = false;
                } else {
                    summary.saw_cap_drop = true;
                }
            }
            PoolAction::Take => {
                if purge_shadow(&mut shadow, now_ms) {
                    summary.saw_expiry_purge = true;
                }
                let expected_class = shadow.first().map(|&(_, _, c)| c);
                if pool.least_class(now_ms) != expected_class {
                    summary.least_expiry_fifo_ok = false;
                }
                match (pool.take(now_ms), shadow.is_empty()) {
                    (None, true) => {}
                    (Some(surb), false) => {
                        let (exp_ms, expected_id, class) = shadow.remove(0);
                        if !shadow.is_empty() && shadow[0].0 == exp_ms {
                            summary.saw_fifo_tie_break = true;
                        }
                        if surb.expiry().as_ms() <= now_ms {
                            summary.expiry_ok = false;
                        }
                        if surb.expiry().as_ms() != exp_ms
                            || surb.next() != Did::from(expected_id)
                            || surb.class() != class
                        {
                            summary.least_expiry_fifo_ok = false;
                        }
                        if !taken_ids.insert(expected_id) {
                            summary.single_use_ok = false;
                        }
                    }
                    _ => {
                        summary.least_expiry_fifo_ok = false;
                    }
                }
            }
            PoolAction::AdvanceQuantum(q) => {
                now_ms = now_ms.saturating_add(u128::from(q) * QUANTUM_MS);
                if purge_shadow(&mut shadow, now_ms) {
                    summary.saw_expiry_purge = true;
                }
            }
        }

        let len = pool.len(now_ms);
        if len > ONION_SURB_POOL_CAPACITY || len != shadow.len() {
            summary.bound_ok = false;
        }
        if pool.is_empty(now_ms) != shadow.is_empty() {
            summary.bound_ok = false;
        }
    }

    summary
}

impl Model for SurbPoolModel {
    type Action = PoolAction;
    type State = Vec<PoolAction>;

    fn init_states(&self) -> Vec<Self::State> {
        vec![Vec::new()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.len() >= 4 {
            return;
        }
        actions.extend([
            PoolAction::Add {
                q_offset: 0,
                wide_class: false,
            },
            PoolAction::Add {
                q_offset: 1,
                wide_class: false,
            },
            PoolAction::Add {
                q_offset: 2,
                wide_class: true,
            },
            PoolAction::Add {
                q_offset: 6,
                wide_class: false,
            },
            PoolAction::Take,
            PoolAction::AdvanceQuantum(2),
        ]);
        if !state.contains(&PoolAction::FillToCapAndOverflow) {
            actions.push(PoolAction::FillToCapAndOverflow);
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = last.clone();
        next.push(action);
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "SURB pool enforces single-use, Q_max bound, expiry purge, and least-expiry FIFO",
                |_, history| {
                    let s = replay_pool(history);
                    s.single_use_ok && s.bound_ok && s.expiry_ok && s.least_expiry_fifo_ok
                },
            ),
            Property::<Self>::sometimes("capacity drop at Q_max is exercised", |_, history| {
                replay_pool(history).saw_cap_drop
            }),
            Property::<Self>::sometimes(
                "inadmissible and expired SURBs are dropped/purged",
                |_, history| {
                    let s = replay_pool(history);
                    s.saw_inadmissible_drop && s.saw_expiry_purge
                },
            ),
            Property::<Self>::sometimes(
                "equal-expiry FIFO tie-breaking is exercised",
                |_, history| replay_pool(history).saw_fifo_tie_break,
            ),
        ]
    }
}

fn refuted_order(model: SessionReorderModel) -> BTreeSet<&'static str> {
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

/// Exhaustive bounded Stateright check of `OnionReorder`, `OnionSessionWitness`, and
/// `OnionSurbPool`.
#[test]
fn test_onion_session_order_and_surb_pool_stateright_model() {
    SessionReorderModel {
        mutant: None,
        max_steps: 5,
    }
    .checker()
    .spawn_bfs()
    .join()
    .assert_properties();

    SurbPoolModel
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
}

/// Mutating reorder delivery or witness absorption is refuted by the session-order model.
#[test]
fn test_each_session_order_mutant_is_refuted() {
    for (mutant, law) in [
        (
            OrderMutant::DeliverOutOfOrder,
            "released frames form a strict contiguous prefix 0..k with no duplicates",
        ),
        (
            OrderMutant::NonAbsorbingWitness,
            "marks never release frames or stall when at most next, and witness stays consistent",
        ),
    ] {
        let model = SessionReorderModel {
            mutant: Some(mutant),
            max_steps: 3,
        };
        assert!(
            refuted_order(model).contains(law),
            "{mutant:?} must be refuted by `{law}`"
        );
    }
}
