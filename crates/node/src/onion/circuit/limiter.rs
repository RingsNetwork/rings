use std::collections::BTreeMap;
use std::sync::Mutex;

use rings_core::dht::Did;

use super::MAX_ONION_CRYPTO_OPS_GLOBAL_PER_WINDOW;
use super::MAX_ONION_CRYPTO_OPS_PER_WINDOW;
use super::MAX_ONION_CRYPTO_PEERS;
use super::ONION_CRYPTO_LIMIT_WINDOW_MS;
use crate::error::Error;
use crate::error::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CryptoWindow {
    window_start_ms: u128,
    used: u32,
}

/// Pure per-peer crypto admission windows.
///
/// Invariant: for every active `from`, `used <= max_ops_per_window` within the half-open
/// interval `[window_start_ms, window_start_ms + ONION_CRYPTO_LIMIT_WINDOW_MS)`.
/// Preservation: `admit` removes expired windows before lookup, resets a stale peer window
/// before incrementing, and increments only after the upper bound check succeeds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OnionCryptoLimiter {
    max_ops_per_window: u32,
    max_global_ops_per_window: u32,
    max_peers: usize,
    global: CryptoWindow,
    windows: BTreeMap<Did, CryptoWindow>,
}

impl Default for OnionCryptoLimiter {
    fn default() -> Self {
        Self::with_limits(
            MAX_ONION_CRYPTO_OPS_PER_WINDOW,
            MAX_ONION_CRYPTO_OPS_GLOBAL_PER_WINDOW,
            MAX_ONION_CRYPTO_PEERS,
        )
    }
}

impl OnionCryptoLimiter {
    #[cfg(test)]
    pub(super) fn with_limit(max_ops_per_window: u32) -> Self {
        Self::with_limits(max_ops_per_window, u32::MAX, usize::MAX)
    }

    fn with_limits(
        max_ops_per_window: u32,
        max_global_ops_per_window: u32,
        max_peers: usize,
    ) -> Self {
        Self {
            max_ops_per_window,
            max_global_ops_per_window,
            max_peers,
            global: CryptoWindow {
                window_start_ms: 0,
                used: 0,
            },
            windows: BTreeMap::new(),
        }
    }

    pub(super) fn admit(&mut self, from: Did, now_ms: u128) -> Result<()> {
        if self.max_ops_per_window == 0 || self.max_global_ops_per_window == 0 {
            return Ok(());
        }
        self.windows.retain(|_, window| {
            now_ms.saturating_sub(window.window_start_ms) < ONION_CRYPTO_LIMIT_WINDOW_MS
        });
        if !self.windows.contains_key(&from) && self.windows.len() >= self.max_peers {
            return Err(Error::NoPermission);
        }
        let next_global = admit_window(self.global, now_ms, self.max_global_ops_per_window)?;
        let next_peer = admit_window(
            self.windows.get(&from).copied().unwrap_or(CryptoWindow {
                window_start_ms: now_ms,
                used: 0,
            }),
            now_ms,
            self.max_ops_per_window,
        )?;
        self.global = next_global;
        self.windows.insert(from, next_peer);
        Ok(())
    }
}

/// Pure window transition shared by the global and per-peer budgets.
fn admit_window(mut window: CryptoWindow, now_ms: u128, limit: u32) -> Result<CryptoWindow> {
    if now_ms.saturating_sub(window.window_start_ms) >= ONION_CRYPTO_LIMIT_WINDOW_MS {
        window.window_start_ms = now_ms;
        window.used = 0;
    }
    if window.used >= limit {
        return Err(Error::NoPermission);
    }
    window.used = window.used.checked_add(1).ok_or(Error::NoPermission)?;
    Ok(window)
}

/// Effect-boundary admission gate for expensive onion crypto operations.
#[derive(Debug, Default)]
pub(super) struct OnionCryptoGate {
    limiter: Mutex<OnionCryptoLimiter>,
}

impl OnionCryptoGate {
    pub(super) fn admit(&self, from: Did, now_ms: u128) -> Result<()> {
        self.limiter
            .lock()
            .map_err(|_| Error::Lock)?
            .admit(from, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_identity_windows_and_global_crypto_work_are_both_bounded() {
        let mut limiter = OnionCryptoLimiter::with_limits(2, 3, 2);
        let first = Did::from(1_u32);
        let second = Did::from(2_u32);
        let third = Did::from(3_u32);

        assert!(limiter.admit(first, 0).is_ok());
        assert!(limiter.admit(first, 1).is_ok());
        assert!(limiter.admit(first, 2).is_err());
        assert!(limiter.admit(second, 2).is_ok());
        assert!(limiter.admit(third, 2).is_err());
        assert!(limiter.admit(second, 3).is_err());
        assert_eq!(limiter.windows.len(), 2);
        assert_eq!(limiter.global.used, 3);
    }

    #[test]
    fn expired_crypto_windows_release_peer_and_global_budgets() {
        let mut limiter = OnionCryptoLimiter::with_limits(1, 1, 1);
        assert!(limiter.admit(Did::from(1_u32), 0).is_ok());
        assert!(limiter.admit(Did::from(2_u32), 1).is_err());
        assert!(limiter
            .admit(Did::from(2_u32), ONION_CRYPTO_LIMIT_WINDOW_MS)
            .is_ok());
        assert!(!limiter.windows.contains_key(&Did::from(1_u32)));
    }
}
