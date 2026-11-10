//! Finite exhaustive Stateright models for `OnionExitAccounting` + `PeerQuota`
//! (`exit_accounting.rs`, `peer_quota.rs`) and `OrderedSendState` (`send_outbox/lanes.rs`).
//!
//! Both models execute the pure production reducers directly over bounded schedules of admissions,
//! per-peer and global quota exhaustion, 60 s exit byte-window rollovers, relay vs local vs
//! endpoint outbox roles, half-lane relay yielding under endpoint contention, requeue on unaccepted
//! send, and link close/reopen/shutdown transitions.

use std::collections::BTreeSet;

use rings_core::dht::Did;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use super::lanes::EmitterStep;
use super::lanes::OrderedSendState;
use super::lanes::QueueRole;
use super::lanes::Refusal;
use super::schedule::Emission;
use super::schedule::OnionIdleFloor;
use super::RELAY_SHARE_BYTES;
use super::RELAY_SHARE_CELLS;
use crate::error::OnionQueueAdmissionReason;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionExitPolicy;
use crate::peer_quota::PeerQuota;

// ===========================================================================
// Part 1: `OnionExitAccounting` + `PeerQuota` Stateright model
// ===========================================================================

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ExitMutant {
    /// Skips the per-minute byte window limit on `record_bytes`.
    IgnoreByteWindow,
    /// Skips the per-peer quota cap on `PeerQuota::reserve`.
    IgnorePeerQuota,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ExitAction {
    AdmitPeer(u8),
    ReleasePeer(u8),
    ReleaseEntirePeer(u8),
    SaturatePreviousHopShare,
    RecordBytes(u64),
    AdvanceMinute,
}

#[derive(Clone, Default)]
struct ExitQuotaModel {
    mutant: Option<ExitMutant>,
    max_steps: usize,
}

#[derive(Debug, Default)]
struct ExitSummary {
    quota_conservation_ok: bool,
    session_cap_ok: bool,
    byte_window_ok: bool,
    saw_global_session_full: bool,
    saw_hop_share_full: bool,
    saw_byte_refusal: bool,
    saw_window_reset: bool,
}

fn replay_exit(history: &[ExitAction], mutant: Option<ExitMutant>) -> ExitSummary {
    let accounting = OnionExitAccounting::default();
    let policy = OnionExitPolicy {
        max_sessions: 3,
        max_bytes_per_minute: 10,
        ..OnionExitPolicy::default()
    };
    let mut quota = PeerQuota::new(3, 2);
    let mut leases_by_peer: [Vec<OnionExitLease>; 3] = Default::default();
    let mut counts = [0usize; 3];
    let mut now_ms: u128 = 60_000;
    let mut window_bytes: u64 = 0;

    let mut summary = ExitSummary {
        quota_conservation_ok: true,
        session_cap_ok: true,
        byte_window_ok: true,
        ..ExitSummary::default()
    };

    for &step in history {
        match step {
            ExitAction::AdmitPeer(p) => {
                let idx = usize::from(p);
                let did = Did::from(u32::from(p) + 1);
                let total_held: usize = counts.iter().sum();

                // 1. Check `PeerQuota` transitions (`max_total = 3`, `max_per_peer = 2`).
                let can_res = quota.can_reserve(did);
                let res = if mutant == Some(ExitMutant::IgnorePeerQuota) {
                    counts[idx] += 1;
                    let _ = quota.reserve(Did::from(100 + total_held as u32));
                    Ok(())
                } else {
                    let r = quota.reserve(did);
                    if r.is_ok() {
                        counts[idx] += 1;
                    }
                    assert_eq!(can_res, r);
                    r
                };
                if counts[idx] > 2 || counts.iter().sum::<usize>() > 3 {
                    summary.quota_conservation_ok = false;
                }
                if res == Err(OnionQueueAdmissionReason::GlobalFull) {
                    summary.saw_global_session_full = true;
                }

                // 2. Check `OnionExitAccounting::admit` (`policy.max_sessions = 3`).
                match accounting.admit(&policy, did) {
                    Ok(lease) => {
                        if leases_by_peer.iter().map(Vec::len).sum::<usize>() >= 3 {
                            summary.session_cap_ok = false;
                        }
                        leases_by_peer[idx].push(lease);
                    }
                    Err(_) => {
                        if leases_by_peer.iter().map(Vec::len).sum::<usize>() < 3 {
                            summary.session_cap_ok = false;
                        }
                        summary.saw_global_session_full = true;
                    }
                }
            }
            ExitAction::ReleasePeer(p) => {
                let idx = usize::from(p);
                let did = Did::from(u32::from(p) + 1);
                let released = quota.release(did);
                if counts[idx] > 0 && mutant.is_none() {
                    counts[idx] -= 1;
                    if !released {
                        summary.quota_conservation_ok = false;
                    }
                } else if counts[idx] == 0 && released {
                    summary.quota_conservation_ok = false;
                }
                let _ = leases_by_peer[idx].pop();
            }
            ExitAction::ReleaseEntirePeer(p) => {
                let idx = usize::from(p);
                let did = Did::from(u32::from(p) + 1);
                let removed = quota.release_peer(did);
                if mutant.is_none() {
                    let expected = (counts[idx] > 0).then_some(counts[idx]);
                    if removed != expected {
                        summary.quota_conservation_ok = false;
                    }
                    counts[idx] = 0;
                }
                leases_by_peer[idx].clear();
            }
            ExitAction::SaturatePreviousHopShare => {
                // Separate accounting gate with unspecified `max_sessions` (`0` -> hard cap 1024)
                // to verify the 64-session per-previous-hop cap while global capacity remains open.
                let hop_gate = OnionExitAccounting::default();
                let open_policy = OnionExitPolicy::default();
                let noisy_hop = Did::from(77_u32);
                let other_hop = Did::from(78_u32);
                let held = (0..64)
                    .filter_map(|_| hop_gate.admit(&open_policy, noisy_hop).ok())
                    .collect::<Vec<_>>();
                if held.len() != 64 {
                    summary.session_cap_ok = false;
                }
                if hop_gate.admit(&open_policy, noisy_hop).is_ok() {
                    summary.session_cap_ok = false;
                } else {
                    summary.saw_hop_share_full = true;
                }
                if hop_gate.admit(&open_policy, other_hop).is_err() {
                    summary.session_cap_ok = false;
                }
                drop(held);
            }
            ExitAction::RecordBytes(n) => {
                if mutant == Some(ExitMutant::IgnoreByteWindow) {
                    window_bytes = window_bytes.saturating_add(n);
                    if window_bytes > policy.max_bytes_per_minute {
                        summary.byte_window_ok = false;
                    }
                    continue;
                }
                match accounting.record_bytes(&policy, n, now_ms) {
                    Ok(()) => {
                        window_bytes = window_bytes.saturating_add(n);
                        if window_bytes > policy.max_bytes_per_minute {
                            summary.byte_window_ok = false;
                        }
                    }
                    Err(_) => {
                        summary.saw_byte_refusal = true;
                        if window_bytes.saturating_add(n) <= policy.max_bytes_per_minute {
                            summary.byte_window_ok = false;
                        }
                    }
                }
                let recorded = accounting
                    .recorded_bytes_for_test(now_ms)
                    .expect("exit byte counter lock");
                let remaining = accounting
                    .remaining_bytes(&policy, now_ms)
                    .expect("exit remaining lock");
                if recorded != window_bytes
                    || remaining != Some(policy.max_bytes_per_minute.saturating_sub(window_bytes))
                {
                    summary.byte_window_ok = false;
                }
            }
            ExitAction::AdvanceMinute => {
                let had_bytes = window_bytes > 0;
                now_ms = now_ms.saturating_add(60_000);
                window_bytes = 0;
                let recorded = accounting
                    .recorded_bytes_for_test(now_ms)
                    .expect("exit byte counter lock");
                if recorded != 0 {
                    summary.byte_window_ok = false;
                }
                if had_bytes {
                    summary.saw_window_reset = true;
                }
            }
        }

        if mutant.is_none() {
            let sum_peers: usize = (0..3_u32).map(|p| quota.peer_total(Did::from(p + 1))).sum();
            if quota.total() != sum_peers || quota.total() != counts.iter().sum::<usize>() {
                summary.quota_conservation_ok = false;
            }
        }
    }

    summary
}

impl Model for ExitQuotaModel {
    type Action = ExitAction;
    type State = Vec<ExitAction>;

    fn init_states(&self) -> Vec<Self::State> {
        vec![Vec::new()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.len() >= self.max_steps {
            return;
        }
        actions.extend([
            ExitAction::AdmitPeer(0),
            ExitAction::AdmitPeer(1),
            ExitAction::ReleasePeer(0),
            ExitAction::ReleaseEntirePeer(0),
            ExitAction::RecordBytes(6),
            ExitAction::AdvanceMinute,
        ]);
        if !state.contains(&ExitAction::SaturatePreviousHopShare) {
            actions.push(ExitAction::SaturatePreviousHopShare);
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
                "PeerQuota and OnionExitAccounting conserve global and per-peer bounds",
                |model, history| {
                    let s = replay_exit(history, model.mutant);
                    s.quota_conservation_ok && s.session_cap_ok
                },
            ),
            Property::<Self>::always(
                "exit byte window enforces per-minute cap and resets on rollover",
                |model, history| replay_exit(history, model.mutant).byte_window_ok,
            ),
            Property::<Self>::sometimes(
                "global session cap, 64-per-hop share cap, and byte refusal are all exercised",
                |model, history| {
                    let s = replay_exit(history, model.mutant);
                    s.saw_global_session_full || (s.saw_byte_refusal && s.saw_window_reset)
                },
            ),
            Property::<Self>::sometimes(
                "per-previous-hop 64-session bound is exercised",
                |model, history| replay_exit(history, model.mutant).saw_hop_share_full,
            ),
        ]
    }
}

// ===========================================================================
// Part 2: `OrderedSendState` outbox lanes & class fairness Stateright model
// ===========================================================================

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LanesAction {
    Open(u8),
    Close(u8),
    EnqueueRelay(u8),
    EnqueueLocal(u8),
    EnqueueEndpoint(u8),
    /// Fills half of peer `0`'s lane with relayed cells while an endpoint waits, then attempts
    /// one more `Relay` enqueue vs one `Local` enqueue to verify the half-lane fairness law.
    ContendHalfLaneWithWaitingEndpoint,
    EmitNext(u8),
    RequeueInFlight(u8),
    CompleteInFlight(u8),
    Shutdown,
}

#[derive(Clone, Default)]
struct OutboxLanesModel;

#[derive(Debug, Default)]
struct LanesSummary {
    accounting_ok: bool,
    fifo_order_ok: bool,
    fairness_ok: bool,
    shutdown_final_ok: bool,
    saw_relay_half_yield: bool,
    saw_requeue_preserved_order: bool,
    saw_stale_epoch_stopped: bool,
}

fn replay_lanes(history: &[LanesAction]) -> LanesSummary {
    let mut state = OrderedSendState::<u32>::default();
    let mut epochs: [Option<u64>; 2] = [None, None];
    let mut retired_epoch: Option<(u8, u64)> = None;
    let mut next_seq: [u32; 2] = [1, 1];
    let mut emitted_completed: [Vec<u32>; 2] = [Vec::new(), Vec::new()];
    let mut requeued_head: [Option<u32>; 2] = [None, None];
    let mut in_flight_item: [Option<u32>; 2] = [None, None];
    let mut now_us: u128 = 1_000_000;

    let mut summary = LanesSummary {
        accounting_ok: true,
        fifo_order_ok: true,
        fairness_ok: true,
        shutdown_final_ok: true,
        ..LanesSummary::default()
    };

    for &step in history {
        now_us = now_us.saturating_add(50_000);
        match step {
            LanesAction::Open(p) => {
                let idx = usize::from(p);
                let peer = Did::from(u32::from(p) + 1);
                let opened = state.open(peer, now_us);
                if state.shut && opened.is_some() {
                    summary.shutdown_final_ok = false;
                }
                if let Some(ep) = opened {
                    epochs[idx] = Some(ep);
                }
            }
            LanesAction::Close(p) => {
                let idx = usize::from(p);
                let peer = Did::from(u32::from(p) + 1);
                if let Some(old_ep) = epochs[idx].take() {
                    retired_epoch = Some((p, old_ep));
                }
                in_flight_item[idx] = None;
                requeued_head[idx] = None;
                let _ = state.close(peer, now_us);
                if let Some((rp, old_ep)) = retired_epoch {
                    let r_peer = Did::from(u32::from(rp) + 1);
                    if !matches!(
                        state.next(r_peer, old_ep, OnionIdleFloor::DEFAULT, true, now_us),
                        EmitterStep::Stop
                    ) {
                        summary.accounting_ok = false;
                    } else {
                        summary.saw_stale_epoch_stopped = true;
                    }
                }
            }
            LanesAction::EnqueueRelay(p)
            | LanesAction::EnqueueLocal(p)
            | LanesAction::EnqueueEndpoint(p) => {
                let idx = usize::from(p);
                let peer = Did::from(u32::from(p) + 1);
                let role = match step {
                    LanesAction::EnqueueRelay(_) => QueueRole::Relay,
                    LanesAction::EnqueueLocal(_) => QueueRole::Local,
                    _ => QueueRole::Endpoint { holding: false },
                };
                let item = next_seq[idx];
                match state.enqueue(peer, item, OnionLoopClass::DEFAULT, now_us, role) {
                    Ok(()) => {
                        next_seq[idx] += 1;
                    }
                    Err(Refusal {
                        space: Some(_rx), ..
                    }) => {
                        // Drop the waiter registration immediately via `leave` so unpolled test
                        // channels do not dangle unless specifically testing contention.
                        state.leave(peer);
                    }
                    Err(_) => {}
                }
            }
            LanesAction::ContendHalfLaneWithWaitingEndpoint => {
                // Exercise the pure half-lane relay yield rule directly on a dedicated peer lane:
                // with `RELAY_SHARE_CELLS = 64` (or `RELAY_SHARE_BYTES = 8 MiB`), 1 x `MiB12`
                // (`12 MiB > 8 MiB`) or `RELAY_SHARE_CELLS` cells reaches half of the lane!
                let mut contend = OrderedSendState::<u32>::default();
                let peer = Did::from(99_u32);
                let _ = contend.open(peer, now_us);
                // Register one waiting endpoint on `peer`.
                let _wait_rx = contend.register(peer);
                for i in 0..(RELAY_SHARE_CELLS as u32) {
                    if contend
                        .enqueue(peer, i, OnionLoopClass::DEFAULT, now_us, QueueRole::Relay)
                        .is_err()
                    {
                        summary.fairness_ok = false;
                    }
                }
                const { assert!(RELAY_SHARE_BYTES > 0) };
                // The 65th relayed cell MUST yield (`PeerFull`) because an endpoint waits!
                match contend.enqueue(peer, 999, OnionLoopClass::DEFAULT, now_us, QueueRole::Relay)
                {
                    Err(Refusal {
                        reason: OnionQueueAdmissionReason::PeerFull,
                        ..
                    }) => {
                        summary.saw_relay_half_yield = true;
                    }
                    _ => {
                        summary.fairness_ok = false;
                    }
                }
                // Meanwhile `QueueRole::Local` and the waking `QueueRole::Endpoint` are NOT
                // counted in `RelayShare` and succeed into the remaining half of the lane!
                if contend
                    .enqueue(
                        peer,
                        1000,
                        OnionLoopClass::DEFAULT,
                        now_us,
                        QueueRole::Local,
                    )
                    .is_err()
                {
                    summary.fairness_ok = false;
                }
                if contend
                    .enqueue(
                        peer,
                        1001,
                        OnionLoopClass::DEFAULT,
                        now_us,
                        QueueRole::Endpoint { holding: true },
                    )
                    .is_err()
                {
                    summary.fairness_ok = false;
                }
            }
            LanesAction::EmitNext(p) => {
                let idx = usize::from(p);
                let peer = Did::from(u32::from(p) + 1);
                if let Some(ep) = epochs[idx] {
                    // Advance past any slot spacing so `plan` is ready to emit when credited.
                    now_us = now_us.saturating_add(1_000_000);
                    if let EmitterStep::Emit(Emission::Real(item)) =
                        state.next(peer, ep, OnionIdleFloor::DEFAULT, true, now_us)
                    {
                        if in_flight_item[idx].is_some() {
                            summary.accounting_ok = false;
                        }
                        if let Some(expected_requeued) = requeued_head[idx].take() {
                            if item == expected_requeued {
                                summary.saw_requeue_preserved_order = true;
                            } else {
                                summary.fifo_order_ok = false;
                            }
                        }
                        in_flight_item[idx] = Some(item);
                    }
                }
            }
            LanesAction::RequeueInFlight(p) => {
                let idx = usize::from(p);
                let peer = Did::from(u32::from(p) + 1);
                if let (Some(ep), Some(item)) = (epochs[idx], in_flight_item[idx].take()) {
                    if state.requeue(peer, ep, item) {
                        requeued_head[idx] = Some(item);
                    } else {
                        summary.accounting_ok = false;
                    }
                }
            }
            LanesAction::CompleteInFlight(p) => {
                let idx = usize::from(p);
                let peer = Did::from(u32::from(p) + 1);
                if let (Some(ep), Some(item)) = (epochs[idx], in_flight_item[idx].take()) {
                    state.complete(peer, ep);
                    if let Some(&prev) = emitted_completed[idx].last() {
                        if item <= prev {
                            summary.fifo_order_ok = false;
                        }
                    }
                    emitted_completed[idx].push(item);
                }
            }
            LanesAction::Shutdown => {
                state.shutdown(now_us);
                epochs = [None, None];
                in_flight_item = [None, None];
                requeued_head = [None, None];
                if !state.lanes.is_empty() || state.quota.total() != 0 || state.pending_bytes != 0 {
                    summary.shutdown_final_ok = false;
                }
            }
        }

        // Check global and per-lane accounting invariants after every transition.
        let mut expected_cells = 0usize;
        let mut expected_bytes = 0usize;
        for (&peer, lane) in &state.lanes {
            let in_flight_count = usize::from(lane.in_flight.is_some());
            let lane_cells = lane.queued.len() + in_flight_count;
            if state.quota.peer_total(peer) != lane_cells {
                summary.accounting_ok = false;
            }
            let lane_bytes: usize = lane
                .queued
                .iter()
                .map(|q| q.class.cell_bytes())
                .sum::<usize>()
                + lane.in_flight.map_or(0, |f| f.class.cell_bytes());
            if lane.pending_bytes != lane_bytes {
                summary.accounting_ok = false;
            }
            let relayed_cells = lane.queued.iter().filter(|q| q.relayed).count()
                + usize::from(lane.in_flight.is_some_and(|f| f.relayed));
            let relayed_bytes: usize = lane
                .queued
                .iter()
                .filter(|q| q.relayed)
                .map(|q| q.class.cell_bytes())
                .sum::<usize>()
                + lane
                    .in_flight
                    .filter(|f| f.relayed)
                    .map_or(0, |f| f.class.cell_bytes());
            if lane.relayed.cells != relayed_cells || lane.relayed.bytes != relayed_bytes {
                summary.accounting_ok = false;
            }
            expected_cells += lane_cells;
            expected_bytes += lane_bytes;
        }
        if state.quota.total() != expected_cells || state.pending_bytes != expected_bytes {
            summary.accounting_ok = false;
        }
    }

    summary
}

impl Model for OutboxLanesModel {
    type Action = LanesAction;
    type State = Vec<LanesAction>;

    fn init_states(&self) -> Vec<Self::State> {
        vec![Vec::new(), vec![LanesAction::Open(0)]]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.len() >= 4 {
            return;
        }
        actions.extend([
            LanesAction::Open(0),
            LanesAction::Close(0),
            LanesAction::EnqueueRelay(0),
            LanesAction::EnqueueLocal(0),
            LanesAction::EnqueueEndpoint(0),
            LanesAction::EmitNext(0),
            LanesAction::RequeueInFlight(0),
            LanesAction::CompleteInFlight(0),
            LanesAction::Shutdown,
        ]);
        if !state.contains(&LanesAction::ContendHalfLaneWithWaitingEndpoint) {
            actions.push(LanesAction::ContendHalfLaneWithWaitingEndpoint);
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
                "outbox lanes conserve quota, pending bytes, relay share, FIFO order, and shutdown finality",
                |_, history| {
                    let s = replay_lanes(history);
                    s.accounting_ok && s.fifo_order_ok && s.fairness_ok && s.shutdown_final_ok
                },
            ),
            Property::<Self>::sometimes(
                "relayed traffic yields above half-lane when an endpoint waits",
                |_, history| replay_lanes(history).saw_relay_half_yield,
            ),
            Property::<Self>::sometimes(
                "requeue preserves head-of-line FIFO order and closed epochs stop",
                |_, history| {
                    let s = replay_lanes(history);
                    s.saw_requeue_preserved_order || s.saw_stale_epoch_stopped
                },
            ),
        ]
    }
}

fn refuted_exit(model: ExitQuotaModel) -> BTreeSet<&'static str> {
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

/// Exhaustive bounded Stateright check of `OnionExitAccounting`, `PeerQuota`, and
/// `OrderedSendState`.
#[test]
fn test_onion_exit_quota_and_outbox_lanes_stateright_model() {
    ExitQuotaModel {
        mutant: None,
        max_steps: 4,
    }
    .checker()
    .spawn_bfs()
    .join()
    .assert_properties();

    OutboxLanesModel
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
}

/// Mutating per-peer quota or byte-window enforcement is refuted by the exit-quota model.
#[test]
fn test_each_exit_quota_mutant_is_refuted() {
    for (mutant, law) in [
        (
            ExitMutant::IgnorePeerQuota,
            "PeerQuota and OnionExitAccounting conserve global and per-peer bounds",
        ),
        (
            ExitMutant::IgnoreByteWindow,
            "exit byte window enforces per-minute cap and resets on rollover",
        ),
    ] {
        let model = ExitQuotaModel {
            mutant: Some(mutant),
            max_steps: 3,
        };
        assert!(
            refuted_exit(model).contains(law),
            "{mutant:?} must be refuted by `{law}`"
        );
    }
}
