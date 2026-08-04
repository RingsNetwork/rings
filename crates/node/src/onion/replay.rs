//! Bounded replay cache for one-shot onion payloads.

use std::collections::HashMap;
use std::hash::Hash;

use rings_core::dht::Did;

use super::circuit::OnionCircuitId;
use super::circuit::OnionForwardNonce;

const ONION_REPLAY_TTL_MS: u128 = 120_000;
const MAX_ONION_REPLAY_ENTRIES: usize = 4096;
const MAX_ONION_REPLAY_PEERS: usize = 64;

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

/// Admission result for a fixed-memory monotonic sequence window.
///
/// `Duplicate` and `Stale` are distinct state facts even though both are rejected at the wire
/// boundary: the TCP adapter records which replay-window relation caused the rejection.
#[cfg(rings_native)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SequenceAdmission {
    /// The sequence had not been observed and is now consumed.
    Consumed,
    /// The sequence is inside the window and its bit was already consumed.
    Duplicate,
    /// The sequence fell left of the retained window.
    Stale,
}

/// A 128-element sliding replay window for one circuit direction.
///
/// Model: `highest` is the greatest admitted sequence and bit `d` records
/// `highest - d`. Thus storage is O(1), a frame can be admitted at most once,
/// and bounded reordering cannot make a fresh frame disappear merely because an
/// unrelated circuit was busy.
#[cfg(rings_native)]
#[derive(Debug, Default)]
pub(crate) struct OnionSequenceWindow {
    highest: Option<u64>,
    consumed: u128,
}

#[cfg(rings_native)]
impl OnionSequenceWindow {
    /// Create a window in which `sequence` has already authorized the circuit's
    /// initial transition.
    pub(crate) fn with_initial(sequence: u64) -> Self {
        Self {
            highest: Some(sequence),
            consumed: 1,
        }
    }

    /// Consume one sequence, accepting unseen values within the reorder window.
    pub(crate) fn consume(&mut self, sequence: u64) -> SequenceAdmission {
        let Some(highest) = self.highest else {
            self.highest = Some(sequence);
            self.consumed = 1;
            return SequenceAdmission::Consumed;
        };
        if sequence > highest {
            let distance = sequence - highest;
            self.consumed = if distance >= u128::BITS.into() {
                1
            } else {
                (self.consumed << distance) | 1
            };
            self.highest = Some(sequence);
            return SequenceAdmission::Consumed;
        }
        let distance = highest - sequence;
        if distance >= u128::BITS.into() {
            return SequenceAdmission::Stale;
        }
        let bit = 1_u128 << distance;
        if self.consumed & bit == bit {
            SequenceAdmission::Duplicate
        } else {
            self.consumed |= bit;
            SequenceAdmission::Consumed
        }
    }
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

struct OnionForwardReplayPartition {
    cache: OnionForwardReplayCache,
    expires_at_ms: u128,
    last_activity_ms: u128,
}

impl OnionForwardReplayPartition {
    fn consume(&mut self, key: OnionForwardReplayKey, now_ms: u128) -> ReplayAdmission {
        let admission = self.cache.consume(key, now_ms);
        if admission == ReplayAdmission::Consumed {
            self.expires_at_ms = self
                .expires_at_ms
                .max(now_ms.saturating_add(self.cache.ttl_ms));
            self.last_activity_ms = now_ms;
        }
        admission
    }

    fn is_live_at(&self, now_ms: u128) -> bool {
        self.expires_at_ms > now_ms
    }

    fn purge_expired(&mut self, now_ms: u128) -> Vec<OnionForwardReplayKey> {
        let mut expired = Vec::new();
        self.cache.entries.retain(|key, expires_at_ms| {
            if *expires_at_ms > now_ms {
                true
            } else {
                expired.push(*key);
                false
            }
        });
        expired
    }
}

#[derive(Clone, Copy)]
struct ReplayOwner {
    peer: Did,
    expires_at_ms: u128,
}

/// Forward one-shot replay caches partitioned by the authenticated immediate sender.
///
/// Invariant: `peers.len() <= max_peers` and each peer cache has at most
/// `max_entries_per_peer` entries, so total replay memory is bounded by their product. A busy
/// authenticated peer can exhaust only its own entry budget. `owners` is the global uniqueness
/// witness: one live `(circuit_id, nonce)` belongs to exactly one peer partition, so changing the
/// immediate relay cannot replay an exit effect. At the peer-partition bound the least-recently
/// active partition is evicted, but its live owner witnesses remain until expiry; rotating cheap
/// identities therefore cannot exclude a new honest peer or make an evicted nonce replayable.
pub(crate) struct OnionForwardReplayPartitions {
    peers: HashMap<Did, OnionForwardReplayPartition>,
    owners: HashMap<OnionForwardReplayKey, ReplayOwner>,
    max_peers: usize,
    max_entries_per_peer: usize,
    ttl_ms: u128,
}

impl Default for OnionForwardReplayPartitions {
    fn default() -> Self {
        Self {
            peers: HashMap::new(),
            owners: HashMap::new(),
            max_peers: MAX_ONION_REPLAY_PEERS,
            max_entries_per_peer: MAX_ONION_REPLAY_ENTRIES,
            ttl_ms: ONION_REPLAY_TTL_MS,
        }
    }
}

impl OnionForwardReplayPartitions {
    #[cfg(test)]
    fn with_limits(max_peers: usize, max_entries_per_peer: usize, ttl_ms: u128) -> Self {
        Self {
            peers: HashMap::new(),
            owners: HashMap::new(),
            max_peers,
            max_entries_per_peer,
            ttl_ms,
        }
    }

