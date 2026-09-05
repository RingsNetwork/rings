use std::collections::BTreeMap;
use std::mem;

use super::delivery::SendCompletionOutcome;
use super::outbound::OutboundCompletion;
use super::SwarmTransport;
use crate::dht::entry::inbox::validate_inbox_relocation;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::error::Error;
use crate::error::Result;
use crate::message::yield_core_actor_step;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::SyncEntriesWithSuccessor;
use crate::message::SyncEntriesWithSuccessorReport;
use crate::utils::get_epoch_ms;
use crate::utils::get_epoch_ms_i64;

const STORAGE_SYNC_ACK_CAPACITY: usize = 1024;

pub(super) type StorageSyncAckMap = BTreeMap<uuid::Uuid, StorageSyncAckCapability>;

pub(super) struct StorageSyncAckCapability {
    recorded_at_ms: i64,
    purpose: StorageSyncPurpose,
    destination: StorageSyncDestination,
    expected_receiver: Did,
    expected_acks: Vec<SyncedEntryAck>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrackedStorageSyncOutcome {
    PersistedLocally,
    Delivered(uuid::Uuid),
    Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageSyncOutcome {
    PersistedLocally,
    Sent(uuid::Uuid),
    Deferred,
}

impl StorageSyncOutcome {
    #[cfg(test)]
    pub(crate) const fn is_sent(self) -> bool {
        matches!(self, Self::Sent(_))
    }

    pub(crate) const fn is_deferred(self) -> bool {
        matches!(self, Self::Deferred)
    }
}

enum StorageSyncCompletion {
    PersistedLocally,
    Submitted {
        tx_id: uuid::Uuid,
        outcome: SendCompletionOutcome,
    },
}

// Invariant: physical-owner sync proves the final owner identity. Placement-key
// sync proves only the next hop visible from the sender, so a farther receiver
// cannot use its report to delete local storage.
fn expected_storage_sync_receiver(destination: StorageSyncDestination, route_next_hop: Did) -> Did {
    match destination {
        StorageSyncDestination::PhysicalOwner(owner) => owner,
        StorageSyncDestination::PlacementKey(_) => route_next_hop,
    }
}

fn storage_sync_ack_now_ms() -> i64 {
    get_epoch_ms_i64()
}

fn expected_sync_acks(data: &[PlacedEntry]) -> Result<Vec<SyncedEntryAck>> {
    data.iter()
        .map(|placed| {
            Ok(SyncedEntryAck::new(
                placed.key,
                placed.entry.clone().try_into_storage_entry()?,
            ))
        })
        .collect()
}

fn validate_report_acks(
    expected_acks: &[SyncedEntryAck],
    reported_acks: &[SyncedEntryAck],
) -> Result<()> {
    let mut unmatched = expected_acks.to_vec();
    for reported_ack in reported_acks {
        let Some(position) = unmatched
            .iter()
            .position(|expected_ack| expected_ack == reported_ack)
        else {
            return Err(Error::InvalidMessage(
                "storage sync report ack was not pending".to_string(),
            ));
        };
        unmatched.swap_remove(position);
    }
    Ok(())
}

fn storage_sync_destination_accepts_placement(
    destination: StorageSyncDestination,
    placement: Did,
) -> bool {
    match destination {
        StorageSyncDestination::PhysicalOwner(_) => true,
        StorageSyncDestination::PlacementKey(key) => key == placement,
    }
}

enum StorageSyncBatchPhase {
    Validate,
    Persist,
}

pub(crate) enum StorageSyncBatchStep {
    Pending,
    Complete(Vec<SyncedEntryAck>),
}

pub(crate) struct StorageSyncBatch<'data> {
    /// Admission time shared by validation and persistence, so a value admitted in the
    /// validate phase cannot expire between phases and fail the batch half-written.
    now_ms: u128,
    /// The authenticated sender of the batch (its transaction signer), for the relocation law of
    /// relay inboxes.
    sender: Did,
    purpose: StorageSyncPurpose,
    destination: StorageSyncDestination,
    data: &'data [PlacedEntry],
    validate_index: usize,
    accepted: Vec<SyncedEntryAck>,
    persist_index: usize,
    phase: StorageSyncBatchPhase,
}

impl<'data> StorageSyncBatch<'data> {
    pub(crate) fn new(msg: &'data SyncEntriesWithSuccessor, sender: Did, now_ms: u128) -> Self {
        Self {
            now_ms,
            sender,
            purpose: msg.purpose,
            destination: msg.destination,
            data: &msg.data,
            validate_index: 0,
            accepted: Vec::with_capacity(msg.data.len()),
            persist_index: 0,
            phase: StorageSyncBatchPhase::Validate,
        }
    }

