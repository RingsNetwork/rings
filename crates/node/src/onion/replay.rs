//! Bounded replay cache for one-shot onion payloads.

use std::collections::HashMap;
use std::hash::Hash;

#[cfg(feature = "node")]
use super::circuit::OnionBackwardNonce;
use super::circuit::OnionCircuitId;
use super::circuit::OnionForwardNonce;
#[cfg(feature = "node")]
use super::circuit::OnionReturnId;

const ONION_REPLAY_TTL_MS: u128 = 120_000;
const MAX_ONION_REPLAY_ENTRIES: usize = 4096;

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

/// Replay cache key for one backward payload observed by a client.
#[cfg(feature = "node")]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OnionBackwardReplayKey {
    return_id: OnionReturnId,
    nonce: OnionBackwardNonce,
}

#[cfg(feature = "node")]
impl OnionBackwardReplayKey {
    /// Build a replay key from the client/exit return id and authenticated backward nonce.
    pub(crate) const fn new(return_id: OnionReturnId, nonce: OnionBackwardNonce) -> Self {
        Self { return_id, nonce }
    }
}

/// Result of attempting to consume one replay key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplayAdmission {
    /// The key was absent and is now consumed.
    Consumed,
    /// The key has already been consumed inside the replay window.
    Duplicate,
    /// The cache is full after expired entries were purged.
    Full,
}

/// Bounded cache of already-consumed nonces.
///
/// Invariant: an inserted key has already authorized at most one state transition.
/// Preservation: `consume` purges expired entries first, rejects duplicate keys, and inserts a new
/// key before the caller executes the side effect.
pub(crate) struct OnionReplayCache<K> {
    entries: HashMap<K, u128>,
    max_entries: usize,
    ttl_ms: u128,
}

impl<K> Default for OnionReplayCache<K> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: MAX_ONION_REPLAY_ENTRIES,
            ttl_ms: ONION_REPLAY_TTL_MS,
        }
    }
}

impl<K> OnionReplayCache<K>
where K: Eq + Hash
{
    #[cfg(test)]
    fn with_limits(max_entries: usize, ttl_ms: u128) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            ttl_ms,
        }
    }

    /// Consume a nonce exactly once inside the current replay window.
    pub(crate) fn consume(&mut self, key: K, now_ms: u128) -> ReplayAdmission {
        self.purge_expired(now_ms);
        if self.entries.contains_key(&key) {
            return ReplayAdmission::Duplicate;
        }
        if self.entries.len() >= self.max_entries {
            return ReplayAdmission::Full;
        }
        self.entries.insert(key, now_ms.saturating_add(self.ttl_ms));
        ReplayAdmission::Consumed
    }

    fn purge_expired(&mut self, now_ms: u128) {
        self.entries
            .retain(|_, expires_at_ms| *expires_at_ms > now_ms);
    }
}

/// Bounded cache of forward nonces at an exit.
pub(crate) type OnionForwardReplayCache = OnionReplayCache<OnionForwardReplayKey>;

/// Bounded cache of backward nonces at a client.
#[cfg(feature = "node")]
pub(crate) type OnionBackwardReplayCache = OnionReplayCache<OnionBackwardReplayKey>;

#[cfg(test)]
mod tests {
    use super::*;

    fn forward_key(byte: u8) -> OnionForwardReplayKey {
        OnionForwardReplayKey::new(
            OnionCircuitId::new([byte; 16]),
            OnionForwardNonce::new([byte.wrapping_add(1); 16]),
        )
    }

    #[test]
    fn replay_cache_rejects_duplicates_inside_window() {
        let mut cache = OnionReplayCache::with_limits(2, 10);
        let key = forward_key(1);

        assert_eq!(cache.consume(key, 0), ReplayAdmission::Consumed);
        assert_eq!(cache.consume(key, 1), ReplayAdmission::Duplicate);
    }

    #[test]
    fn replay_cache_rejects_new_keys_when_full() {
        let mut cache = OnionReplayCache::with_limits(1, 10);

        assert_eq!(cache.consume(forward_key(1), 0), ReplayAdmission::Consumed);
        assert_eq!(cache.consume(forward_key(2), 1), ReplayAdmission::Full);
    }

    #[test]
    fn replay_cache_reclaims_expired_keys_before_capacity_check() {
        let mut cache = OnionReplayCache::with_limits(1, 10);

        assert_eq!(cache.consume(forward_key(1), 0), ReplayAdmission::Consumed);
        assert_eq!(cache.consume(forward_key(2), 11), ReplayAdmission::Consumed);
    }
}