    /// Consume one one-shot forward nonce in the sender's isolated partition.
    pub(crate) fn consume(
        &mut self,
        peer: Did,
        key: OnionForwardReplayKey,
        now_ms: u128,
    ) -> ReplayAdmission {
        if self.max_peers == 0 || self.max_entries_per_peer == 0 {
            return ReplayAdmission::Full;
        }
        self.purge_expired_owners(now_ms);
        self.purge_peer(peer, now_ms);
        if let Some(owner) = self.owners.get(&key).copied() {
            if owner.expires_at_ms > now_ms {
                return ReplayAdmission::Duplicate;
            }
            self.owners.remove(&key);
            self.purge_peer(owner.peer, now_ms);
        }
        // Admission rejection is atomic: a full owner-witness budget must not evict a live peer
        // partition when no successor state can be committed.
        let max_owner_entries = self.max_peers.saturating_mul(self.max_entries_per_peer);
        if self.owners.len() >= max_owner_entries {
            return ReplayAdmission::Full;
        }
        if !self.peers.contains_key(&peer) && self.peers.len() >= self.max_peers {
            self.purge_expired_peers(now_ms);
        }
        if !self.peers.contains_key(&peer) && self.peers.len() >= self.max_peers {
            self.evict_least_recently_active_peer();
        }
        let partition = self
            .peers
            .entry(peer)
            .or_insert_with(|| OnionForwardReplayPartition {
                cache: OnionReplayCache::with_limits(self.max_entries_per_peer, self.ttl_ms),
                expires_at_ms: 0,
                last_activity_ms: now_ms,
            });
        let admission = partition.consume(key, now_ms);
        if admission == ReplayAdmission::Consumed {
            self.owners.insert(key, ReplayOwner {
                peer,
                expires_at_ms: now_ms.saturating_add(self.ttl_ms),
            });
        }
        admission
    }

    fn purge_peer(&mut self, peer: Did, now_ms: u128) {
        let (expired, empty) = match self.peers.get_mut(&peer) {
            Some(partition) => {
                let expired = partition.purge_expired(now_ms);
                let empty = partition.cache.entries.is_empty();
                (expired, empty)
            }
            None => return,
        };
        for key in expired {
            if self
                .owners
                .get(&key)
                .is_some_and(|owner| owner.peer == peer && owner.expires_at_ms <= now_ms)
            {
                self.owners.remove(&key);
            }
        }
        if empty {
            self.peers.remove(&peer);
        }
    }

    fn purge_expired_peers(&mut self, now_ms: u128) {
        let expired_peers = self
            .peers
            .iter()
            .filter_map(|(peer, partition)| (!partition.is_live_at(now_ms)).then_some(*peer))
            .collect::<Vec<_>>();
        for peer in expired_peers {
            self.purge_peer(peer, now_ms);
        }
    }

    fn purge_expired_owners(&mut self, now_ms: u128) {
        self.owners.retain(|_, owner| owner.expires_at_ms > now_ms);
    }

