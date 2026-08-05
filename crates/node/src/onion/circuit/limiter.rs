use std::collections::BTreeMap;
use std::sync::Mutex;

use rings_core::dht::Did;

use super::MAX_ONION_CRYPTO_BYTES_GLOBAL_PER_WINDOW;
use super::MAX_ONION_CRYPTO_BYTES_PER_WINDOW;
use super::MAX_ONION_CRYPTO_OPS_GLOBAL_PER_WINDOW;
use super::MAX_ONION_CRYPTO_OPS_PER_WINDOW;
use super::MAX_ONION_CRYPTO_PEERS;
use super::ONION_CRYPTO_LIMIT_WINDOW_MS;
use crate::error::Error;
use crate::error::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CryptoBudget {
    operations: u32,
    visible_cell_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CryptoLimits {
    per_peer: CryptoBudget,
    global: CryptoBudget,
    peers: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CryptoWindow {
    window_start_ms: u128,
    used_ops: u32,
    used_bytes: u64,
    last_admitted_ms: u128,
}

/// Pure per-peer crypto admission windows.
///
/// Invariant: for every active `from`, operation and visible cell-byte charges remain within
/// their configured limits inside the half-open
/// interval `[window_start_ms, window_start_ms + ONION_CRYPTO_LIMIT_WINDOW_MS)`.
/// Preservation: `admit` removes expired evidence, computes both global and peer transitions
/// before committing either budget, and evicts only the least-recently-admitted peer when the
/// witness set is full. Identity-independent global limits make that fairness eviction unable to
/// reset the node-wide
/// crypto or bandwidth budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OnionCryptoLimiter {
    limits: CryptoLimits,
    global: CryptoWindow,
    windows: BTreeMap<Did, CryptoWindow>,
}

impl Default for OnionCryptoLimiter {
    fn default() -> Self {
        Self::with_limits(CryptoLimits {
            per_peer: CryptoBudget {
                operations: MAX_ONION_CRYPTO_OPS_PER_WINDOW,
                visible_cell_bytes: MAX_ONION_CRYPTO_BYTES_PER_WINDOW,
            },
            global: CryptoBudget {
                operations: MAX_ONION_CRYPTO_OPS_GLOBAL_PER_WINDOW,
                visible_cell_bytes: MAX_ONION_CRYPTO_BYTES_GLOBAL_PER_WINDOW,
            },
            peers: MAX_ONION_CRYPTO_PEERS,
        })
    }
}

impl OnionCryptoLimiter {
    #[cfg(test)]
    pub(super) fn with_limit(max_ops_per_window: u32) -> Self {
        Self::with_limits(CryptoLimits {
            per_peer: CryptoBudget {
                operations: max_ops_per_window,
                visible_cell_bytes: u64::MAX,
            },
            global: CryptoBudget {
                operations: u32::MAX,
                visible_cell_bytes: u64::MAX,
            },
            peers: usize::MAX,
        })
    }

    fn with_limits(limits: CryptoLimits) -> Self {
        Self {
            limits,
            global: CryptoWindow {
                window_start_ms: 0,
                used_ops: 0,
                used_bytes: 0,
                last_admitted_ms: 0,
            },
            windows: BTreeMap::new(),
        }
    }

    pub(super) fn admit(&mut self, from: Did, now_ms: u128, visible_cell_bytes: u64) -> Result<()> {
        if self.limits.per_peer.operations == 0 || self.limits.global.operations == 0 {
            return Ok(());
        }
        self.windows.retain(|_, window| {
            now_ms.saturating_sub(window.window_start_ms) < ONION_CRYPTO_LIMIT_WINDOW_MS
        });
        let new_peer = !self.windows.contains_key(&from);
        if new_peer && self.limits.peers == 0 {
            return Err(Error::NoPermission);
        }
        let next_global =
            admit_window(self.global, now_ms, self.limits.global, visible_cell_bytes)?;
        let next_peer = admit_window(
            self.windows.get(&from).copied().unwrap_or(CryptoWindow {
                window_start_ms: now_ms,
                used_ops: 0,
                used_bytes: 0,
                last_admitted_ms: now_ms,
            }),
            now_ms,
            self.limits.per_peer,
            visible_cell_bytes,
        )?;
        if new_peer && self.windows.len() >= self.limits.peers {
            let evicted = least_recent_peer(&self.windows).ok_or(Error::NoPermission)?;
            self.windows.remove(&evicted);
        }
        self.global = next_global;
        self.windows.insert(from, next_peer);
        Ok(())
    }
}

/// Pure window transition shared by the global and per-peer operation/byte budgets.
fn admit_window(
    mut window: CryptoWindow,
    now_ms: u128,
    budget: CryptoBudget,
    visible_cell_bytes: u64,
) -> Result<CryptoWindow> {
    if now_ms.saturating_sub(window.window_start_ms) >= ONION_CRYPTO_LIMIT_WINDOW_MS {
        window.window_start_ms = now_ms;
        window.used_ops = 0;
        window.used_bytes = 0;
    }
    if window.used_ops >= budget.operations {
        return Err(Error::NoPermission);
    }
    let next_bytes = window
        .used_bytes
        .checked_add(visible_cell_bytes)
        .filter(|used| *used <= budget.visible_cell_bytes)
        .ok_or(Error::NoPermission)?;
    window.used_ops = window.used_ops.checked_add(1).ok_or(Error::NoPermission)?;
    window.used_bytes = next_bytes;
    // A wall-clock rollback may extend throttling, but must not make an active peer look older
    // than every untouched peer and become the deterministic eviction victim.
    window.last_admitted_ms = window.last_admitted_ms.max(now_ms);
    Ok(window)
}

/// Deterministic LRU selection: `BTreeMap` iteration supplies the DID tie-break at one timestamp.
fn least_recent_peer(windows: &BTreeMap<Did, CryptoWindow>) -> Option<Did> {
    windows
        .iter()
        .min_by_key(|(_, window)| window.last_admitted_ms)
        .map(|(peer, _)| *peer)
}

/// Effect-boundary admission gate for expensive onion crypto operations.
#[derive(Debug, Default)]
pub(super) struct OnionCryptoGate {
    limiter: Mutex<OnionCryptoLimiter>,
}

impl OnionCryptoGate {
    pub(super) fn admit(&self, from: Did, now_ms: u128, visible_cell_bytes: u64) -> Result<()> {
        self.limiter
            .lock()
            .map_err(|_| Error::Lock)?
            .admit(from, now_ms, visible_cell_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(
        per_peer_operations: u32,
        global_operations: u32,
        per_peer_bytes: u64,
        global_bytes: u64,
        peers: usize,
    ) -> CryptoLimits {
        CryptoLimits {
            per_peer: CryptoBudget {
                operations: per_peer_operations,
                visible_cell_bytes: per_peer_bytes,
            },
            global: CryptoBudget {
                operations: global_operations,
                visible_cell_bytes: global_bytes,
            },
            peers,
        }
    }

    #[test]
    fn distinct_identity_windows_and_global_crypto_work_are_both_bounded() {
        let mut limiter = OnionCryptoLimiter::with_limits(limits(2, 3, u64::MAX, u64::MAX, 2));
        let first = Did::from(1_u32);
        let second = Did::from(2_u32);
        let third = Did::from(3_u32);

        assert!(limiter.admit(first, 0, 0).is_ok());
        assert!(limiter.admit(first, 1, 0).is_ok());
        assert!(limiter.admit(first, 2, 0).is_err());
        assert!(limiter.admit(second, 2, 0).is_ok());
        assert!(limiter.admit(third, 2, 0).is_err());
        assert!(limiter.admit(second, 3, 0).is_err());
        assert_eq!(limiter.windows.len(), 2);
        assert_eq!(limiter.global.used_ops, 3);
    }

    #[test]
    fn expired_crypto_windows_release_peer_and_global_budgets() {
        let mut limiter = OnionCryptoLimiter::with_limits(limits(1, 1, u64::MAX, u64::MAX, 1));
        assert!(limiter.admit(Did::from(1_u32), 0, 0).is_ok());
        assert!(limiter.admit(Did::from(2_u32), 1, 0).is_err());
        assert!(limiter
            .admit(Did::from(2_u32), ONION_CRYPTO_LIMIT_WINDOW_MS, 0)
            .is_ok());
        assert!(!limiter.windows.contains_key(&Did::from(1_u32)));
    }

    #[test]
    fn full_peer_table_evicts_lru_without_resetting_global_budget() {
        let first = Did::from(1_u32);
        let second = Did::from(2_u32);
        let third = Did::from(3_u32);
        let mut limiter = OnionCryptoLimiter::with_limits(limits(4, 5, 100, 100, 2));

        assert!(limiter.admit(first, 1, 10).is_ok());
        assert!(limiter.admit(second, 2, 10).is_ok());
        assert!(limiter.admit(first, 3, 10).is_ok());
        assert!(limiter.admit(first, 0, 0).is_ok());
        assert!(limiter.admit(third, 4, 10).is_ok());
        assert!(limiter.windows.contains_key(&first));
        assert!(!limiter.windows.contains_key(&second));
        assert!(limiter.windows.contains_key(&third));
        assert_eq!(limiter.global.used_ops, 5);
        assert!(limiter.admit(second, 5, 0).is_err());
        assert!(!limiter.windows.contains_key(&second));
        assert!(limiter.windows.contains_key(&first));
        assert!(limiter.windows.contains_key(&third));
    }

    #[test]
    fn visible_bucket_bytes_bound_oversized_cell_amplification() {
        let peer = Did::from(1_u32);
        let mut limiter = OnionCryptoLimiter::with_limits(limits(10, 10, 12, 20, 2));

        assert!(limiter.admit(peer, 1, 12).is_ok());
        assert!(limiter.admit(peer, 2, 1).is_err());
        assert_eq!(limiter.global.used_bytes, 12);
        assert_eq!(
            limiter.windows.get(&peer).map(|window| window.used_bytes),
            Some(12)
        );
    }
}
