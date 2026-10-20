//! Tests of the pure schedule and the queue algebra.

use std::time::Duration;

use rings_core::dht::Did;

use super::lanes::EmitterStep;
use super::lanes::OrderedSendState;
use super::lanes::QueueRole;
use super::lanes::Refusal;
use super::schedule::is_active;
use super::schedule::is_credited;
use super::schedule::plan;
use super::schedule::slot_micros;
use super::schedule::units_of;
use super::schedule::Emission;
use super::schedule::LinkClock;
use super::schedule::OnionIdleFloor;
use super::schedule::Plan;
use super::schedule::ONION_LINK_BUDGET_UNITS;
use super::schedule::ONION_LINK_DWELL_US;
use super::MAX_PENDING_ONION_SENDS_PER_PEER;
use super::MAX_PENDING_ONION_SEND_BYTES_PER_PEER;
use crate::error::Error;
use crate::error::OnionQueueAdmissionReason;
use crate::error::Result;
use crate::onion::circuit::OnionCellBucket;
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
        match plan(clock, queued > 0, true, floor, now_us) {
            Plan::Emit => {
                let real = queued > 0;
                queued = queued.saturating_sub(1);
                clock.last_start = Some((now_us, units));
                starts.push((now_us, real));
            }
            Plan::WaitCredit => unreachable!("the schedule is always credited"),
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

/// Rate (#913): the gate is per frame. A lane with room for one frame, or for any number below
/// the frames of the largest class, `f(12 MiB) = 192` at 64 KiB frames, admits a start; only an
/// exhausted lane, or no connection, holds it, so no class waits for a cell-sized window that the
/// lane's `W = 16` could never offer.
#[test]
fn test_the_credit_gate_needs_room_for_one_frame_only() {
    for free in [1, 2, 15, 16, 191] {
        assert!(is_credited(Some(free)), "{free} free frames admit a start");
    }
    assert!(!is_credited(Some(0)));
    assert!(!is_credited(None));

    let units = units_of(OnionLoopClass::DEFAULT);
    let clock = LinkClock {
        last_real_us: Some(0),
        last_start: Some((0, units)),
    };
    let due_us = slot_micros(units);
    let floor = OnionIdleFloor::DEFAULT;
    assert_eq!(
        plan(clock, true, is_credited(Some(1)), floor, due_us),
        Plan::Emit
    );
    assert_eq!(
        plan(clock, true, is_credited(Some(0)), floor, due_us),
        Plan::WaitCredit
    );
}

/// Credit (#913): with no credit the schedule emits nothing, due or not, and a credited slot
/// is decided as before, so credit only ever delays a start (Budget safety holds).
#[test]
fn test_no_credit_emits_nothing_and_credit_only_delays() {
    let floor = OnionIdleFloor::DEFAULT;
    let units = units_of(OnionLoopClass::DEFAULT);
    let clock = LinkClock {
        last_real_us: Some(0),
        last_start: Some((0, units)),
    };
    let due_us = slot_micros(units);

    assert_eq!(
        plan(clock, true, false, floor, 10 * due_us),
        Plan::WaitCredit
    );
    assert_eq!(
        plan(LinkClock::default(), true, false, floor, 0),
        Plan::WaitCredit
    );
    assert_eq!(
        plan(clock, true, true, floor, due_us - 1),
        Plan::WaitUntil(due_us)
    );
    assert_eq!(plan(clock, true, true, floor, 10 * due_us), Plan::Emit);
}

/// Volume hiding (#913): an uncredited lane takes no queued cell and arms its wake-up, and
/// once credited it emits its queued real cell exactly as it would have.
#[test]
fn test_an_uncredited_lane_holds_its_queue_until_credit_returns() {
    let peer = Did::from(14_u32);
    let class = OnionLoopClass::DEFAULT;
    let floor = OnionIdleFloor::DEFAULT;
    let mut state = OrderedSendState::<u32>::default();
    let epoch = state.open(peer, 0).expect("a new lane");
    assert!(state
        .enqueue(peer, 7, 1, class, 0, QueueRole::Relay)
        .is_ok());

    assert!(matches!(
        state.next(peer, epoch, floor, false, 0),
        EmitterStep::WaitCredit(_)
    ));
    assert_eq!(
        state.lanes.get(&peer).map(|lane| lane.queued.len()),
        Some(1)
    );
    assert!(matches!(
        state.next(peer, epoch, floor, true, 0),
        EmitterStep::Emit(Emission::Real(7))
    ));
}

/// Order: a real cell whose send never left goes back to the head of its lane
/// with its quota kept, and is the next real cell to leave.
#[test]
fn test_an_unsent_real_cell_is_requeued_at_the_head() {
    let peer = Did::from(15_u32);
    let class = OnionLoopClass::DEFAULT;
    let floor = OnionIdleFloor::DEFAULT;
    let mut state = OrderedSendState::<u32>::default();
    let epoch = state.open(peer, 0).expect("a new lane");
    for item in [1, 2] {
        assert!(state
            .enqueue(peer, item, 1, class, 0, QueueRole::Relay)
            .is_ok());
    }
    let EmitterStep::Emit(Emission::Real(unsent)) = state.next(peer, epoch, floor, true, 0) else {
        panic!("the first real cell");
    };

    assert!(state.requeue(peer, epoch, unsent));
    assert_eq!(state.quota.total(), 2, "its quota is kept");
    let slot_us = slot_micros(units_of(class));
    assert!(matches!(
        state.next(peer, epoch, floor, true, slot_us),
        EmitterStep::Emit(Emission::Real(1))
    ));
}

/// A node's own control cells never yield to relayed traffic: they are not in
/// the relay share, so a waiting endpoint does not refuse them.
#[test]
fn test_local_control_cells_are_not_in_the_relay_share() {
    let peer = Did::from(16_u32);
    let class = OnionLoopClass::DEFAULT;
    let endpoint = QueueRole::Endpoint { holding: false };
    let mut state = OrderedSendState::<usize>::default();
    state.open(peer, 0).expect("a new lane");
    for item in 0..super::RELAY_SHARE_CELLS {
        assert!(state
            .enqueue(peer, item, 1, class, 0, QueueRole::Relay)
            .is_ok());
    }
    for item in 0..MAX_PENDING_ONION_SENDS_PER_PEER - super::RELAY_SHARE_CELLS - 1 {
        assert!(state
            .enqueue(peer, 500 + item, 1, class, 0, endpoint)
            .is_ok());
    }
    assert!(state
        .enqueue(peer, 900, 1, class, 0, QueueRole::Local)
        .is_ok());
    let Err(Refusal { space: Some(_), .. }) = state.enqueue(peer, 901, 1, class, 0, endpoint)
    else {
        panic!("the full lane registers the endpoint");
    };
    assert!(matches!(
        state.enqueue(peer, 902, 1, class, 0, QueueRole::Relay),
        Err(Refusal { space: None, .. })
    ));
    assert_eq!(
        state.lanes.get(&peer).map(|lane| lane.relayed.cells),
        Some(super::RELAY_SHARE_CELLS),
        "the local cell is not counted as relayed"
    );
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
    let epoch = state.open(peer, 0).expect("a new lane");

    assert!(state
        .enqueue(peer, 1, 7, class, 0, QueueRole::Relay)
        .is_ok());
    assert!(state
        .enqueue(peer, 2, 7, class, 0, QueueRole::Relay)
        .is_ok());
    assert!(matches!(
        state.next(peer, epoch, floor, true, 0),
        EmitterStep::Emit(Emission::Real(1))
    ));
    // One slot later, with the first cell still in flight: cover.
    let slot_us = slot_micros(1);
    assert!(matches!(
        state.next(peer, epoch, floor, true, 1),
        EmitterStep::Wait(due, _) if due == slot_us
    ));
    assert!(matches!(
        state.next(peer, epoch, floor, true, slot_us),
        EmitterStep::Emit(Emission::Cover(_))
    ));
    state.complete(peer, epoch);
    assert!(matches!(
        state.next(peer, epoch, floor, true, 2 * slot_us),
        EmitterStep::Emit(Emission::Real(2))
    ));
    state.complete(peer, epoch);

    assert!(state.close(peer, 0).is_empty());
    assert!(matches!(
        state.next(peer, epoch, floor, true, 3 * slot_us),
        EmitterStep::Stop
    ));
    assert_eq!(state.quota.total(), 0);
    assert_eq!(state.pending_bytes, 0);
}

/// Lanes follow the links (#895 H1): a cell for a peer with no up link is refused and handed
/// back, and creates no lane.
#[test]
fn test_only_an_open_link_has_a_lane() {
    let peer = Did::from(6_u32);
    let mut state = OrderedSendState::<u32>::default();

    assert!(matches!(
        state.enqueue(peer, 7, 1, OnionLoopClass::DEFAULT, 0, QueueRole::Relay),
        Err(Refusal {
            reason: OnionQueueAdmissionReason::LinkDown,
            item: 7,
            ..
        })
    ));
    assert!(state.lanes.is_empty());
    assert!(state.open(peer, 0).is_some());
    assert!(state.open(peer, 0).is_none());
    assert!(state
        .enqueue(peer, 8, 1, OnionLoopClass::DEFAULT, 0, QueueRole::Relay)
        .is_ok());
}

/// A reopened lane has a new epoch (#895 H3): the emitter of the closed lane meets `Stop`,
/// so a lane never has two emitters.
#[test]
fn test_an_emitter_of_a_closed_lane_stops_on_its_reopened_successor() {
    let peer = Did::from(7_u32);
    let floor = OnionIdleFloor::DEFAULT;
    let mut state = OrderedSendState::<u32>::default();
    let first = state.open(peer, 0).expect("a new lane");
    state.close(peer, 0);
    let second = state.open(peer, 0).expect("a new lane");

    assert_ne!(first, second);
    assert!(matches!(
        state.next(peer, first, floor, true, 0),
        EmitterStep::Stop
    ));
    assert!(matches!(
        state.next(peer, second, floor, true, 0),
        EmitterStep::Emit(Emission::Cover(_))
    ));
}

/// A real emission is charged its own class (#895 H2): a large cell queued before a small
/// one delays the next start by the large cell's slot, whatever was enqueued last.
#[test]
fn test_a_real_emission_is_charged_its_own_class() -> Result<()> {
    let peer = Did::from(8_u32);
    let large = OnionLoopClass::from(OnionCellBucket::MiB1);
    let floor = OnionIdleFloor::DEFAULT;
    let mut state = OrderedSendState::default();
    let epoch = state.open(peer, 0).ok_or(Error::InvalidData)?;
    state
        .enqueue(peer, 1, large.cell_bytes(), large, 0, QueueRole::Relay)
        .map_err(|_| Error::InvalidData)?;
    state
        .enqueue(peer, 2, 1, OnionLoopClass::DEFAULT, 0, QueueRole::Relay)
        .map_err(|_| Error::InvalidData)?;

    assert!(matches!(
        state.next(peer, epoch, floor, true, 0),
        EmitterStep::Emit(Emission::Real(1))
    ));
    state.complete(peer, epoch);
    let large_slot = slot_micros(units_of(large));
    assert!(matches!(
        state.next(peer, epoch, floor, true, slot_micros(1)),
        EmitterStep::Wait(due, _) if due == large_slot
    ));
    assert!(matches!(
        state.next(peer, epoch, floor, true, large_slot),
        EmitterStep::Emit(Emission::Real(2))
    ));
    Ok(())
}

/// No lost wake-up: an enqueue during the emitter's wait fires its wake-up, and the cell is
/// due one active slot after the last emission.
#[test]
fn test_an_enqueue_wakes_the_waiting_emitter() {
    let peer = Did::from(5_u32);
    let class = OnionLoopClass::DEFAULT;
    let floor = OnionIdleFloor::DEFAULT;
    let mut state = OrderedSendState::<u32>::default();
    let epoch = state.open(peer, 0).expect("a new lane");
    assert!(matches!(
        state.next(peer, epoch, floor, true, 0),
        EmitterStep::Emit(Emission::Cover(_))
    ));
    let EmitterStep::Wait(due, mut woken) = state.next(peer, epoch, floor, true, 1) else {
        panic!("an idle link waits for its floor");
    };
    assert_eq!(due, floor.slot_micros(1));

    assert!(state
        .enqueue(peer, 9, 1, class, 20_000, QueueRole::Relay)
        .is_ok());
    assert_eq!(woken.try_recv(), Ok(Some(())));
    assert!(matches!(
        state.next(peer, epoch, floor, true, 20_000),
        EmitterStep::Emit(Emission::Real(9))
    ));
}

/// A full lane registers an endpoint under the same lock as its refusal, and the next
/// completion wakes exactly that one waiter; while it waits, relays holding half of the lane
/// yield it.
#[test]
fn test_a_completion_wakes_one_waiting_endpoint_and_relays_yield_beyond_half() {
    let peer = Did::from(10_u32);
    let floor = OnionIdleFloor::DEFAULT;
    let class = OnionLoopClass::DEFAULT;
    let endpoint = QueueRole::Endpoint { holding: false };
    let mut state = OrderedSendState::<usize>::default();
    let epoch = state.open(peer, 0).expect("a new lane");
    for item in 0..MAX_PENDING_ONION_SENDS_PER_PEER {
        assert!(state
            .enqueue(peer, item, 1, class, 0, QueueRole::Relay)
            .is_ok());
    }
    let Err(Refusal {
        reason: OnionQueueAdmissionReason::PeerFull,
        space: Some(mut first),
        ..
    }) = state.enqueue(peer, 900, 1, class, 0, endpoint)
    else {
        panic!("a full lane registers the endpoint");
    };
    let Err(Refusal {
        space: Some(mut second),
        ..
    }) = state.enqueue(peer, 901, 1, class, 0, endpoint)
    else {
        panic!("a second endpoint registers too");
    };

    assert!(matches!(
        state.next(peer, epoch, floor, true, 0),
        EmitterStep::Emit(Emission::Real(0))
    ));
    state.complete(peer, epoch);
    assert_eq!(first.try_recv(), Ok(Some(())));
    assert_eq!(second.try_recv(), Ok(None), "one place wakes one waiter");
    assert!(matches!(
        state.enqueue(peer, 999, 1, class, 0, QueueRole::Relay),
        Err(Refusal {
            reason: OnionQueueAdmissionReason::PeerFull,
            space: None,
            ..
        })
    ));
    assert!(state
        .enqueue(peer, 900, 1, class, 0, QueueRole::Endpoint {
            holding: true
        })
        .is_ok());
}

/// Fairness: a waiting endpoint does not veto relays below half of the lane,
/// so relayed traffic toward a busy endpoint's link is never blackholed.
#[test]
fn test_relays_below_half_keep_their_share_while_an_endpoint_waits() {
    let peer = Did::from(13_u32);
    let floor = OnionIdleFloor::DEFAULT;
    let class = OnionLoopClass::DEFAULT;
    let endpoint = QueueRole::Endpoint { holding: false };
    let relays = 10;
    let mut state = OrderedSendState::<usize>::default();
    let epoch = state.open(peer, 0).expect("a new lane");
    for item in 0..MAX_PENDING_ONION_SENDS_PER_PEER - relays {
        assert!(state.enqueue(peer, item, 1, class, 0, endpoint).is_ok());
    }
    for item in 0..relays {
        assert!(state
            .enqueue(peer, 1_000 + item, 1, class, 0, QueueRole::Relay)
            .is_ok());
    }
    let Err(Refusal {
        space: Some(mut woken),
        ..
    }) = state.enqueue(peer, 900, 1, class, 0, endpoint)
    else {
        panic!("a full lane registers the endpoint");
    };

    assert!(matches!(
        state.next(peer, epoch, floor, true, 0),
        EmitterStep::Emit(Emission::Real(0))
    ));
    state.complete(peer, epoch);
    assert_eq!(woken.try_recv(), Ok(Some(())));
    assert!(
        state
            .enqueue(peer, 999, 1, class, 0, QueueRole::Relay)
            .is_ok(),
        "relays hold less than half, so the freed place is theirs to take"
    );
    assert_eq!(
        state.lanes.get(&peer).map(|lane| lane.relayed.cells),
        Some(11)
    );
}

/// Shutdown is terminal: every lane closes and no link fact opens one again.
#[test]
fn test_shutdown_closes_every_lane_and_opens_none_again() {
    let mut state = OrderedSendState::<u32>::default();
    let peer = Did::from(11_u32);
    let epoch = state.open(peer, 0).expect("a new lane");

    state.shutdown(0);
    assert!(matches!(
        state.next(peer, epoch, OnionIdleFloor::DEFAULT, true, 0),
        EmitterStep::Stop
    ));
    assert_eq!(state.open(peer, 0), None);
    assert!(state.lanes.is_empty());
}

/// A lane reopened within `V` keeps its emission clock: its first emission is
/// due a slot after the last one of the closed lane, not at once.
#[test]
fn test_a_reopened_lane_keeps_its_emission_clock() {
    let peer = Did::from(12_u32);
    let floor = OnionIdleFloor::DEFAULT;
    let mut state = OrderedSendState::<u32>::default();
    let first = state.open(peer, 0).expect("a new lane");
    assert!(matches!(
        state.next(peer, first, floor, true, 0),
        EmitterStep::Emit(Emission::Cover(_))
    ));
    state.close(peer, 0);
    let second = state.open(peer, 1).expect("a new lane");

    assert!(matches!(
        state.next(peer, second, floor, true, 1),
        EmitterStep::Wait(due, _) if due == floor.slot_micros(1)
    ));
    state.close(peer, 0);
    let third = state
        .open(peer, ONION_FORWARD_MAX_VALIDITY_MS * 1_000)
        .expect("a new lane");
    assert!(matches!(
        state.next(
            peer,
            third,
            floor,
            true,
            ONION_FORWARD_MAX_VALIDITY_MS * 1_000
        ),
        EmitterStep::Emit(Emission::Cover(_))
    ));
}

/// Cover takes the class of the link's last real cell.
#[test]
fn test_cover_takes_the_class_of_the_last_real_cell() -> Result<()> {
    let peer = Did::from(2_u32);
    let large = OnionLoopClass::from(OnionCellBucket::MiB1);
    let floor = OnionIdleFloor::DEFAULT;
    let mut state = OrderedSendState::default();
    let epoch = state.open(peer, 0).ok_or(Error::InvalidData)?;
    state
        .enqueue(peer, 1, 1, large, 0, QueueRole::Relay)
        .map_err(|_| Error::InvalidData)?;
    assert!(matches!(
        state.next(peer, epoch, floor, true, 0),
        EmitterStep::Emit(Emission::Real(1))
    ));
    state.complete(peer, epoch);
    assert!(matches!(
        state.next(peer, epoch, floor, true, slot_micros(units_of(large))),
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
    state.open(peer, 0);
    state.open(other, 0);
    for value in 0..MAX_PENDING_ONION_SENDS_PER_PEER {
        assert!(state
            .enqueue(peer, value, 1, class, 0, QueueRole::Relay)
            .is_ok());
    }
    assert!(matches!(
        state.enqueue(peer, 200, 1, class, 0, QueueRole::Relay),
        Err(Refusal {
            reason: OnionQueueAdmissionReason::PeerFull,
            item: 200,
            ..
        })
    ));
    assert!(state
        .enqueue(other, 201, 1, class, 0, QueueRole::Relay)
        .is_ok());

    let mut global = OrderedSendState::default();
    for peer_id in 20_u32..25 {
        global.open(Did::from(peer_id), 0);
    }
    for peer_id in 20_u32..24 {
        assert!(global
            .enqueue(
                Did::from(peer_id),
                peer_id,
                MAX_PENDING_ONION_SEND_BYTES_PER_PEER,
                class,
                0,
                QueueRole::Relay,
            )
            .is_ok());
    }
    assert!(matches!(
        global.enqueue(Did::from(24_u32), 24, 1, class, 0, QueueRole::Relay),
        Err(Refusal {
            reason: OnionQueueAdmissionReason::GlobalFull,
            item: 24,
            ..
        })
    ));
    assert_eq!(global.close(Did::from(20_u32), 0), vec![20]);
    assert!(global
        .enqueue(Did::from(24_u32), 24, 1, class, 0, QueueRole::Relay)
        .is_ok());
}