    async fn run(mut self, transport: &SwarmTransport) -> Result<Vec<SyncedEntryAck>> {
        let mut persistence_steps_without_yield = 0usize;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        let mut progress_probe_epoch = None;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        let mut progress_witness_recorded = false;
        loop {
            let previous_persist_index = self.persist_index;
            let step = self.step(transport).await?;
            let persisted = self.persist_index > previous_persist_index;
            if persisted && !per_entry_yield_enabled() {
                persistence_steps_without_yield = persistence_steps_without_yield.saturating_add(1);
                #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                if persistence_steps_without_yield == 1 {
                    progress_probe_epoch = crate::simulation::signal_storage_progress_probe();
                } else if progress_probe_epoch.is_some()
                    && progress_probe_epoch == crate::simulation::storage_progress_epoch()
                {
                    crate::simulation::record_protection_violation(
                        crate::simulation::ProtectionLayer::PerEntryYield,
                    );
                }
            }
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            if persisted
                && per_entry_yield_enabled()
                && progress_probe_epoch.is_none()
                && !progress_witness_recorded
            {
                progress_probe_epoch = crate::simulation::signal_storage_progress_probe();
            }
            match step {
                StorageSyncBatchStep::Pending => {
                    if per_entry_yield_enabled() {
                        yield_core_actor_step().await;
                        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                        if persisted {
                            crate::simulation::record_storage_actor_yield(transport.dht.did);
                        }
                        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                        if let Some(epoch_before_yield) = progress_probe_epoch.take() {
                            if crate::simulation::storage_progress_epoch()
                                .is_some_and(|epoch| epoch > epoch_before_yield)
                            {
                                crate::simulation::record_storage_progress_between_entries();
                                progress_witness_recorded = true;
                            } else {
                                crate::simulation::record_protection_violation(
                                    crate::simulation::ProtectionLayer::PerEntryYield,
                                );
                            }
                        }
                        persistence_steps_without_yield = 0;
                    }
                }
                StorageSyncBatchStep::Complete(acks) => return Ok(acks),
            }
        }
    }

    pub(crate) async fn step(
        &mut self,
        transport: &SwarmTransport,
    ) -> Result<StorageSyncBatchStep> {
        match self.phase {
            StorageSyncBatchPhase::Validate => match self.validate_one(transport)? {
                Some(step) => Ok(step),
                None => self.persist_one(transport).await,
            },
            StorageSyncBatchPhase::Persist => self.persist_one(transport).await,
        }
    }

    fn validate_one(&mut self, transport: &SwarmTransport) -> Result<Option<StorageSyncBatchStep>> {
        let Some(placed) = self.data.get(self.validate_index) else {
            self.phase = StorageSyncBatchPhase::Persist;
            return Ok(None);
        };
        self.validate_index += 1;

        // Preservation: every input is validated before the first storage
        // effect, so a later invalid input leaves the entire batch unwritten.
        // The admission law is re-checked by `join_storage_entry` at persist
        // time; it is evaluated here so that a failing entry rejects the batch
        // before any earlier entry has been written.
        if should_persist_synced_entry(transport, self.destination, placed)?
            && self.relay_relocation_permits(transport, placed)?
        {
            placed.validate_placement(transport.storage_redundancy())?;
            placed
                .entry
                .validate_admissible_at(self.now_ms, transport.network_id)?;
            let entry = placed.entry.clone().try_into_storage_entry()?;
            self.accepted.push(SyncedEntryAck::new(placed.key, entry));
        }

        Ok(Some(StorageSyncBatchStep::Pending))
    }

