//! The Swarm's storage API: the application boundary of DHT storage. Its remote sends go
//! through the application-boundary sender, so a caller learns whether each left; the protocol
//! logic it drives lives with the storage handlers (`message::handlers::storage`).

use async_trait::async_trait;
use bytes::Bytes;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::error::Result;
use crate::message::handlers::storage::handle_storage_fetch_act;
use crate::message::handlers::storage::operate_entry;
use crate::swarm::Swarm;

/// ChordStorageInterface should imply necessary method for DHT storage
///
/// Element lifetime: every element of a data topic expires at the earlier of its dot's issue time
/// plus the element horizon `H = EntryKind::Data.max_lifetime_ms()` and its topic's retention
/// bound, the latest bound any write joined into the topic requested (a plain write requests
/// `EntryKind::Data.default_lifetime_ms()`); writing the value again issues a fresh dot and a
/// fresh bound. A removal covers every earlier dot of its value, and is collected `H + σ` after
/// the dot it covers, with `σ = TS_OFFSET_TOLERANCE_MS`. The laws are stated in the `retention`
/// module of `dht::entry`.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait ChordStorageInterface {
    /// Fetch an entry from DHT storage.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()>;
    /// Store an entry on DHT storage, replacing its payloads (an `Overwrite`). Each stored
    /// payload expires as the trait documentation states unless it is written again.
    async fn storage_store(&self, entry: Entry) -> Result<()>;
    /// Append the element `data`, as its bytes, to a Data kind entry. The element expires as
    /// the trait documentation states unless it is appended again.
    async fn storage_append_data(&self, topic: &str, data: Bytes) -> Result<()>;
    /// Tombstone the observed element `data` in a Data kind entry: the removal covers every dot
    /// of `data` the storage owner holds or has held under a later one.
    async fn storage_tombstone_data(&self, topic: &str, data: Bytes) -> Result<()>;
}

/// ChordStorageInterfaceCacheChecker defines the interface for checking the local cache of the DHT.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait ChordStorageInterfaceCacheChecker {
    /// Check the local cache of the DHT for a specific entry key.
    ///
    /// Returns an optional `Entry` representing the cached data, or `None` if it is not found.
    async fn storage_check_cache(&self, entry_key: Did) -> Option<Entry>;
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl ChordStorageInterfaceCacheChecker for Swarm {
    /// Check local cache
    async fn storage_check_cache(&self, entry_key: Did) -> Option<Entry> {
        self.dht.local_cache_get(entry_key).await.ok().flatten()
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl ChordStorageInterface for Swarm {
    /// Fetch an entry. If it exists in local storage, copy it to the cache;
    /// otherwise query the responsible remote node.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()> {
        let transport = self.transport.clone();
        let redundancy = transport.storage_redundancy();
        transport.start_storage_lookup(entry_key, redundancy)?;
        let action = transport
            .dht
            .entry_lookup_for_fetch(entry_key, redundancy)
            .await?;
        let result = handle_storage_fetch_act(
            transport.clone(),
            &transport.application_egress(),
            entry_key,
            action,
            redundancy,
        )
        .await;
        let correlation = crate::swarm::observer::LookupCorrelation::StorageResource(entry_key);
        if result.is_err() {
            transport.observer().lookup_finished(
                crate::swarm::observer::LookupKind::Storage,
                correlation,
                crate::swarm::observer::LookupOutcome::Failed,
            );
        } else if transport.dht.local_cache_get(entry_key).await?.is_some() {
            transport.observer().lookup_finished(
                crate::swarm::observer::LookupKind::Storage,
                correlation,
                crate::swarm::observer::LookupOutcome::Succeeded,
            );
        }
        result
    }

    /// Store Entry, `TryInto<Entry>` is implemented for alot of types
    async fn storage_store(&self, entry: Entry) -> Result<()> {
        operate_entry(
            &self.transport,
            &self.transport.application_egress(),
            EntryOperation::Overwrite(entry),
        )
        .await
    }

    async fn storage_append_data(&self, topic: &str, data: Bytes) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        operate_entry(
            &self.transport,
            &self.transport.application_egress(),
            EntryOperation::Extend(entry),
        )
        .await
    }

    async fn storage_tombstone_data(&self, topic: &str, data: Bytes) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        operate_entry(
            &self.transport,
            &self.transport.application_egress(),
            EntryOperation::Tombstone(entry),
        )
        .await
    }
}
