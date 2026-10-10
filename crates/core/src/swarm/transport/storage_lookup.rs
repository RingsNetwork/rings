use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use super::SwarmTransport;
use crate::dht::entry::PlacementMiss;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::observer::LookupCorrelation;
use crate::swarm::observer::LookupKind;
use crate::utils::get_epoch_ms_i64;

const STORAGE_LOOKUP_OBSERVATION_TTL_MS: i64 = 30_000;
/// Maximum number of read-repair miss observation buckets retained per transport.
pub(crate) const STORAGE_LOOKUP_OBSERVATION_CAPACITY: usize = 1024;

// Invariant: after every successful observation-buffer mutation,
// observations.len() <= STORAGE_LOOKUP_OBSERVATION_CAPACITY.
// Invariant: after evict_storage_lookup_observations(observations, now), every
// retained bucket satisfies
// now.saturating_sub(observed_at_ms) <= STORAGE_LOOKUP_OBSERVATION_TTL_MS. This
// is the freshness witness required before PlacementMiss.owner drives read-repair.
pub(super) type StorageLookupObservationMap =
    BTreeMap<StorageLookupObservationKey, StorageLookupObservation>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct StorageLookupObservationKey {
    resource: Did,
    redundancy: u16,
}

pub(super) struct StorageLookupObservation {
    observed_at_ms: i64,
    misses: BTreeSet<PlacementMiss>,
    /// The node's answer clock at the last entry cached for this key, `0` before any: the reply
    /// marker (see [`StorageAnswerClock`]).
    answered_at: u64,
}

/// The node's storage answer clock: one tick per entry a lookup caches, for any key.
///
/// Law (marker): a fetcher reads the clock (its mark) before it fetches, and knows its key was
/// answered once the key's `answered_at` exceeds the mark. Ticks are monotone and node-wide, so
/// every answer after the mark stamps above it, whatever round of the key starts or bucket is
/// evicted meanwhile; an answer before the mark stamps at or below it.
#[derive(Default)]
pub(super) struct StorageAnswerClock(AtomicU64);

impl StorageAnswerClock {
    /// The clock now.
    fn mark(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Tick, and return the tick's stamp, above every mark read before it.
    fn tick(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel).saturating_add(1)
    }
}

fn storage_lookup_observation_now_ms() -> i64 {
    get_epoch_ms_i64()
}

fn oldest_storage_lookup_observation_key(
    observations: &StorageLookupObservationMap,
) -> Option<StorageLookupObservationKey> {
    observations
        .iter()
        .min_by_key(|(key, observation)| (observation.observed_at_ms, **key))
        .map(|(key, _)| *key)
}

// Post: observations.len() <= STORAGE_LOOKUP_OBSERVATION_CAPACITY.
// Post: forall bucket in observations,
// now_ms.saturating_sub(bucket.observed_at_ms) <= STORAGE_LOOKUP_OBSERVATION_TTL_MS.
// Preservation: removing expired buckets and then oldest buckets cannot create
// a stale bucket or increase the number of buckets.
// Post: observations.len() <= max_len; a caller about to insert passes
// CAPACITY - 1 so the insertion keeps the capacity invariant.
fn evict_storage_lookup_observations(
    observations: &mut StorageLookupObservationMap,
    now_ms: i64,
    max_len: usize,
) {
    observations.retain(|_, observation| {
        now_ms.saturating_sub(observation.observed_at_ms) <= STORAGE_LOOKUP_OBSERVATION_TTL_MS
    });

    while observations.len() > max_len {
        let Some(stale_key) = oldest_storage_lookup_observation_key(observations) else {
            break;
        };
        observations.remove(&stale_key);
    }
}

impl SwarmTransport {
    fn storage_lookup_observation_key(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<StorageLookupObservationKey> {
        self.ensure_storage_redundancy(redundancy)?;
        Ok(StorageLookupObservationKey {
            resource,
            redundancy,
        })
    }

    /// Start a fresh lookup round for `resource`.
    ///
    /// This replaces any previous miss observations for the same resource and
    /// redundancy with an empty local-authorized bucket. Inbound FoundEntry
    /// messages may only add misses to an existing bucket, so remote peers cannot
    /// create a new redundancy mode.
    ///
    /// Post: if capacity permits one active lookup, a bucket exists for
    /// `(resource, redundancy)` and contains no misses.
    /// Preservation: eviction establishes the capacity and freshness invariants
    /// before replacing the lookup-round bucket.
    pub(crate) fn start_storage_lookup(&self, resource: Did, redundancy: u16) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(
            &mut observations,
            now,
            STORAGE_LOOKUP_OBSERVATION_CAPACITY.saturating_sub(1),
        );
        let answered_at = observations
            .get(&key)
            .map_or(0, |observation| observation.answered_at);
        observations.insert(key, StorageLookupObservation {
            observed_at_ms: now,
            misses: BTreeSet::new(),
            answered_at,
        });
        self.observer().lookup_started(
            LookupKind::Storage,
            LookupCorrelation::StorageResource(resource),
        );
        Ok(())
    }

