//! The Swarm's storage API: the application boundary of DHT storage. Its remote sends go
//! through the application-boundary sender, so a caller learns whether each left, and every
//! remote placement is rerouted (#859); the protocol logic it drives lives with the storage
//! handlers (`message::handlers::storage`).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::handlers::storage::handle_storage_fetch_act;
use crate::message::handlers::storage::operate_entry;
use crate::swarm::transport::Attempts;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;
use crate::utils::get_epoch_ms;

/// ChordStorageInterface should imply necessary method for DHT storage
///
/// Element lifetime: every element of a data topic expires at the earlier of its dot's issue time
/// plus the element horizon `H = EntryKind::Data.max_lifetime_ms()` and its topic's retention
/// bound, the latest bound any write joined into the topic requested (a plain write requests
/// `EntryKind::Data.default_lifetime_ms()`); writing the value again issues a fresh dot and a
/// fresh bound. A removal covers every earlier dot of its value, and is collected `H + σ` after
/// the dot it covers, with `σ = TS_OFFSET_TOLERANCE_MS`. The laws are stated in the `retention`
/// module of `dht::entry`.
///
/// Every remote placement of these operations is rerouted (`reroute`, #859): a send refused
/// before backend acceptance is retried from topology recomputed after a topology, link or
/// capacity event, never after a duration, within `REROUTING_BUDGET` deferrals; exhaustion
/// returns `Error::ReroutingExhausted` with the last cause. A placement is applied at most
/// once, so append and tombstone keep their non-idempotent semantics.
///
/// The placements of one operation run concurrently and all run to completion: an `Err`
/// means that at least one placement failed while others may have been applied, and it is an
/// ambiguous error whenever any placement's outcome is unknown.
///
/// Visibility (#948, #982): `Ok` means every placement was applied here or has left this node,
/// not that a remote one was applied. A later fetch resolves each placement again, under the
/// topology then, and an owner follows each node's referenced view, which changes on topology
/// events; the old owner hands the entry off only in a storage repair round. So the store and
/// the fetch may reach different owners:
///
/// ```text
/// store(e) ─▶ X = owner_t₀(p) applies e    ── a view changes: owner_t₁(p) = Y ≠ X ──
/// fetch(k) ─▶ Y = owner_t₁(p) holds nothing ─▶ miss
/// ```
///
/// The writer itself observes its write instead: once an operation returns `Ok`, it is applied
/// to the writer's fetch cache as every owner applies it
/// ([`ChordStorageCache::local_cache_observe_write`]), and a fetch answers the join of that
/// cache and its replies. Law (read-your-writes): if `op` on `k` returned `Ok` at `t₀`, every
/// read of `k` by this node after `t₀` contains `op`'s effect while `k` stays cached and the
/// effect is live. Another node reads the write once an owner its lookup reaches holds it.
///
/// Read-your-writes is a session guarantee about the writer's own view, not evidence of
/// replication: after `Ok` an owner may still refuse or lose the operation, and the writer
/// reads it all the same. A caller therefore never takes its own read-back as a publication
/// check (a registry's readiness, for one, #866); only another node's read witnesses that an
/// owner holds the write.
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

/// The DHT operations of a swarm, scoped by a stop token.
///
/// A placement waiting to be rerouted (#859) ends with `Error::ReroutingStopped` once `stop` is
/// requested. Nothing else is interrupted: storage reads and writes, locks, and sends run to
/// completion, so a caller that owns IndexedDB futures stops cooperatively. `Swarm`'s own
/// [`ChordStorageInterface`] is this view under a stop that never fires.
pub struct ScopedStorage<'a> {
    /// The transport the operations run on.
    transport: &'a Arc<SwarmTransport>,
    /// Ends every rerouting wait of these operations once requested.
    stop: StopToken,
}

impl Swarm {
    /// The DHT operations of this swarm, whose rerouting waits end once `stop` is requested.
    pub fn scoped_storage(&self, stop: StopToken) -> ScopedStorage<'_> {
        ScopedStorage {
            transport: &self.transport,
            stop,
        }
    }
}

impl ScopedStorage<'_> {
    /// The attempts every remote placement of these operations makes.
    fn attempts(&self) -> Attempts {
        Attempts::Rerouted(self.stop.clone())
    }

    /// Apply `operation` at its placements and, once every one has been applied or has left,
    /// observe it in this node's cache: the read-your-writes law of
    /// [`ChordStorageInterface`].
    ///
    /// The operation is stamped once, here, above every version the cache holds for its key
    /// (the writer-order law of [`ChordStorageCache`]): stamping is idempotent, so the owners
    /// apply and the cache observes the same dots. An `Err` observes nothing.
    async fn operate_observed(&self, operation: EntryOperation) -> Result<()> {
        let dht = &self.transport.dht;
        let operation = dht
            .local_cache_stamp_write(operation, get_epoch_ms())
            .await?;
        operate_entry(self.transport, operation.clone(), self.attempts()).await?;
        dht.local_cache_observe_write(operation).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl ChordStorageInterface for ScopedStorage<'_> {
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
            self.attempts(),
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
        self.operate_observed(EntryOperation::Overwrite(entry))
            .await
    }

    async fn storage_append_data(&self, topic: &str, data: Bytes) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        self.operate_observed(EntryOperation::Extend(entry)).await
    }

    async fn storage_tombstone_data(&self, topic: &str, data: Bytes) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        self.operate_observed(EntryOperation::Tombstone(entry))
            .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl ChordStorageInterface for Swarm {
    /// [`ScopedStorage::storage_fetch`] under a stop that never fires.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()> {
        self.scoped_storage(StopToken::never())
            .storage_fetch(entry_key)
            .await
    }

    /// [`ScopedStorage::storage_store`] under a stop that never fires.
    async fn storage_store(&self, entry: Entry) -> Result<()> {
        self.scoped_storage(StopToken::never())
            .storage_store(entry)
            .await
    }

    /// [`ScopedStorage::storage_append_data`] under a stop that never fires.
    async fn storage_append_data(&self, topic: &str, data: Bytes) -> Result<()> {
        self.scoped_storage(StopToken::never())
            .storage_append_data(topic, data)
            .await
    }

    /// [`ScopedStorage::storage_tombstone_data`] under a stop that never fires.
    async fn storage_tombstone_data(&self, topic: &str, data: Bytes) -> Result<()> {
        self.scoped_storage(StopToken::never())
            .storage_tombstone_data(topic, data)
            .await
    }
}
