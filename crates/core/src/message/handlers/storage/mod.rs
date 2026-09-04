#![deny(missing_docs)]

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::dht::entry::inbox::inbox_destination;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::ChordStorage;
use crate::dht::ChordStorageCache;
use crate::dht::ChordStorageRepair;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::core_actor_steps;
use crate::message::effects::yield_core_actor_step;
use crate::message::effects::CoreEffect;
use crate::message::types::FoundEntry;
use crate::message::types::Message;
use crate::message::types::SearchEntry;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::message::types::SyncEntriesWithSuccessorReport;
use crate::message::Encoded;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;
use crate::utils::get_epoch_ms;

/// ChordStorageInterface should imply necessary method for DHT storage
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait ChordStorageInterface<const REDUNDANT: u16> {
    /// Fetch an entry from DHT storage.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()>;
    /// Store an entry on DHT storage.
    async fn storage_store(&self, entry: Entry) -> Result<()>;
    /// Append data to a Data kind entry.
    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()>;
    /// Append data to a Data kind entry uniquely.
    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()>;
    /// Tombstone observed data in a Data kind entry.
    async fn storage_tombstone_data(&self, topic: &str, data: Encoded) -> Result<()>;
    /// Compact a Data kind entry after removing listed payloads.
    async fn storage_compact_data(&self, topic: &str, removals: Vec<Encoded>) -> Result<()>;
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

fn finish_storage_action(act: PeerRingAction) -> Result<()> {
    match act {
        PeerRingAction::None => Ok(()),
        act => Err(Error::unexpected_peer_ring_action(act)),
    }
}

async fn reset_storage_relay_destination(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    next: Did,
) -> Result<()> {
    handler
        .run_effects([CoreEffect::reset_destination(ctx, next)])
        .await
}

async fn repair_observed_storage_misses(
    transport: Arc<SwarmTransport>,
    entry: Entry,
    redundancy: u16,
) -> Result<()> {
    let misses = transport.take_storage_misses(entry.did, redundancy)?;
    let repair = transport
        .dht
        .read_repair_entry(entry, &misses, redundancy)
        .await?;
    run_storage_repair_transport_effects(transport, repair).await
}

/// Execute storage fetch actions for the Swarm-facing storage API.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_recursion(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_recursion)]
async fn handle_storage_fetch_act(
    transport: Arc<SwarmTransport>,
    resource: Did,
    act: PeerRingAction,
    redundancy: u16,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(evidence) => {
            transport
                .dht
                .local_cache_put(evidence.entry.clone())
                .await?;
            let misses = evidence.misses;
            let repair = transport
                .dht
                .read_repair_entry(evidence.entry, &misses, redundancy)
                .await?;
            run_storage_repair_transport_effects(transport.clone(), repair).await?;
        }
        PeerRingAction::RemoteAction(next, dht_act) => {
            if let PeerRingRemoteAction::FindEntry(query) = dht_act {
                tracing::debug!(
                    "storage_fetch send_message: SearchEntry({:?}) to {:?}",
                    query,
                    next
                );
                transport
                    .send_message(
                        Message::SearchEntry(SearchEntry {
                            resource: query.resource,
                            placement: query.placement,
                            redundancy,
                        }),
                        next,
                    )
                    .await?;
            }
        }
        PeerRingAction::MultiActions(acts) => {
            for (act, has_next) in core_actor_steps(acts) {
                handle_storage_fetch_act(transport.clone(), resource, act, redundancy).await?;
                if has_next {
                    yield_core_actor_step().await;
                }
            }
        }
        PeerRingAction::EntryMisses(misses) => {
            transport.observe_storage_misses(resource, redundancy, misses)?;
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

/// Execute storage store actions for the Swarm-facing storage API.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_recursion(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_recursion)]
pub(super) async fn handle_storage_store_act(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::RemoteAction(target, PeerRingRemoteAction::FindEntryForOperate(op)) => {
            transport
                .send_message(Message::OperateEntry(*op), target)
                .await?;
        }
        PeerRingAction::MultiActions(acts) => {
            for (act, has_next) in core_actor_steps(acts) {
                handle_storage_store_act(transport.clone(), act).await?;
                if has_next {
                    yield_core_actor_step().await;
                }
            }
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

async fn operate_entry_at_placement(
    dht: &PeerRing,
    placement: Did,
    op: EntryOperation,
    writer: Did,
) -> Result<()> {
    let now_ms = get_epoch_ms();
    dht.operate_storage_entry(now_ms, placement, op.stamped(now_ms, dht.did)?, writer)
        .await
}

async fn handle_placed_entry_operation(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    msg: &PlacedEntryOperation,
) -> Result<()> {
    msg.validate_placement(handler.transport.storage_redundancy())?;

    match handler
        .dht
        .find_storage_owner_for(msg.placement, msg.op.entry().kind)?
    {
        PeerRingAction::Some(_) => {
            operate_entry_at_placement(
                &handler.dht,
                msg.placement,
                msg.op.clone(),
                ctx.transaction.signer(),
            )
            .await
        }
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(_)) => {
            reset_storage_relay_destination(handler, ctx, next).await
        }
        action => Err(Error::unexpected_peer_ring_action(action)),
    }
}

/// Execute copy-only storage repair actions at the Swarm API adapter boundary.
async fn run_storage_repair_transport_effects(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
) -> Result<()> {
    for (delivery, has_next) in core_actor_steps(act.coalesced_storage_sync_deliveries()?) {
        let msg = SyncEntriesWithSuccessor::from_delivery(delivery);
        transport
            .send_storage_sync_or_defer(msg, "storage_repair")
            .await?;
        if has_next {
            yield_core_actor_step().await;
        }
    }
    Ok(())
}

/// Execute storage search actions emitted by inbound message handlers.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_recursion(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_recursion)]
async fn handle_storage_search_act(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    act: PeerRingAction,
    resource: Did,
    redundancy: u16,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(evidence) => {
            // A relay inbox is readable by its recipient alone.
            let data = match evidence.entry.kind {
                EntryKind::Data => vec![evidence.entry],
                EntryKind::RelayMessage
                    if ctx.transaction.signer() == inbox_destination(evidence.entry.did) =>
                {
                    vec![evidence.entry]
                }
                EntryKind::RelayMessage => Vec::new(),
            };
            handler
                .run_effects([CoreEffect::send_report_message(
                    ctx,
                    Message::FoundEntry(FoundEntry {
                        data,
                        misses: evidence.misses,
                        resource,
                        redundancy,
                    }),
                )])
                .await
        }
        PeerRingAction::EntryMisses(misses) => {
            handler
                .run_effects([CoreEffect::send_report_message(
                    ctx,
                    Message::FoundEntry(FoundEntry {
                        data: vec![],
                        misses,
                        resource,
                        redundancy,
                    }),
                )])
                .await
        }
        PeerRingAction::RemoteAction(next, _) => {
            reset_storage_relay_destination(handler, ctx, next).await
        }
        PeerRingAction::MultiActions(acts) => {
            for (act, has_next) in core_actor_steps(acts) {
                handle_storage_search_act(handler, ctx, act, resource, redundancy).await?;
                if has_next {
                    yield_core_actor_step().await;
                }
            }

            Ok(())
        }
        act => finish_storage_action(act),
    }
}

async fn operate_entry_under_redundancy<const REDUNDANT: u16>(
    swarm: &Swarm,
    operation: EntryOperation,
) -> Result<()> {
    swarm.transport.ensure_storage_redundancy::<REDUNDANT>()?;
    operate_entry(swarm.transport.clone(), operation).await
}

/// Apply `operation` under the transport's configured redundancy: locally where this node is
/// an accepted placement and by `OperateEntry` toward every remote one.
pub(crate) async fn operate_entry(
    transport: Arc<SwarmTransport>,
    operation: EntryOperation,
) -> Result<()> {
    let action = transport
        .dht
        .entry_operate_with_redundancy(operation, transport.storage_redundancy())
        .await?;
    handle_storage_store_act(transport, action).await
}

fn next_hop_for_sync_entries(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    msg: &SyncEntriesWithSuccessor,
) -> Result<Option<Did>> {
    if msg.destination.did() != ctx.relay.destination {
        return Err(Error::InvalidMessage(format!(
            "sync destination {:?} does not match relay destination {}",
            msg.destination, ctx.relay.destination
        )));
    }

    if ctx.is_relay_destination_for(handler.dht.did) {
        return Ok(None);
    }

    handler.dht.next_hop_for_storage_sync(msg.destination)
}

async fn report_synced_entries(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    purpose: StorageSyncPurpose,
    destination: StorageSyncDestination,
    acks: Vec<SyncedEntryAck>,
) -> Result<()> {
    handler
        .run_effects([CoreEffect::send_report_message(
            ctx,
            Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport::new(
                purpose,
                destination,
                handler.dht.did,
                acks,
            )),
        )])
        .await
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
impl<const REDUNDANT: u16> ChordStorageInterface<REDUNDANT> for Swarm {
    /// Fetch an entry. If it exists in local storage, copy it to the cache;
    /// otherwise query the responsible remote node.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let transport = self.transport.clone();
        let redundancy = transport.storage_redundancy();
        transport.start_storage_lookup(entry_key, redundancy)?;
        let act = transport
            .dht
            .entry_lookup_for_fetch(entry_key, redundancy)
            .await?;
        handle_storage_fetch_act(transport, entry_key, act, redundancy).await
    }

    /// Store Entry, `TryInto<Entry>` is implemented for alot of types
    async fn storage_store(&self, entry: Entry) -> Result<()> {
        operate_entry_under_redundancy::<REDUNDANT>(self, EntryOperation::Overwrite(entry)).await
    }

    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        operate_entry_under_redundancy::<REDUNDANT>(self, EntryOperation::Extend(entry)).await
    }

    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        operate_entry_under_redundancy::<REDUNDANT>(self, EntryOperation::Touch(entry)).await
    }

    async fn storage_tombstone_data(&self, topic: &str, data: Encoded) -> Result<()> {
        let entry: Entry = (topic.to_string(), data).try_into()?;
        operate_entry_under_redundancy::<REDUNDANT>(self, EntryOperation::Tombstone(entry)).await
    }

    async fn storage_compact_data(&self, topic: &str, removals: Vec<Encoded>) -> Result<()> {
        let entry = Entry::new(Entry::gen_did(topic)?, removals, EntryKind::Data);
        operate_entry_under_redundancy::<REDUNDANT>(self, EntryOperation::CompactData(entry)).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<SearchEntry> for MessageHandler {
    /// Search Entry via successor
    /// If a Entry is storead local, it will response immediately.(See Chordstorageinterface::storage_fetch)
    async fn handle(&self, ctx: &MessagePayload, msg: &SearchEntry) -> Result<()> {
        // For relay message, set redundant to 1
        match <PeerRing as ChordStorage<_, 1>>::entry_lookup(&self.dht, msg.placement).await {
            Ok(action) => {
                handle_storage_search_act(self, ctx, action, msg.resource, msg.redundancy).await
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FoundEntry> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &FoundEntry) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        // Pre: this node started a local lookup for (resource, redundancy).
        // Preservation: all remote-controlled FoundEntry fields are validated
        // before local_cache_put or read-repair can write storage state.
        let found_entry = msg.single_entry()?;
        self.transport
            .ensure_storage_lookup_active(msg.resource, msg.redundancy)?;
        self.transport.observe_storage_misses(
            msg.resource,
            msg.redundancy,
            msg.misses.iter().copied(),
        )?;
        if let Some(data) = found_entry {
            self.dht.local_cache_put(data.clone()).await?;
            repair_observed_storage_misses(self.transport.clone(), data.clone(), msg.redundancy)
                .await?;
        } else if !msg.misses.is_empty() {
            if let Some(entry) = self.dht.local_cache_get(msg.resource).await? {
                repair_observed_storage_misses(self.transport.clone(), entry, msg.redundancy)
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<PlacedEntryOperation> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &PlacedEntryOperation) -> Result<()> {
        handle_placed_entry_operation(self, ctx, msg).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<SyncEntriesWithSuccessor> for MessageHandler {
    // received remote sync entry request
    async fn handle(&self, ctx: &MessagePayload, msg: &SyncEntriesWithSuccessor) -> Result<()> {
        if let Some(next) = next_hop_for_sync_entries(self, ctx, msg)? {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, Some(next))])
                .await;
        }

        let origin = ctx.relay.try_origin_sender()?;
        let acks = self
            .transport
            .persist_storage_sync_entries(msg, origin)
            .await?;
        if msg.purpose.permits_source_cleanup() {
            if let Err(e) =
                report_synced_entries(self, ctx, msg.purpose, msg.destination, acks).await
            {
                tracing::warn!("Failed to report synced entries: {e:?}");
            }
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<SyncEntriesWithSuccessorReport> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload,
        msg: &SyncEntriesWithSuccessorReport,
    ) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }

        let signer = ctx.transaction.signer();
        let origin = ctx.relay.try_origin_sender()?;
        if signer != msg.receiver || origin != msg.receiver {
            return Err(Error::InvalidMessage(
                "storage sync report receiver does not match signed report origin".to_string(),
            ));
        }
        let acks =
            self.transport
                .take_pending_storage_sync_ack(ctx.transaction.tx_id, signer, msg)?;
        let action = self.dht.acknowledge_synced_entries(&acks).await?;
        finish_storage_action(action)
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod tests;