    /// Record an answer of the active lookup round of `(resource, redundancy)`: an entry it
    /// found has just been cached, and the cache serves it.
    ///
    /// Post: the key is answered since every mark read before this call
    /// ([`Self::storage_lookup_answered_since`]); a missing bucket is left missing.
    pub(crate) fn answer_storage_lookup(&self, resource: Did, redundancy: u16) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        if let Some(observation) = observations.get_mut(&key) {
            observation.answered_at = self.storage_answer_clock.tick();
        }
        Ok(())
    }

    /// The storage answer clock now: the mark a fetcher reads before it fetches.
    pub(crate) fn storage_lookup_mark(&self) -> u64 {
        self.storage_answer_clock.mark()
    }

    /// Whether a lookup of `(resource, redundancy)` cached an entry after `mark` was read.
    ///
    /// Post: `true` exactly when the key's latest answer stamps above `mark`; `false` once no
    /// round of the key is retained, so a reader that waits for it falls back to the cache when
    /// its poll budget ends.
    pub(crate) fn storage_lookup_answered_since(
        &self,
        resource: Did,
        redundancy: u16,
        mark: u64,
    ) -> Result<bool> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        evict_storage_lookup_observations(
            &mut observations,
            storage_lookup_observation_now_ms(),
            STORAGE_LOOKUP_OBSERVATION_CAPACITY,
        );
        Ok(observations
            .get(&key)
            .is_some_and(|observation| observation.answered_at > mark))
    }

    /// Validate that a storage lookup response belongs to a local lookup round.
    ///
    /// Post: `Ok(())` proves a fresh bucket exists for `(resource, redundancy)`.
    pub(crate) fn ensure_storage_lookup_active(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(
            &mut observations,
            now,
            STORAGE_LOOKUP_OBSERVATION_CAPACITY,
        );
        if observations.contains_key(&key) {
            Ok(())
        } else {
            Err(Error::InvalidMessage(
                "storage lookup response has no active local lookup".to_string(),
            ))
        }
    }

    /// Buffer placement misses observed by an in-flight storage lookup.
    ///
    /// Post: retained observation buckets satisfy the capacity and freshness
    /// invariants.
    /// Post: the supplied misses are appended only to a bucket previously created
    /// by [`Self::start_storage_lookup`].
    pub(crate) fn observe_storage_misses(
        &self,
        resource: Did,
        redundancy: u16,
        misses: impl IntoIterator<Item = PlacementMiss>,
    ) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut misses = misses.into_iter().peekable();
        if misses.peek().is_none() {
            return Ok(());
        }
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(
            &mut observations,
            now,
            STORAGE_LOOKUP_OBSERVATION_CAPACITY,
        );
        let Some(observation) = observations.get_mut(&key) else {
            return Err(Error::InvalidMessage(
                "storage miss observation has no active local lookup".to_string(),
            ));
        };
        observation.observed_at_ms = now;
        observation.misses.extend(misses);
        evict_storage_lookup_observations(
            &mut observations,
            now,
            STORAGE_LOOKUP_OBSERVATION_CAPACITY,
        );
        Ok(())
    }

    /// Drain fresh miss observations for a found entry.
    ///
    /// Post: returned misses come only from a bucket that survived freshness
    /// eviction at this call's observation time.
    /// Post: the bucket remains active with no buffered misses until TTL or a new
    /// lookup round removes it.
    /// Preservation: eviction before drain prevents stale owners from driving
    /// late read-repair.
    pub(crate) fn take_storage_misses(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<Vec<PlacementMiss>> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(
            &mut observations,
            now,
            STORAGE_LOOKUP_OBSERVATION_CAPACITY,
        );
        let Some(observation) = observations.get_mut(&key) else {
            return Err(Error::InvalidMessage(
                "storage repair has no active local lookup".to_string(),
            ));
        };
        Ok(std::mem::take(&mut observation.misses)
            .into_iter()
            .collect())
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    /// Test hook: make one observation bucket older than the freshness TTL.
    pub(crate) fn expire_storage_lookup_observation(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        if let Some(observation) = observations.get_mut(&key) {
            observation.observed_at_ms = storage_lookup_observation_now_ms()
                .saturating_sub(STORAGE_LOOKUP_OBSERVATION_TTL_MS + 1);
        }
        Ok(())
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    /// Test hook: count retained observation buckets.
    pub(crate) fn storage_lookup_observation_count(&self) -> Result<usize> {
        let observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(observations.len())
    }
}
