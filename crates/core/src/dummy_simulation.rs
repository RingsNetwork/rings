//! Cross-crate deterministic clock and entropy seam for `dummy`-transport tests.
//!
//! Unlike `crate::simulation` (which is restricted to `rings-core` unit tests), this module is
//! compiled whenever `feature = "dummy"` is enabled on native targets so `rings-node` integration
//! and session tests can drive protocol time and randomness without wall-clock sleeps.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::RngCore;
use rand::SeedableRng;

use crate::utils::Clock;
use crate::utils::ManualClock;
use crate::utils::SharedClock;
use crate::utils::SharedRng;

const UUID_VERSION_4: u8 = 0x40;
const UUID_RFC4122_VARIANT: u8 = 0x80;

struct DummySimulationState {
    clock: ManualClock,
    rng: SharedRng,
    ambient_rng: StdRng,
}

impl DummySimulationState {
    fn new(seed: u64, initial_epoch_ms: u128) -> Self {
        let mixed = rings_transport::connections::dummy_controlled::mix_seed(seed);
        Self {
            clock: ManualClock::new(initial_epoch_ms),
            rng: SharedRng::seed_from_u64(seed),
            ambient_rng: StdRng::seed_from_u64(mixed),
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<DummySimulationState>> = const { RefCell::new(None) };
}

/// Errors returned when installing or advancing a [`DummySimulationGuard`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DummySimulationError {
    /// Another [`DummySimulationGuard`] is already active on the current thread.
    #[error("a dummy simulation guard is already active on this thread")]
    AlreadyActive,
    /// Advancing the simulated epoch timestamp would overflow `u128`.
    #[error("dummy simulation clock overflowed")]
    TimeOverflow,
}

/// Thread-local deterministic clock and RNG guard for native `dummy` tests.
///
/// Installing this guard overrides [`crate::utils::get_epoch_ms`], [`crate::utils::SystemEntropy`],
/// and UUID generation on the current thread while also exposing explicit [`SharedClock`] and
/// [`SharedRng`] handles that can be injected into builders and protocol drivers.
pub struct DummySimulationGuard {
    clock: ManualClock,
    rng: SharedRng,
    _current_thread: PhantomData<Rc<()>>,
}

impl DummySimulationGuard {
    /// Install a deterministic simulation seam on the current thread.
    pub fn enter(seed: u64, initial_epoch_ms: u128) -> Result<Self, DummySimulationError> {
        STATE.with(|slot| {
            let mut borrow = slot.borrow_mut();
            if borrow.is_some() {
                return Err(DummySimulationError::AlreadyActive);
            }
            let state = DummySimulationState::new(seed, initial_epoch_ms);
            let clock = state.clock.clone();
            let rng = state.rng.clone();
            *borrow = Some(state);
            Ok(Self {
                clock,
                rng,
                _current_thread: PhantomData,
            })
        })
    }

    /// Return the current simulated Unix epoch timestamp in milliseconds.
    pub fn now_ms(&self) -> u128 {
        self.clock.now_ms()
    }

    /// Set the simulated Unix epoch timestamp in milliseconds.
    pub fn set_epoch_ms(&self, now_ms: u128) {
        self.clock.set(now_ms);
    }

    /// Advance the simulated Unix epoch timestamp by `delta_ms` milliseconds without touching
    /// Tokio timers.
    pub fn advance_ms(&self, delta_ms: u128) -> Result<u128, DummySimulationError> {
        self.clock
            .checked_advance(delta_ms)
            .ok_or(DummySimulationError::TimeOverflow)
    }

    /// Advance both the simulated Unix epoch clock and Tokio's paused timer wheel by `duration`.
    pub async fn advance(&self, duration: Duration) -> Result<u128, DummySimulationError> {
        let now_ms = self.advance_ms(duration.as_millis())?;
        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
        Ok(now_ms)
    }

    /// Return the manual clock backing this guard as a [`SharedClock`].
    pub fn shared_clock(&self) -> SharedClock {
        Arc::new(self.clock.clone())
    }

    /// Return the manual clock backing this guard.
    pub fn manual_clock(&self) -> ManualClock {
        self.clock.clone()
    }

    /// Return the seeded [`SharedRng`] backing this guard.
    pub fn shared_rng(&self) -> SharedRng {
        self.rng.clone()
    }
}

impl Drop for DummySimulationGuard {
    fn drop(&mut self) {
        STATE.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

/// Return the overridden Unix epoch timestamp in milliseconds when a [`DummySimulationGuard`] is
/// active on the current thread.
pub fn epoch_ms_override() -> Option<u128> {
    STATE.with(|slot| slot.borrow().as_ref().map(|state| state.clock.now_ms()))
}

pub(crate) fn try_fill_entropy_bytes(dest: &mut [u8]) -> bool {
    STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(state) = borrow.as_mut() else {
            return false;
        };
        state.ambient_rng.fill_bytes(dest);
        true
    })
}

pub(crate) fn next_uuid_override() -> Option<uuid::Uuid> {
    let mut bytes = [0_u8; 16];
    if !try_fill_entropy_bytes(&mut bytes) {
        return None;
    }
    let [_, _, _, _, _, _, ref mut version, _, ref mut variant, ..] = bytes;
    *version = (*version & 0x0f) | UUID_VERSION_4;
    *variant = (*variant & 0x3f) | UUID_RFC4122_VARIANT;
    Some(uuid::Uuid::from_bytes(bytes))
}