    /// Whether the relocation law lets this batch carry `placed`: a data topic always, a relay
    /// inbox only as an ownership hand-off from the receiver's predecessor.
    ///
    /// Post: `false` skips the entry without acknowledging it, so its owner keeps it and offers
    /// it again on a later pass; it does not fail the batch, as the entry is not invalid, only
    /// not this receiver's to take yet.
    fn relay_relocation_permits(
        &self,
        transport: &SwarmTransport,
        placed: &PlacedEntry,
    ) -> Result<bool> {
        if !placed.entry.kind.is_relay_inbox() {
            return Ok(true);
        }
        if !self.purpose.is_ownership_handoff() {
            return Ok(false);
        }
        let predecessor = transport
            .dht
            .with_topology_state(|state| state.predecessor)?;
        Ok(validate_inbox_relocation(self.sender, predecessor).is_ok())
    }

    async fn persist_one(&mut self, transport: &SwarmTransport) -> Result<StorageSyncBatchStep> {
        let Some(ack) = self.accepted.get(self.persist_index) else {
            return Ok(self.complete());
        };
        let key = ack.key;
        let entry = ack.entry.clone();

        transport
            .dht
            .join_storage_entry(self.now_ms, key, entry)
            .await?;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::record_storage_persisted(transport.dht.did);
        self.persist_index += 1;

        if self.persist_index == self.accepted.len() {
            Ok(self.complete())
        } else {
            Ok(StorageSyncBatchStep::Pending)
        }
    }

    fn complete(&mut self) -> StorageSyncBatchStep {
        StorageSyncBatchStep::Complete(mem::take(&mut self.accepted))
    }
}

