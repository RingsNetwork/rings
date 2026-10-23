#![deny(missing_docs)]

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;
use futures::future::join_all;
use rerouted::LookupPlacement;
use rerouted::OperatePlacement;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::relay_holder::is_unplaced_hold;
use crate::dht::ChordStorageCache;
use crate::dht::ChordStorageRepair;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::dht::OperateRoute;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::error::Error;
use crate::error::Result;
use crate::error::SendClass;
use crate::message::effects::core_actor_steps;
use crate::message::effects::yield_core_actor_step;
use crate::message::effects::CoreEffect;
use crate::message::types::FoundEntry;
use crate::message::types::Message;
use crate::message::types::RelayCarrierRefused;
use crate::message::types::SearchEntry;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::message::types::SyncEntriesWithSuccessorReport;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::swarm::transport::egress::Application;
use crate::swarm::transport::egress::Discipline;
use crate::swarm::transport::egress::Egress;
use crate::swarm::transport::reroute;
use crate::swarm::transport::Attempts;
use crate::swarm::transport::Route;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Join the results of an operation's placements, which all ran to completion.
///
/// `Ok` iff every placement succeeded. Otherwise the error reported is the join of the
/// placements' acceptance classes: an `Ambiguous` error when any placement's effect is unknown
/// (so a caller never mistakes a possibly-applied operation for an unapplied one), else the
/// first error; every other error is logged, not dropped.
fn join_placements(results: Vec<Result<()>>) -> Result<()> {
    let mut errors = results
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return Ok(());
    }
    let reported = errors
        .iter()
        .position(|error| error.send_class() == SendClass::Ambiguous)
        .unwrap_or(0);
    let error = errors.swap_remove(reported);
    for other in errors {
        tracing::warn!(%other, "another placement of the storage operation failed");
    }
    Err(error)
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

/// Repair the misses a remote lookup observed for `entry`; a handler runs it, so each repair is
/// only queued.
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
    run_storage_repair_transport_effects(&transport, &transport.protocol_egress(), repair).await
}

/// Cache `entry`, a reply to the lookup round of `(resource, redundancy)`, and count it as the
/// round's answer iff the cache now serves it.
///
/// Law (an answer is what a reader can read): the reply marker advances only for a value the
/// cache serves at this clock. A carrier read just before its retention bound and cached just
/// after it (or under a clock up to σ ahead) is admitted, since an unstable remove or register
/// holds it live, but it is served as absent; counting it would end a fetcher's wait on a cache
/// that holds nothing to read while another placement's reply may still be in flight.
async fn cache_lookup_reply(
    transport: &SwarmTransport,
    resource: Did,
    redundancy: u16,
    entry: Entry,
) -> Result<()> {
    transport.dht.local_cache_put(entry).await?;
    if transport.dht.local_cache_get(resource).await?.is_some() {
        transport.answer_storage_lookup(resource, redundancy)?;
    }
    Ok(())
}

/// Execute storage fetch actions for the Swarm-facing storage API: each remote lookup is a
/// placement making the `attempts` given (`reroute`, #859), each read repair is sent through
/// `sender`, the application boundary's egress.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_recursion(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_recursion)]
pub(crate) async fn handle_storage_fetch_act(
    transport: Arc<SwarmTransport>,
    sender: &Egress<'_, Application>,
    resource: Did,
    act: PeerRingAction,
    redundancy: u16,
    attempts: Attempts,
) -> Result<()> {
    match act {
        PeerRingAction::SomeEntry(evidence) => {
            cache_lookup_reply(&transport, resource, redundancy, evidence.entry.clone()).await?;
            let misses = evidence.misses;
            let repair = transport
                .dht
                .read_repair_entry(evidence.entry, &misses, redundancy)
                .await?;
            run_storage_repair_transport_effects(&transport, sender, repair).await?;
        }
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindEntry(query)) => {
            tracing::debug!("storage_fetch SearchEntry({query:?}) to {next:?}");
            let placement = LookupPlacement { query, redundancy };
            let message = placement.message(query);
            reroute(
                &transport,
                &placement,
                Route::Remote { next, message },
                attempts,
            )
            .await?;
        }
        PeerRingAction::MultiActions(acts) => {
            // Placements are independent: one waiting for its trigger must not hold the others.
            // The Swarm storage API runs off the core actor, so nothing here yields to it.
            join_placements(
                join_all(acts.into_iter().map(|act| {
                    handle_storage_fetch_act(
                        transport.clone(),
                        sender,
                        resource,
                        act,
                        redundancy,
                        attempts.clone(),
                    )
                }))
                .await,
            )?;
        }
        PeerRingAction::EntryMisses(misses) => {
            transport.observe_storage_misses(resource, redundancy, misses)?;
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

/// Execute storage store actions: each remote operation is a placement making the `attempts`
/// given (`reroute`, #859), sent under the egress discipline they imply.
///
/// ```text
/// MultiActions under Rerouted ⇒ concurrently: placements are independent, and one waiting
///                                for its trigger must not hold the others (the Swarm API runs
///                                off the core actor)
///                 under Single   ⇒ one by one, yielding to the core actor between steps (the
///                                protocol context runs on it)
/// ```
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_recursion(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_recursion)]
pub(super) async fn handle_storage_store_act(
    transport: Arc<SwarmTransport>,
    act: PeerRingAction,
    attempts: Attempts,
) -> Result<()> {
    match act {
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindEntryForOperate(op)) => {
            let placement = OperatePlacement(*op);
            let message = placement.message();
            reroute(
                &transport,
                &placement,
                Route::Remote { next, message },
                attempts,
            )
            .await?;
        }
        PeerRingAction::MultiActions(acts) => {
            match attempts {
                Attempts::Rerouted(_) => join_placements(
                    join_all(acts.into_iter().map(|act| {
                        handle_storage_store_act(transport.clone(), act, attempts.clone())
                    }))
                    .await,
                )?,
                Attempts::Single => {
                    for (act, has_next) in core_actor_steps(acts) {
                        handle_storage_store_act(transport.clone(), act, Attempts::Single).await?;
                        if has_next {
                            yield_core_actor_step().await;
                        }
                    }
                }
            }
        }
        act => finish_storage_action(act)?,
    }
    Ok(())
}

