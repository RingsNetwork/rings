use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::utils::get_epoch_ms;

use super::circuit::OnionCircuitId;
use super::OnionExitPolicy;
use crate::error::Error;
use crate::error::Result;

const EXIT_LIMIT_WINDOW_MS: u128 = 60_000;

/// Shared accounting gate for onion exits.
///
/// Invariant: `active_circuits == count({ circuit | active_streams_by_circuit[circuit] > 0 })`.
/// Invariant: `bytes_this_window <= policy.max_bytes_per_minute` whenever that policy field is
/// non-zero.
/// Preservation: `admit` checks active counters and byte budget under one lock before committing any
/// stream/circuit increment; dropping the returned lease decrements the same circuit key;
/// `record_bytes` resets stale windows before adding.
/// Post: `remaining_bytes` returns the exact bytes that may still be recorded in the current window,
/// or `None` when the byte policy is unlimited.
#[derive(Clone, Default)]
pub(crate) struct OnionExitAccounting {
    limiter: Arc<Mutex<ExitLimiter>>,
}

#[derive(Default)]
struct ExitLimiter {
    active_circuits: u32,
    active_streams_by_circuit: HashMap<ExitCircuitKey, u32>,
    window_start_ms: u128,
    bytes_this_window: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExitCircuitKey {
    circuit_id: OnionCircuitId,
    return_peer: Did,
}

impl ExitCircuitKey {
    const fn new(circuit_id: OnionCircuitId, return_peer: Did) -> Self {
        Self {
            circuit_id,
            return_peer,
        }
    }
}

/// Lease for one admitted exit stream/request.
pub(crate) struct OnionExitLease {
    limiter: Arc<Mutex<ExitLimiter>>,
    circuit: ExitCircuitKey,
}

impl Drop for OnionExitLease {
    fn drop(&mut self) {
        if let Ok(mut limiter) = self.limiter.lock() {
            if let Some(active_streams) = limiter.active_streams_by_circuit.get_mut(&self.circuit) {
                if *active_streams > 1 {
                    *active_streams -= 1;
                } else {
                    limiter.active_streams_by_circuit.remove(&self.circuit);
                    limiter.active_circuits = limiter.active_circuits.saturating_sub(1);
                }
            }
        }
    }
}

impl OnionExitAccounting {
    /// Admit one exit stream or request under `policy`.
    pub(crate) fn admit(
        &self,
        policy: &OnionExitPolicy,
        circuit_id: OnionCircuitId,
        return_peer: Did,
        bytes: u64,
    ) -> Result<OnionExitLease> {
        let circuit = ExitCircuitKey::new(circuit_id, return_peer);
        let mut limiter = self.limiter.lock().map_err(|_| Error::Lock)?;
        limiter.refresh_byte_window(get_epoch_ms());
        let active_streams = limiter
            .active_streams_by_circuit
            .get(&circuit)
            .copied()
            .unwrap_or_default();
        if policy.max_streams_per_circuit > 0 && active_streams >= policy.max_streams_per_circuit {
            return Err(Error::NoPermission);
        }
        if active_streams == 0
            && policy.max_circuits > 0
            && limiter.active_circuits >= policy.max_circuits
        {
            return Err(Error::NoPermission);
        }
        let next_bytes = limiter.next_recorded_bytes(policy, bytes)?;
        if active_streams == 0 {
            limiter.active_circuits = limiter.active_circuits.saturating_add(1);
        }
        limiter
            .active_streams_by_circuit
            .insert(circuit.clone(), active_streams.saturating_add(1));
        if let Some(next_bytes) = next_bytes {
            limiter.bytes_this_window = next_bytes;
        }
        Ok(OnionExitLease {
            limiter: self.limiter.clone(),
            circuit,
        })
    }

    /// Record exit payload bytes under the per-minute policy window.
    pub(crate) fn record_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<()> {
        if policy.max_bytes_per_minute == 0 || bytes == 0 {
            return Ok(());
        }
        let mut limiter = self.limiter.lock().map_err(|_| Error::Lock)?;
        limiter.refresh_byte_window(get_epoch_ms());
        if let Some(next) = limiter.next_recorded_bytes(policy, bytes)? {
            limiter.bytes_this_window = next;
        }
        Ok(())
    }

    /// Return bytes still available in the current per-minute window.
    #[cfg(feature = "browser")]
    pub(crate) fn remaining_bytes(&self, policy: &OnionExitPolicy) -> Result<Option<u64>> {
        if policy.max_bytes_per_minute == 0 {
            return Ok(None);
        }
        let mut limiter = self.limiter.lock().map_err(|_| Error::Lock)?;
        limiter.refresh_byte_window(get_epoch_ms());
        Ok(Some(
            policy
                .max_bytes_per_minute
                .saturating_sub(limiter.bytes_this_window),
        ))
    }
}

impl ExitLimiter {
    fn refresh_byte_window(&mut self, now_ms: u128) {
        if now_ms.saturating_sub(self.window_start_ms) >= EXIT_LIMIT_WINDOW_MS {
            self.window_start_ms = now_ms;
            self.bytes_this_window = 0;
        }
    }

    fn next_recorded_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<Option<u64>> {
        if policy.max_bytes_per_minute == 0 || bytes == 0 {
            return Ok(None);
        }
        let next = self.bytes_this_window.saturating_add(bytes);
        if next > policy.max_bytes_per_minute {
            return Err(Error::NoPermission);
        }
        Ok(Some(next))
    }
}
