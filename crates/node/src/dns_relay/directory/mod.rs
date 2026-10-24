//! The relay's directory: an owner's announcement, read from its descriptor in the DHT.
//!
//! ```text
//!   announcement(d) = announcement(σ)    σ = the newest descriptor on service_topic(d) that is
//!                                            signed for d, verifies and is live
//!                   = ⊥                  no such descriptor, or it does not validate
//!                   = Err                the DHT fetch failed
//! ```
//!
//! Every DNS query for an owner and every connection to its address reads its announcement, so
//! fetched answers are cached for [`LOOKUP_CACHE_TTL_MS`]: an owner's change of services reaches
//! the relay that much later at most, and the DHT sees one fetch per owner per period, however
//! many queries arrive. Failed fetches are not cached.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use rings_core::dht::entry::Entry;
use rings_core::utils::get_epoch_ms;
use rings_dns_relay::Announcement;
use rings_dns_relay::Directory;
use rings_dns_relay::LookupFailure;

use super::descriptor::service_topic;
use super::descriptor::ServiceDescriptor;
use super::DidOwner;
use crate::descriptor::RegistryElement;
use crate::prelude::rings_core::dht::Did;
use crate::processor::Processor;

/// How long a fetched announcement is answered from the cache, in milliseconds.
pub const LOOKUP_CACHE_TTL_MS: u128 = 15_000;
/// Most owners the cache holds; past it, the oldest fetch is evicted first.
const LOOKUP_CACHE_CAPACITY: usize = 4_096;

/// One cached fetch: its answer and when it was fetched.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Fetched {
    /// The announcement fetched, or `None` when the owner announced nothing valid.
    announcement: Option<Announcement>,
    /// When it was fetched, in milliseconds since the Unix epoch.
    at_ms: u128,
}

/// The fetch cache: pure, the caller supplies the time.
///
/// ```text
///   get(c, d, t) = c d                     if t − at(c d) < ttl
///   Invariant: |c| ≤ capacity
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
struct LookupCache {
    entries: BTreeMap<Did, Fetched>,
    ttl_ms: u128,
    capacity: usize,
}

impl LookupCache {
    /// An empty cache keeping fetches for `ttl_ms`, at most `capacity` of them.
    fn new(ttl_ms: u128, capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            ttl_ms,
            capacity,
        }
    }

    /// The cached answer for `did` at `now_ms`, if a fresh one is held; `Some(None)` is a fresh
    /// negative answer.
    fn get(&self, did: &Did, now_ms: u128) -> Option<Option<Announcement>> {
        self.entries
            .get(did)
            .filter(|fetched| now_ms.saturating_sub(fetched.at_ms) < self.ttl_ms)
            .map(|fetched| fetched.announcement.clone())
    }

    /// Cache `announcement` as fetched for `did` at `now_ms`.
    ///
    /// Preservation: when the cache is full, it first drops every stale fetch, then the oldest
    /// one, so it never exceeds its capacity.
    fn put(&mut self, did: Did, announcement: Option<Announcement>, now_ms: u128) {
        if !self.entries.contains_key(&did) && self.entries.len() >= self.capacity {
            let ttl_ms = self.ttl_ms;
            self.entries
                .retain(|_did, fetched| now_ms.saturating_sub(fetched.at_ms) < ttl_ms);
            if self.entries.len() >= self.capacity {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_did, fetched)| fetched.at_ms)
                    .map(|(did, _fetched)| *did);
                if let Some(oldest) = oldest {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(did, Fetched {
            announcement,
            at_ms: now_ms,
        });
    }
}

/// The announcement the newest valid descriptor among `elements` makes for `did`, at `now_ms`
/// in overlay `network_id`.
fn announcement_in(
    elements: &[bytes::Bytes],
    did: Did,
    now_ms: u128,
    network_id: u32,
) -> Option<Announcement> {
    let descriptors = elements
        .iter()
        .filter_map(|element| ServiceDescriptor::from_element(element).ok());
    let latest = ServiceDescriptor::latest_valid_for(descriptors, did, now_ms, network_id)?;
    match latest.announcement() {
        Ok(announcement) => Some(announcement),
        Err(error) => {
            tracing::debug!("dns relay: {did} announces an invalid descriptor: {error}");
            None
        }
    }
}

/// The directory over a node's DHT.
pub struct DhtDirectory {
    processor: Arc<Processor>,
    cache: Mutex<LookupCache>,
}

impl DhtDirectory {
    /// The directory reading `processor`'s DHT.
    pub fn new(processor: Arc<Processor>) -> Self {
        Self {
            processor,
            cache: Mutex::new(LookupCache::new(LOOKUP_CACHE_TTL_MS, LOOKUP_CACHE_CAPACITY)),
        }
    }

    /// The fresh cached answer for `did` at `now_ms`, if any.
    fn cached(&self, did: &Did, now_ms: u128) -> Option<Option<Announcement>> {
        self.cache.lock().ok()?.get(did, now_ms)
    }

    /// Cache `announcement` for `did`, fetched at `now_ms`.
    fn remember(&self, did: Did, announcement: Option<Announcement>, now_ms: u128) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(did, announcement, now_ms);
        }
    }
}

#[async_trait]
impl Directory for DhtDirectory {
    type Owner = DidOwner;

    async fn announcement(&self, owner: DidOwner) -> Result<Option<Announcement>, LookupFailure> {
        let did = owner.0;
        let now_ms = get_epoch_ms();
        if let Some(cached) = self.cached(&did, now_ms) {
            return Ok(cached);
        }
        let key = Entry::gen_did(&service_topic(did))
            .map_err(|error| LookupFailure(error.to_string()))?;
        let entry = self
            .processor
            .fetch_storage_entry(key)
            .await
            .map_err(|error| LookupFailure(error.to_string()))?;
        let announcement = entry.and_then(|entry| {
            announcement_in(&entry.data, did, now_ms, self.processor.swarm.network_id())
        });
        self.remember(did, announcement.clone(), now_ms);
        Ok(announcement)
    }
}

#[cfg(test)]
mod test_directory;
