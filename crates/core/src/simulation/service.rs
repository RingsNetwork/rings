//! Deterministic service costs consumed inside production inbound work.

use std::time::Duration;

use super::verify_effect_boundary;
use super::with_runtime;
use super::with_runtime_mut;
use super::SimulationRuntimeError;
use super::SimulationRuntimeGuard;

const REASSEMBLY_BYTES_PER_MS: usize = 32;
const MAX_REASSEMBLY_FRAME_SERVICE_MS: u64 = 2_000;

/// Bounded virtual service cost for one real reassembly frame.
pub(crate) fn reassembly_frame_service_ms(bytes: usize) -> u64 {
    u64::try_from(bytes.max(1).div_ceil(REASSEMBLY_BYTES_PER_MS))
        .unwrap_or(MAX_REASSEMBLY_FRAME_SERVICE_MS)
        .min(MAX_REASSEMBLY_FRAME_SERVICE_MS)
}

impl SimulationRuntimeGuard {
    /// Charge real reassembly callbacks a deterministic virtual service cost.
    pub(crate) fn enable_reassembly_service(&self) -> Result<(), SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| runtime.reassembly_service_enabled = true)
    }

    /// Stop charging reassembly callbacks after the bounded pressure witness.
    pub(crate) fn disable_reassembly_service(&self) -> Result<(), SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| runtime.reassembly_service_enabled = false)
    }
}

/// Await the configured service cost from inside the production actor future.
pub(crate) async fn wait_reassembly_service(bytes: usize) {
    let enabled = with_runtime(|runtime| runtime.reassembly_service_enabled).unwrap_or(false);
    if enabled {
        crate::utils::sleep(Duration::from_millis(reassembly_frame_service_ms(bytes))).await;
    }
}