    fn evict_least_recently_active_peer(&mut self) {
        let oldest = self
            .peers
            .iter()
            .min_by_key(|(peer, partition)| (partition.last_activity_ms, **peer))
            .map(|(peer, _)| *peer);
        if let Some(peer) = oldest {
            self.peers.remove(&peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use rings_core::ecc::SecretKey;

    use super::*;

    fn forward_key(byte: u8) -> OnionForwardReplayKey {
        OnionForwardReplayKey::new(
            OnionCircuitId::new([byte; 16]),
            OnionForwardNonce::new([byte.wrapping_add(1); 16]),
        )
    }

    fn peer() -> Did {
        SecretKey::random().address().into()
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

    #[test]
    fn replay_partitions_evict_lru_peer_without_reopening_consumed_nonce() {
        let mut partitions = OnionForwardReplayPartitions::with_limits(1, 2, 10);
        let first_peer = peer();
        let second_peer = peer();
        let first_key = forward_key(1);

        assert_eq!(
            partitions.consume(first_peer, first_key, 0),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(second_peer, forward_key(2), 1),
            ReplayAdmission::Consumed
        );
        assert_eq!(partitions.peers.len(), 1);
        assert!(partitions.peers.contains_key(&second_peer));
        assert_eq!(
            partitions.consume(first_peer, first_key, 2),
            ReplayAdmission::Duplicate
        );
    }

    #[test]
    fn replay_partitions_reclaim_expired_peer_only_at_admission_bound() {
        let mut partitions = OnionForwardReplayPartitions::with_limits(1, 2, 10);
        let first_peer = peer();
        let second_peer = peer();

        assert_eq!(
            partitions.consume(first_peer, forward_key(1), 0),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(second_peer, forward_key(2), 11),
            ReplayAdmission::Consumed
        );
        assert!(!partitions.peers.contains_key(&first_peer));
        assert!(partitions.peers.contains_key(&second_peer));
    }

    #[test]
    fn busy_peer_does_not_consume_another_peers_entry_budget() {
        let mut partitions = OnionForwardReplayPartitions::with_limits(2, 1, 10);
        let first_peer = peer();
        let second_peer = peer();

        assert_eq!(
            partitions.consume(first_peer, forward_key(1), 0),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(first_peer, forward_key(2), 1),
            ReplayAdmission::Full
        );
        assert_eq!(
            partitions.consume(second_peer, forward_key(3), 1),
            ReplayAdmission::Consumed
        );
    }

    #[test]
    fn replay_key_is_global_across_authenticated_peer_partitions() {
        let mut partitions = OnionForwardReplayPartitions::with_limits(2, 2, 10);
        let first_peer = peer();
        let second_peer = peer();
        let key = forward_key(1);

        assert_eq!(
            partitions.consume(first_peer, key, 0),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(second_peer, key, 1),
            ReplayAdmission::Duplicate
        );
    }

    #[test]
    fn zero_partition_or_entry_budget_fails_closed_without_metadata() {
        for mut partitions in [
            OnionForwardReplayPartitions::with_limits(0, 1, 10),
            OnionForwardReplayPartitions::with_limits(1, 0, 10),
        ] {
            assert_eq!(
                partitions.consume(peer(), forward_key(1), 0),
                ReplayAdmission::Full
            );
            assert!(partitions.peers.is_empty());
            assert!(partitions.owners.is_empty());
        }
    }

    #[test]
    fn live_owner_witnesses_bound_identity_rotation_memory() {
        let mut partitions = OnionForwardReplayPartitions::with_limits(1, 2, 10);
        let first_peer = peer();
        let second_peer = peer();
        let third_peer = peer();

        assert_eq!(
            partitions.consume(first_peer, forward_key(1), 0),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(second_peer, forward_key(2), 1),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(third_peer, forward_key(3), 2),
            ReplayAdmission::Full
        );
        assert_eq!(partitions.owners.len(), 2);
        assert_eq!(partitions.peers.len(), 1);
    }

    #[test]
    fn expired_global_key_can_move_to_another_peer_without_aba_removal() {
        let mut partitions = OnionForwardReplayPartitions::with_limits(2, 2, 10);
        let first_peer = peer();
        let second_peer = peer();
        let key = forward_key(1);

        assert_eq!(
            partitions.consume(first_peer, key, 0),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(second_peer, key, 11),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(first_peer, forward_key(2), 12),
            ReplayAdmission::Consumed
        );
        assert_eq!(
            partitions.consume(second_peer, key, 12),
            ReplayAdmission::Duplicate
        );
    }

    #[test]
    #[cfg(rings_native)]
    fn sequence_window_accepts_bounded_reordering_once() {
        let mut window = OnionSequenceWindow::default();

        assert_eq!(window.consume(0), SequenceAdmission::Consumed);
        assert_eq!(window.consume(2), SequenceAdmission::Consumed);
        assert_eq!(window.consume(1), SequenceAdmission::Consumed);
        assert_eq!(window.consume(1), SequenceAdmission::Duplicate);
    }

    #[test]
    #[cfg(rings_native)]
    fn sequence_window_rejects_values_left_of_fixed_window() {
        let mut window = OnionSequenceWindow::with_initial(0);

        assert_eq!(window.consume(128), SequenceAdmission::Consumed);
        assert_eq!(window.consume(0), SequenceAdmission::Stale);
        assert_eq!(window.consume(1), SequenceAdmission::Consumed);
    }
}
