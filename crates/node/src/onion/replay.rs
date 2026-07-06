//! Bounded replay cache for exit-side one-shot payloads.

use std::collections::HashMap;

use super::circuit::OnionCircuitId;
use super::circuit::OnionForwardNonce;
use crate::error::Error;
use crate::error::Result;

const ONION_FORWARD_REPLAY_TTL_MS: u128 = 120_000;
const MAX_ONION_FORWARD_REPLAY_ENTRIES: usize = 4096;

/// Replay cache key for one forward payload observed by an exit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OnionForwardReplayKey {
    circuit_id: OnionCircuitId,
    nonce: OnionForwardNonce,
}

impl OnionForwardReplayKey {
    /// Build a replay key from the circuit id and encrypted forward nonce.
    pub(crate) const fn new(circuit_id: OnionCircuitId, nonce: OnionForwardNonce) -> Self {
        Self { circuit_id, nonce }
    }
}

/// Bounded cache of already-consumed forward nonces.
///
/// Invariant: an inserted key has already authorized at most one exit-side action.
/// Preservation: `consume` purges expired entries first, rejects duplicate keys, and inserts a new
/// key before the caller executes the side effect.
pub(crate) struct OnionForwardReplayCache {
    entries: HashMap<OnionForwardReplayKey, u128>,
    max_entries: usize,
    ttl_ms: u128,
}

impl Default for OnionForwardReplayCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: MAX_ONION_FORWARD_REPLAY_ENTRIES,
            ttl_ms: ONION_FORWARD_REPLAY_TTL_MS,
        }
    }
}

impl OnionForwardReplayCache {
    /// Consume a forward nonce exactly once inside the current replay window.
    pub(crate) fn consume(&mut self, key: OnionForwardReplayKey, now_ms: u128) -> Result<()> {
        self.purge_expired(now_ms);
        if self.entries.contains_key(&key) {
            return Err(Error::OnionRouteError(
                "replayed onion forward payload".to_string(),
            ));
        }
        if self.entries.len() >= self.max_entries {
            return Err(Error::NoPermission);
        }
        self.entries.insert(key, now_ms.saturating_add(self.ttl_ms));
        Ok(())
    }

    fn purge_expired(&mut self, now_ms: u128) {
        self.entries
            .retain(|_, expires_at_ms| *expires_at_ms > now_ms);
    }
}