fn per_entry_yield_enabled() -> bool {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    {
        crate::simulation::protection_profile().per_entry_yield()
    }
    #[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
    {
        true
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
#[test]
fn test_per_entry_yield_ablation_reaches_real_storage_batch_policy() {
    assert!(per_entry_yield_enabled());
    let _runtime = crate::simulation::SimulationRuntimeGuard::enter(
        44,
        1_700_000_000_000,
        crate::simulation::ProtectionProfile::without_per_entry_yield(),
    )
    .expect("simulation runtime must install");
    assert!(!per_entry_yield_enabled());
}

fn should_persist_synced_entry(
    transport: &SwarmTransport,
    destination: StorageSyncDestination,
    placed: &PlacedEntry,
) -> Result<bool> {
    if !storage_sync_destination_accepts_placement(destination, placed.key) {
        return Ok(false);
    }

    match transport
        .dht
        .find_storage_owner_for(placed.key, placed.entry.kind)?
    {
        // Invariant: `Some(_)` is the local-storage branch. In non-virtual
        // Chord storage the DID carried by `Some` is the successor witness used
        // for fallback lookup, not a remote-owner denial.
        PeerRingAction::Some(_) => Ok(true),
        PeerRingAction::RemoteAction(_, PeerRingRemoteAction::FindSuccessor(_)) => Ok(false),
        action => Err(Error::unexpected_peer_ring_action(action)),
    }
}

// Post: pending.len() < STORAGE_SYNC_ACK_CAPACITY.
// Preservation: evicting an old pending capability before inserting a new one
// can only make that old report fail validation; it cannot make an unproven
// report delete local storage.
fn evict_storage_sync_acks(pending: &mut StorageSyncAckMap) {
    while pending.len() >= STORAGE_SYNC_ACK_CAPACITY {
        let Some(stale_key) = pending
            .iter()
            .min_by_key(|(tx_id, capability)| (capability.recorded_at_ms, **tx_id))
            .map(|(tx_id, _)| *tx_id)
        else {
            break;
        };
        pending.remove(&stale_key);
    }
}

impl SwarmTransport {
    /// Validate a complete local storage-sync batch before persisting any entry.
    ///
    /// Pre: `sender` is the authenticated signer of the message carrying `msg`, never a value
    /// read from its relay path.
    pub(crate) async fn persist_storage_sync_entries(
        &self,
        msg: &SyncEntriesWithSuccessor,
        sender: Did,
    ) -> Result<Vec<SyncedEntryAck>> {
        StorageSyncBatch::new(msg, sender, get_epoch_ms())
            .run(self)
            .await
    }

    /// Record the exact ack capability created by an outbound storage-sync payload.
    ///
    /// Pre: `tx_id` is the transaction id of the payload whose message data is
    /// `SyncEntriesWithSuccessor { purpose, destination, data }`.
    /// Pre: `purpose.permits_source_cleanup()`.
    /// Pre: `route_next_hop` is the `PeerRing::next_hop_for_storage_sync`
    /// result used as that payload's relay next-hop.
    /// Post: a later report for `tx_id` can delete local storage only if its
    /// receiver, destination, and ack values are justified by this recorded
    /// payload and storage-route proof.
    pub(crate) fn record_pending_storage_sync_ack(
        &self,
        tx_id: uuid::Uuid,
        purpose: StorageSyncPurpose,
        destination: StorageSyncDestination,
        route_next_hop: Did,
        data: &[PlacedEntry],
    ) -> Result<()> {
        if !purpose.permits_source_cleanup() {
            return Err(Error::InvalidMessage(
                "storage sync purpose does not permit pending cleanup ack".to_string(),
            ));
        }
        let capability = StorageSyncAckCapability {
            recorded_at_ms: storage_sync_ack_now_ms(),
            purpose,
            destination,
            expected_receiver: expected_storage_sync_receiver(destination, route_next_hop),
            expected_acks: expected_sync_acks(data)?,
        };
        let mut pending = self
            .pending_storage_sync_acks
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        evict_storage_sync_acks(&mut pending);
        pending.insert(tx_id, capability);
        Ok(())
    }

    fn remove_pending_storage_sync_ack(&self, tx_id: uuid::Uuid) {
        if let Ok(mut pending) = self.pending_storage_sync_acks.lock() {
            pending.remove(&tx_id);
        }
    }

    /// Consume a pending storage-sync ack capability.
    ///
    /// Pre: transaction and payload signatures have been verified before message
    /// dispatch.
    /// Post: `Ok(acks)` implies the report signer matches the report receiver,
    /// the receiver is admitted by the send-time route proof, and every
    /// returned ack was present in the outbound sync payload for `tx_id`.
    pub(crate) fn take_pending_storage_sync_ack(
        &self,
        tx_id: uuid::Uuid,
        signer: Did,
        report: &SyncEntriesWithSuccessorReport,
    ) -> Result<Vec<SyncedEntryAck>> {
        if signer != report.receiver {
            return Err(Error::InvalidMessage(
                "storage sync report signer does not match receiver".to_string(),
            ));
        }
        if !report.purpose.permits_source_cleanup() {
            return Err(Error::InvalidMessage(
                "storage sync report purpose does not permit source cleanup".to_string(),
            ));
        }

        let mut pending = self
            .pending_storage_sync_acks
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        let Some(capability) = pending.get(&tx_id) else {
            return Err(Error::InvalidMessage(
                "storage sync report has no pending capability".to_string(),
            ));
        };
        if capability.purpose != report.purpose {
            return Err(Error::InvalidMessage(
                "storage sync report purpose does not match pending sync".to_string(),
            ));
        }
        if capability.destination != report.destination {
            return Err(Error::InvalidMessage(
                "storage sync report destination does not match pending sync".to_string(),
            ));
        }
        if capability.expected_receiver != signer {
            return Err(Error::InvalidMessage(
                "storage sync report receiver does not match pending sync".to_string(),
            ));
        }
        validate_report_acks(&capability.expected_acks, &report.acks)?;

        let acks = report.acks.clone();
        pending.remove(&tx_id);
        Ok(acks)
    }

    async fn send_storage_sync_with_completion(
        &self,
        msg: SyncEntriesWithSuccessor,
        completion: OutboundCompletion,
    ) -> Result<StorageSyncCompletion> {
        let destination = msg.destination.did();
        let Some(next_hop) = self
            .dht
            .next_hop_for_storage_sync(msg.destination)?
            .filter(|next_hop| *next_hop != self.dht.did)
        else {
            self.persist_storage_sync_entries(&msg, self.dht.did)
                .await?;
            return Ok(StorageSyncCompletion::PersistedLocally);
        };
        let payload = MessagePayload::new_send(
            Message::SyncEntriesWithSuccessor(msg.clone()),
            self.message_signer(),
            next_hop,
            destination,
        )?;
        let tx_id = payload.transaction.tx_id;
        let records_cleanup_ack = msg.purpose.permits_source_cleanup();
        if records_cleanup_ack {
            self.record_pending_storage_sync_ack(
                tx_id,
                msg.purpose,
                msg.destination,
                next_hop,
                &msg.data,
            )?;
        }
        let send_outcome = match completion {
            OutboundCompletion::Detached => self.send_payload_detached_with_outcome(payload).await,
            OutboundCompletion::Tracked => self.send_payload_tracked(payload).await,
        };
        match send_outcome {
            Ok(SendCompletionOutcome::Succeeded) => Ok(StorageSyncCompletion::Submitted {
                tx_id,
                outcome: SendCompletionOutcome::Succeeded,
            }),
            Ok(SendCompletionOutcome::Cancelled) => {
                if records_cleanup_ack {
                    self.remove_pending_storage_sync_ack(tx_id);
                }
                Ok(StorageSyncCompletion::Submitted {
                    tx_id,
                    outcome: SendCompletionOutcome::Cancelled,
                })
            }
            Err(error) => {
                if records_cleanup_ack {
                    self.remove_pending_storage_sync_ack(tx_id);
                }
                if error.is_deferrable_data_plane_send() {
                    Ok(StorageSyncCompletion::Submitted {
                        tx_id,
                        outcome: SendCompletionOutcome::Cancelled,
                    })
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Send a storage-sync payload and register cleanup acks only for hand-off sync.
    ///
    /// The result distinguishes local persistence, remote submission, and a
    /// cancelled data-plane admission that maintenance must recompute and retry.
    pub(crate) async fn send_storage_sync(
        &self,
        msg: SyncEntriesWithSuccessor,
    ) -> Result<StorageSyncOutcome> {
        match self
            .send_storage_sync_with_completion(msg, OutboundCompletion::Detached)
            .await?
        {
            StorageSyncCompletion::PersistedLocally => Ok(StorageSyncOutcome::PersistedLocally),
            StorageSyncCompletion::Submitted {
                tx_id,
                outcome: SendCompletionOutcome::Succeeded,
            } => Ok(StorageSyncOutcome::Sent(tx_id)),
            StorageSyncCompletion::Submitted {
                outcome: SendCompletionOutcome::Cancelled,
                ..
            } => Ok(StorageSyncOutcome::Deferred),
        }
    }

    /// Send storage repair and wait until every frame has completed or cancelled.
    pub(crate) async fn send_storage_sync_tracked(
        &self,
        msg: SyncEntriesWithSuccessor,
    ) -> Result<TrackedStorageSyncOutcome> {
        match self
            .send_storage_sync_with_completion(msg, OutboundCompletion::Tracked)
            .await?
        {
            StorageSyncCompletion::PersistedLocally => {
                Ok(TrackedStorageSyncOutcome::PersistedLocally)
            }
            StorageSyncCompletion::Submitted {
                tx_id,
                outcome: SendCompletionOutcome::Succeeded,
            } => Ok(TrackedStorageSyncOutcome::Delivered(tx_id)),
            StorageSyncCompletion::Submitted {
                outcome: SendCompletionOutcome::Cancelled,
                ..
            } => Ok(TrackedStorageSyncOutcome::Deferred),
        }
    }

    /// Send storage sync as a deferrable data-plane effect.
    ///
    /// Backpressure, connection replacement, transport readiness loss, or a
    /// vanished route means this anti-entropy payload was not accepted. These
    /// are not DHT safety failures and may not bubble through message callbacks
    /// as failed control-plane events.
    pub(crate) async fn send_storage_sync_or_defer(
        &self,
        msg: SyncEntriesWithSuccessor,
        context: &'static str,
    ) -> Result<StorageSyncOutcome> {
        let purpose = msg.purpose;
        let destination = msg.destination;
        let destination_did = destination.did();
        let entries = msg.data.len();
        let next_hop = self
            .dht
            .next_hop_for_storage_sync(destination)
            .ok()
            .flatten();
        let next_hop_state = next_hop
            .and_then(|did| self.get_connection(did))
            .map(|conn| conn.webrtc_connection_state());

        let outcome = self.send_storage_sync(msg).await?;
        if outcome.is_deferred() {
            tracing::warn!(
                target: "rings_core::storage_sync",
                local = %self.dht.did,
                context,
                purpose = ?purpose,
                destination = ?destination,
                destination_did = %destination_did,
                next_hop = ?next_hop,
                next_hop_state = ?next_hop_state,
                entries,
                "storage sync data-plane send cancelled and deferred"
            );
        }
        Ok(outcome)
    }
}