/// Apply or route `msg`; a relay hold this node does not take (`is_unplaced_hold`) is also
/// handed back to its writer, which keeps it and sends it again (`RelayCarrierRefused`).
async fn handle_placed_entry_operation(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    msg: &PlacedEntryOperation,
) -> Result<()> {
    let result = place_entry_operation(handler, ctx, msg).await;
    if let Err(error) = &result {
        if msg.op.kind().is_relay_inbox() && is_unplaced_hold(error) {
            let refusal = Message::RelayCarrierRefused(RelayCarrierRefused {
                operation: msg.clone(),
            });
            if let Err(report) = handler
                .run_effects([CoreEffect::send_report_message(ctx, refusal)])
                .await
            {
                tracing::warn!(?report, refused = ?error, "relay hold refusal not reported");
            }
        }
    }
    result
}

/// Apply `msg` here when this node accepts its placement, or route it on.
async fn place_entry_operation(
    handler: &MessageHandler,
    ctx: &MessagePayload,
    msg: &PlacedEntryOperation,
) -> Result<()> {
    msg.validate_placement(handler.transport.storage_redundancy())?;

    let dht = &handler.dht;
    match dht.operate_route(msg.placement, msg.op.kind())? {
        OperateRoute::Owned => {
            let now_ms = get_epoch_ms();
            let op = msg.op.clone().stamped(now_ms, dht.did)?;
            dht.operate_storage_entry(now_ms, msg.placement, op, ctx.transaction.origin())
                .await
        }
        // A relay carrier is sent to the holder its writer's view names, never routed toward
        // one: a holder that disagrees refuses it rather than pass it on (`dht::relay_holder`).
        OperateRoute::Remote(_) if msg.op.kind().is_relay_inbox() => {
            Err(Error::RelayCarrierNotHeldHere)
        }
        OperateRoute::Remote(next) => reset_storage_relay_destination(handler, ctx, next).await,
    }
}

/// Execute copy-only storage repair actions through `egress`, under its discipline: the Swarm
/// storage API waits for each repair's admission, a handler only queues it.
async fn run_storage_repair_transport_effects<D: Discipline>(
    transport: &SwarmTransport,
    egress: &Egress<'_, D>,
    act: PeerRingAction,
) -> Result<()> {
    for (delivery, has_next) in core_actor_steps(act.coalesced_storage_sync_deliveries()?) {
        let msg = SyncEntriesWithSuccessor::from_delivery(delivery);
        transport
            .send_storage_sync_or_defer(msg, egress, "storage_repair")
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
            handler
                .run_effects([CoreEffect::send_report_message(
                    ctx,
                    Message::FoundEntry(FoundEntry {
                        data: vec![evidence.entry],
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

/// Apply `operation` under the transport's configured redundancy: locally where this node is
/// an accepted placement and by `OperateEntry` toward every remote one, each remote placement
/// making the `attempts` given.
///
/// The Swarm storage API reroutes (`Attempts::Rerouted`), its sends under the application
/// discipline; the protocol context makes one attempt (`Attempts::Single`), its sends under the
/// protocol discipline, so no inbound lane waits on a peer (see `swarm::transport::Attempts`).
pub(crate) async fn operate_entry(
    transport: &Arc<SwarmTransport>,
    operation: EntryOperation,
    attempts: Attempts,
) -> Result<()> {
    let action = transport
        .dht
        .entry_operate(operation, transport.storage_redundancy())
        .await?;
    handle_storage_store_act(transport.clone(), action, attempts).await
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
impl HandleMsg<SearchEntry> for MessageHandler {
    /// Look up the one placement `msg` interrogates: a value stored here is answered at once,
    /// otherwise the search is forwarded toward the placement's owner.
    async fn handle(&self, ctx: &MessagePayload, msg: &SearchEntry) -> Result<()> {
        // Redundancy 1: the lookup covers `msg.placement` alone, the sender's other placements
        // are interrogated by their own searches.
        let action = self.dht.entry_lookup(msg.placement, 1).await?;
        handle_storage_search_act(self, ctx, action, msg.resource, msg.redundancy).await
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
            cache_lookup_reply(&self.transport, msg.resource, msg.redundancy, data.clone()).await?;
            repair_observed_storage_misses(self.transport.clone(), data.clone(), msg.redundancy)
                .await?;
        } else if !msg.misses.is_empty() {
            if let Some(entry) = self
                .dht
                .local_cache_held(msg.resource, crate::utils::get_epoch_ms())
                .await?
            {
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

        let sender = ctx.transaction.origin();
        let acks = self
            .transport
            .persist_storage_sync_entries(msg, sender)
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

        let origin = ctx.transaction.origin();
        if origin != msg.receiver {
            return Err(Error::InvalidMessage(
                "storage sync report receiver does not match signed report origin".to_string(),
            ));
        }
        let acks =
            self.transport
                .take_pending_storage_sync_ack(ctx.transaction.tx_id, origin, msg)?;
        let action = self.dht.acknowledge_synced_entries(&acks).await?;
        finish_storage_action(action)
    }
}

mod rerouted;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod tests;
