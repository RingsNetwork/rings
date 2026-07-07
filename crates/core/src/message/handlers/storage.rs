#![warn(missing_docs)]

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use crate::dht::entry::Entry;
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
use crate::message::effects::CoreEffect;
use crate::message::effects::PayloadRelayFunctor;
use crate::message::effects::StorageSyncFunctor;
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
use crate::prelude::entry::EntryOperation;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;

/// ChordStorageInterface should imply necessary method for DHT storage
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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
}

/// ChordStorageInterfaceCacheChecker defines the interface for checking the local cache of the DHT.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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
        .run_effects([PayloadRelayFunctor::reset_destination(ctx, next).into()])
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
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_storage_fetch_act<const REDUNDANT: u16>(
    transport: Arc<SwarmTransport>,
    resource: Did,
    act: PeerRingAction,
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
                .read_repair_entry(evidence.entry, &misses, REDUNDANT)
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
                            redundancy: REDUNDANT,
                        }),
                        next,
                    )
                    .await?;
            }
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_fetch_act::<REDUNDANT>(transport.clone(), resource, act).await?;
            }
        }
        PeerRingAction::EntryMisses(misses) => {
            transport.observe_storage_misses(resource, REDUNDANT, misses)?;
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

/// Execute storage store actions for the Swarm-facing storage API.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
pub(super) async fn handle_storage_store_act(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
) -> Result<()> {
    match act {
        PeerRingAction::RemoteAction(target, PeerRingRemoteAction::FindEntryForOperate(op)) => {
            transport
                .send_message(Message::OperateEntry(op), target)
                .await?;
        }
        PeerRingAction::MultiActions(acts) => {
            for act in acts {
                handle_storage_store_act(transport.clone(), act).await?;
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
) -> Result<()> {
    let op = op.stamped(dht.did)?;
    let this = match dht.storage.get(&placement.to_string()).await? {
        Some(this) => this,
        None => op.clone().gen_default_entry()?,
    };
    let entry = this.operate(op, dht.did)?;
    dht.join_storage_entry(placement, entry).await?;
    Ok(())
}

async fn handle_placed_entry_operation(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    msg: &PlacedEntryOperation,
) -> Result<()> {
    msg.validate_placement(handler.transport.storage_redundancy())?;

    match handler.dht.find_storage_owner(msg.placement)? {
        PeerRingAction::Some(_) => {
            operate_entry_at_placement(&handler.dht, msg.placement, msg.op.clone()).await
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
    for delivery in act.storage_sync_deliveries()? {
        let msg = SyncEntriesWithSuccessor::from_delivery(delivery);
        transport.send_storage_sync(msg).await?;
    }
    Ok(())
}

/// Lower copy-only storage repair actions emitted inside message handlers.
///
/// Law: lowering a storage-sync delivery preserves its destination and payload
/// exactly; the transport interpreter chooses the storage route and records the
/// ack capability at the effect boundary.
pub(super) fn storage_sync_effects(act: PeerRingAction) -> Result<Vec<CoreEffect<'static>>> {
    act.storage_sync_deliveries()?
        .into_iter()
        .map(|delivery| {
            let msg = SyncEntriesWithSuccessor::from_delivery(delivery);
            Ok(StorageSyncFunctor::send_storage_sync(msg).into())
        })
        .collect()
}

/// Execute storage search actions emitted by inbound message handlers.
#[cfg_attr(feature = "wasm", async_recursion(?Send))]
#[cfg_attr(not(feature = "wasm"), async_recursion)]
async fn handle_storage_search_act(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    act: PeerRingAction,
    resource: Did,
    redundancy: u16,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(evidence) => {
            handler
                .run_effects([PayloadRelayFunctor::send_report_message(
                    ctx,
                    Message::FoundEntry(FoundEntry {
                        data: vec![evidence.entry],
                        misses: evidence.misses,
                        resource,
                        redundancy,
                    }),
                )
                .into()])
                .await
        }
        PeerRingAction::EntryMisses(misses) => {
            handler
                .run_effects([PayloadRelayFunctor::send_report_message(
                    ctx,
                    Message::FoundEntry(FoundEntry {
                        data: vec![],
                        misses,
                        resource,
                        redundancy,
                    }),
                )
                .into()])
                .await
        }
        PeerRingAction::RemoteAction(next, _) => {
            reset_storage_relay_destination(handler, ctx, next).await
        }
        PeerRingAction::MultiActions(acts) => {
            let jobs = acts.iter().map(|act| async move {
                handle_storage_search_act(handler, ctx, act.clone(), resource, redundancy).await
            });

            for res in futures::future::join_all(jobs).await {
                if res.is_err() {
                    tracing::error!("Failed on handle multi actions: {:#?}", res)
                }
            }

            Ok(())
        }
        act => finish_storage_action(act),
    }
}

async fn persist_synced_entries(
    handler: &MessageHandler,
    msg: &SyncEntriesWithSuccessor,
) -> Result<Vec<SyncedEntryAck>> {
    // Preservation: batch validation is complete before the first storage
    // effect. Invalid placement data cannot leave a partially-written batch.
    let acks = accepted_synced_entry_acks(handler, msg)?;

    for ack in acks.iter() {
        handler
            .dht
            .join_storage_entry(ack.key, ack.entry.clone())
            .await?;
    }

    Ok(acks)
}

fn accepted_synced_entry_acks(
    handler: &MessageHandler,
    msg: &SyncEntriesWithSuccessor,
) -> Result<Vec<SyncedEntryAck>> {
    // Pre: no storage write for this sync batch has happened.
    // Post: every returned ack is locally routable and names an affine replica
    // placement for the entry under the configured storage redundancy.
    let mut acks = Vec::with_capacity(msg.data.len());
    for placed in msg.data.iter() {
        if !should_persist_synced_entry(&handler.dht, msg.destination, placed.key)? {
            continue;
        }

        placed.validate_placement(handler.transport.storage_redundancy())?;
        let entry = placed.entry.clone().try_into_storage_entry()?;
        acks.push(SyncedEntryAck::new(placed.key, entry));
    }
    Ok(acks)
}

fn should_persist_synced_entry(
    dht: &PeerRing,
    destination: StorageSyncDestination,
    placement: Did,
) -> Result<bool> {
    // Pre: `destination` was already matched against the signed relay
    // destination by `next_hop_for_sync_entries`.
    // Post: true implies this receiver is the local storage branch for
    // `placement`, and PlacementKey destinations can ack only their exact
    // placement key.
    if !destination_accepts_placement(destination, placement) {
        return Ok(false);
    }

    local_accepts_storage_placement(dht, placement)
}

fn destination_accepts_placement(destination: StorageSyncDestination, placement: Did) -> bool {
    match destination {
        StorageSyncDestination::PhysicalOwner(_) => true,
        StorageSyncDestination::PlacementKey(key) => key == placement,
    }
}

fn local_accepts_storage_placement(dht: &PeerRing, placement: Did) -> Result<bool> {
    match dht.find_storage_owner(placement)? {
        // Invariant: `Some(_)` is the local-storage branch. In non-virtual
        // Chord storage the DID carried by `Some` is the successor witness used
        // for fallback lookup, not a remote-owner denial.
        PeerRingAction::Some(_) => Ok(true),
        PeerRingAction::RemoteAction(_, PeerRingRemoteAction::FindSuccessor(_)) => Ok(false),
        action => Err(Error::unexpected_peer_ring_action(action)),
    }
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
        .run_effects([PayloadRelayFunctor::send_report_message(
            ctx,
            Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport::new(
                purpose,
                destination,
                handler.dht.did,
                acks,
            )),
        )
        .into()])
        .await
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorageInterfaceCacheChecker for Swarm {
    /// Check local cache
    async fn storage_check_cache(&self, entry_key: Did) -> Option<Entry> {
        self.dht.local_cache_get(entry_key).await.ok().flatten()
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl<const REDUNDANT: u16> ChordStorageInterface<REDUNDANT> for Swarm {
    /// Fetch an entry. If it exists in local storage, copy it to the cache;
    /// otherwise query the responsible remote node.
    async fn storage_fetch(&self, entry_key: Did) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        self.transport.start_storage_lookup(entry_key, REDUNDANT)?;
        // If peer found that data is on it's localstore, copy it to the cache
        let act =
            <PeerRing as ChordStorage<_, REDUNDANT>>::entry_lookup(&self.dht, entry_key).await?;
        handle_storage_fetch_act::<REDUNDANT>(self.transport.clone(), entry_key, act).await?;
        Ok(())
    }

    /// Store Entry, `TryInto<Entry>` is implemented for alot of types
    async fn storage_store(&self, entry: Entry) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let op = EntryOperation::Overwrite(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }

    async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let entry: Entry = (topic.to_string(), data).try_into()?;
        let op = EntryOperation::Extend(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }

    async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let entry: Entry = (topic.to_string(), data).try_into()?;
        let op = EntryOperation::Touch(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }

    async fn storage_tombstone_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.transport.ensure_storage_redundancy::<REDUNDANT>()?;
        let entry: Entry = (topic.to_string(), data).try_into()?;
        let op = EntryOperation::Tombstone(entry);
        let act = <PeerRing as ChordStorage<_, REDUNDANT>>::entry_operate(&self.dht, op).await?;
        handle_storage_store_act(self.transport.clone(), act).await?;
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<FoundEntry> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &FoundEntry) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([PayloadRelayFunctor::forward_payload(ctx, None).into()])
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<PlacedEntryOperation> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &PlacedEntryOperation) -> Result<()> {
        handle_placed_entry_operation(self, ctx, msg).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SyncEntriesWithSuccessor> for MessageHandler {
    // received remote sync entry request
    async fn handle(&self, ctx: &MessagePayload, msg: &SyncEntriesWithSuccessor) -> Result<()> {
        if let Some(next) = next_hop_for_sync_entries(self, ctx, msg)? {
            return self
                .run_effects([PayloadRelayFunctor::forward_payload(ctx, Some(next)).into()])
                .await;
        }

        let acks = persist_synced_entries(self, msg).await?;
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<SyncEntriesWithSuccessorReport> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload,
        msg: &SyncEntriesWithSuccessorReport,
    ) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([PayloadRelayFunctor::forward_payload(ctx, None).into()])
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

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod tests;
