//! The pure emission schedule of one link: the slot, the idle floor and [`plan`].

use std::time::Duration;

use super::super::ONION_ADMISSION_SENDER_UNITS;
use super::super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use super::super::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::onion::sphinx::class::OnionLoopClass;

/// The receiver's per-sender budget `B` in units (#834 L9).
pub(super) const ONION_LINK_BUDGET_UNITS: u128 = ONION_ADMISSION_SENDER_UNITS as u128;

/// `ρ = 9/10`, as numerator and denominator: the margin below the budget that covers the
/// receiver's window alignment and a clock skew between the two ends.
pub(super) const EMISSION_RATE_NUMERATOR: u128 = 9;
/// See [`EMISSION_RATE_NUMERATOR`].
pub(super) const EMISSION_RATE_DENOMINATOR: u128 = 10;

/// `slot(u) = u / r = u·V / (ρ·B)`, in microseconds: the emission clock granularity is one
/// microsecond, and the slot is rounded up, which only lowers the rate.
pub(super) const fn slot_micros(units: u128) -> u128 {
    (units * ONION_FORWARD_MAX_VALIDITY_MS * 1_000 * EMISSION_RATE_DENOMINATOR)
        .div_ceil(EMISSION_RATE_NUMERATOR * ONION_LINK_BUDGET_UNITS)
}

/// `u_max = u(12 MiB) = 768`, the units of the largest class.
pub(super) const ONION_LINK_MAX_UNITS: u128 = OnionLoopClass::MiB12.units().get() as u128;

// Budget safety at the largest class: `ρ·B + u_max < B`.
const _: () = assert!(
    ONION_LINK_BUDGET_UNITS * EMISSION_RATE_NUMERATOR / EMISSION_RATE_DENOMINATOR
        + ONION_LINK_MAX_UNITS
        < ONION_LINK_BUDGET_UNITS
);

/// `V` in microseconds: how long a closed lane's clock is kept for a reopen.
pub(super) const WINDOW_US: u128 = ONION_FORWARD_MAX_VALIDITY_MS * 1_000;

/// `D = Q`, the dwell: an active link stays active for `D` after its last real cell.
pub(super) const ONION_LINK_DWELL_US: u128 = ONION_FORWARD_EXPIRY_QUANTUM_MS * 1_000;

/// The idle floor `r_idle`: the cover rate of an up link that carries no real cell, as the
/// period of one unit. The default is one unit per second; a node may lower the rate (a longer
/// period, for a browser), never raise it, so `r_idle ≤ 1 unit/s < r` by construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionIdleFloor {
    /// `1 / r_idle` in microseconds per unit.
    micros_per_unit: u128,
}

impl OnionIdleFloor {
    /// `r_idle = 1` unit/s (#880).
    pub const DEFAULT: Self = Self {
        micros_per_unit: 1_000_000,
    };

    /// The floor of one unit per `period`, or `None` if `period` is shorter than one second:
    /// the floor is configurable downwards only.
    pub fn per_unit(period: Duration) -> Option<Self> {
        let micros_per_unit = period.as_micros();
        (micros_per_unit >= Self::DEFAULT.micros_per_unit).then_some(Self { micros_per_unit })
    }

    /// The idle slot after `units` units: `units / r_idle`.
    pub(super) const fn slot_micros(self, units: u128) -> u128 {
        units * self.micros_per_unit
    }
}

impl Default for OnionIdleFloor {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a link's emitter sends in a slot.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Emission<T> {
    /// Send this queued real cell.
    Real(T),
    /// Send a uniform cover cell of this class.
    Cover(OnionLoopClass),
}

/// The emission clock of one link: what the pure schedule [`plan`] reads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct LinkClock {
    /// When the last real cell was enqueued, in microseconds on the sender's clock.
    pub(super) last_real_us: Option<u128>,
    /// When the last emission started, and its units.
    pub(super) last_start: Option<(u128, u128)>,
}

/// The schedule's answer at one instant.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Plan {
    /// Emit now: a real cell if one is queued, else cover.
    Emit,
    /// Emit nothing until the link's receiver returns credit.
    WaitCredit,
    /// Emit nothing before this instant (microseconds).
    WaitUntil(u128),
}

/// Whether a link is active at `now`: a real cell is queued, or the last one was enqueued less
/// than `D` ago.
pub(super) const fn is_active(clock: LinkClock, queued: bool, now_us: u128) -> bool {
    queued
        || match clock.last_real_us {
            Some(last_real_us) => now_us < last_real_us + ONION_LINK_DWELL_US,
            None => false,
        }
}

/// Whether a link may start an emission: its lane has room for the cell's first frame,
/// `free(l) ≥ 1`, where `free` is `None` without a connection (see the module's Rate law for why
/// the gate is per frame, not per cell).
pub(super) const fn is_credited(free: Option<u64>) -> bool {
    matches!(free, Some(free) if free > 0)
}

/// The pure schedule of one up link (#880 option C), with `credited = free(l) ≥ 1` read from
/// the link's transport credit view (#913):
///
/// ```text
/// active(now) = queued ∨ now < last_real + D
/// due(now)    = last_start + (active(now) ? u/r : u/r_idle)      (0 before the first emission)
/// plan(now)   = ¬credited ⇒ WaitCredit;  now ≥ due(now) ? Emit : WaitUntil(due(now))
/// ```
///
/// Laws: every emission starts at least `u/r` after the previous one (`u/r_idle ≥ u/r`), so the
/// budget bound of the module holds across phase changes, and a wait for credit only delays the
/// next start; while active and credited the rate is `r`, while idle it is `r_idle`, so an
/// up link with credit emits at `≥ r_idle`; and an active period, begun by a real cell at `t`,
/// lasts at least until `t + D`.
pub(super) fn plan(
    clock: LinkClock,
    queued: bool,
    credited: bool,
    floor: OnionIdleFloor,
    now_us: u128,
) -> Plan {
    if !credited {
        return Plan::WaitCredit;
    }
    let Some((start_us, units)) = clock.last_start else {
        return Plan::Emit;
    };
    let slot_us = if is_active(clock, queued, now_us) {
        slot_micros(units)
    } else {
        floor.slot_micros(units)
    };
    let due_us = start_us + slot_us;
    if now_us >= due_us {
        Plan::Emit
    } else {
        Plan::WaitUntil(due_us)
    }
}
