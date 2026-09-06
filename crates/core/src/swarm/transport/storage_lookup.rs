use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::SwarmTransport;
use crate::dht::entry::PlacementMiss;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
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
fn evict_storage_lookup_observations(observations: &mut StorageLookupObservationMap, now_ms: i64) {
    observations.retain(|_, observation| {
        now_ms.saturating_sub(observation.observed_at_ms) <= STORAGE_LOOKUP_OBSERVATION_TTL_MS
    });

    while observations.len() > STORAGE_LOOKUP_OBSERVATION_CAPACITY {
        let Some(stale_key) = oldest_storage_lookup_observation_key(observations) else {
            break;
        };
        observations.remove(&stale_key);
    }
}

fn reserve_storage_lookup_observation_slot(observations: &mut StorageLookupObservationMap) {
    while observations.len() >= STORAGE_LOOKUP_OBSERVATION_CAPACITY {
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
        self.ensure_storage_redundancy_value(redundancy)?;
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
        evict_storage_lookup_observations(&mut observations, now);
        reserve_storage_lookup_observation_slot(&mut observations);
        observations.insert(key, StorageLookupObservation {
            observed_at_ms: now,
            misses: BTreeSet::new(),
        });
        Ok(())
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
        evict_storage_lookup_observations(&mut observations, now);
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
        evict_storage_lookup_observations(&mut observations, now);
        let Some(observation) = observations.get_mut(&key) else {
            return Err(Error::InvalidMessage(
                "storage miss observation has no active local lookup".to_string(),
            ));
        };
        observation.observed_at_ms = now;
        observation.misses.extend(misses);
        evict_storage_lookup_observations(&mut observations, now);
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
        evict_storage_lookup_observations(&mut observations, now);
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
