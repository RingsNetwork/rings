use std::collections::BTreeMap;

use super::delivery::SendCompletionOutcome;
use super::SwarmTransport;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::SyncEntriesWithSuccessor;
use crate::message::SyncEntriesWithSuccessorReport;
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

#[derive(Clone, Copy)]
enum StorageSyncCompletion {
    Detached,
    Tracked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrackedStorageSyncOutcome {
    Delivered(uuid::Uuid),
    Deferred,
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
    async fn apply_local_storage_sync(&self, msg: &SyncEntriesWithSuccessor) -> Result<()> {
        for placed in msg.data.iter() {
            if !storage_sync_destination_accepts_placement(msg.destination, placed.key) {
                continue;
            }

            match self.dht.find_storage_owner(placed.key)? {
                PeerRingAction::Some(_) => {
                    placed.validate_placement(self.storage_redundancy())?;
                    self.dht
                        .join_storage_entry(placed.key, placed.entry.clone())
                        .await?;
                }
                PeerRingAction::RemoteAction(_, PeerRingRemoteAction::FindSuccessor(_)) => {}
                action => return Err(Error::unexpected_peer_ring_action(action)),
            }
        }
        Ok(())
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
            .map_err(|_| Error::DHTSyncLockError)?;
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
            .map_err(|_| Error::DHTSyncLockError)?;
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
        completion: StorageSyncCompletion,
    ) -> Result<(uuid::Uuid, SendCompletionOutcome)> {
        let destination = msg.destination.did();
        let Some(next_hop) = self.dht.next_hop_for_storage_sync(msg.destination)? else {
            self.apply_local_storage_sync(&msg).await?;
            return Ok((uuid::Uuid::new_v4(), SendCompletionOutcome::Succeeded));
        };
        if next_hop == self.dht.did {
            self.apply_local_storage_sync(&msg).await?;
            return Ok((uuid::Uuid::new_v4(), SendCompletionOutcome::Succeeded));
        }
        let payload = MessagePayload::new_send(
            Message::SyncEntriesWithSuccessor(msg.clone()),
            self.session_sk(),
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
            StorageSyncCompletion::Detached => {
                self.send_payload_detached_with_outcome(payload).await
            }
            StorageSyncCompletion::Tracked => self.send_payload_tracked(payload).await,
        };
        match send_outcome {
            Ok(SendCompletionOutcome::Succeeded) => Ok((tx_id, SendCompletionOutcome::Succeeded)),
            Ok(SendCompletionOutcome::Cancelled) => {
                if records_cleanup_ack {
                    self.remove_pending_storage_sync_ack(tx_id);
                }
                Ok((tx_id, SendCompletionOutcome::Cancelled))
            }
            Err(error) => {
                if records_cleanup_ack {
                    self.remove_pending_storage_sync_ack(tx_id);
                }
                if error.is_deferrable_data_plane_send() {
                    Ok((tx_id, SendCompletionOutcome::Cancelled))
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Send a storage-sync payload and register cleanup acks only for hand-off sync.
    ///
    /// `Ok(None)` means the data-plane admission or tracked delivery was
    /// cancelled, so maintenance must recompute and retry from current topology.
    pub(crate) async fn send_storage_sync(
        &self,
        msg: SyncEntriesWithSuccessor,
    ) -> Result<Option<uuid::Uuid>> {
        match self
            .send_storage_sync_with_completion(msg, StorageSyncCompletion::Detached)
            .await?
        {
            (tx_id, SendCompletionOutcome::Succeeded) => Ok(Some(tx_id)),
            (_, SendCompletionOutcome::Cancelled) => Ok(None),
        }
    }

    /// Send storage repair and wait until every frame has completed or cancelled.
    pub(crate) async fn send_storage_sync_tracked(
        &self,
        msg: SyncEntriesWithSuccessor,
    ) -> Result<TrackedStorageSyncOutcome> {
        match self
            .send_storage_sync_with_completion(msg, StorageSyncCompletion::Tracked)
            .await?
        {
            (tx_id, SendCompletionOutcome::Succeeded) => {
                Ok(TrackedStorageSyncOutcome::Delivered(tx_id))
            }
            (_, SendCompletionOutcome::Cancelled) => Ok(TrackedStorageSyncOutcome::Deferred),
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
    ) -> Result<Option<uuid::Uuid>> {
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
        if outcome.is_none() {
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
