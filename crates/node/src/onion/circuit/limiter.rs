use std::collections::BTreeMap;
use std::sync::Mutex;

use rings_core::dht::Did;

use super::MAX_ONION_CRYPTO_OPS_PER_WINDOW;
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
/// Invariant: for every active `from`, `used <= max_ops` within the half-open interval
/// `[window_start_ms, window_start_ms + ONION_CRYPTO_LIMIT_WINDOW_MS)`.
/// Preservation: `admit` removes expired windows before lookup, resets a stale peer window
/// before incrementing, and increments only after the upper bound check succeeds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct OnionCryptoLimiter {
    windows: BTreeMap<Did, CryptoWindow>,
}

impl OnionCryptoLimiter {
    pub(super) fn admit(&mut self, from: Did, now_ms: u128, max_ops: u32) -> Result<()> {
        if max_ops == 0 {
            return Ok(());
        }
        self.windows.retain(|_, window| {
            now_ms.saturating_sub(window.window_start_ms) < ONION_CRYPTO_LIMIT_WINDOW_MS
        });
        let window = self.windows.entry(from).or_insert(CryptoWindow {
            window_start_ms: now_ms,
            used: 0,
        });
        if now_ms.saturating_sub(window.window_start_ms) >= ONION_CRYPTO_LIMIT_WINDOW_MS {
            window.window_start_ms = now_ms;
            window.used = 0;
        }
        if window.used >= max_ops {
            return Err(Error::NoPermission);
        }
        window.used = window.used.saturating_add(1);
        Ok(())
    }
}

/// Effect-boundary admission gate for expensive onion crypto operations.
#[derive(Debug, Default)]
pub(super) struct OnionCryptoGate {
    limiter: Mutex<OnionCryptoLimiter>,
}

impl OnionCryptoGate {
    pub(super) fn admit(&self, from: Did, now_ms: u128) -> Result<()> {
        self.limiter.lock().map_err(|_| Error::Lock)?.admit(
            from,
            now_ms,
            MAX_ONION_CRYPTO_OPS_PER_WINDOW,
        )
    }
}
