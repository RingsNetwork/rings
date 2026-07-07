use std::collections::BTreeMap;

use chrono::Utc;

use super::SwarmTransport;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::Did;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::SyncEntriesWithSuccessor;
use crate::message::SyncEntriesWithSuccessorReport;

const STORAGE_SYNC_ACK_CAPACITY: usize = 1024;

pub(super) type StorageSyncAckMap = BTreeMap<uuid::Uuid, StorageSyncAckCapability>;

pub(super) struct StorageSyncAckCapability {
    recorded_at_ms: i64,
    purpose: StorageSyncPurpose,
    destination: StorageSyncDestination,
    receiver_proof: StorageSyncReceiverProof,
    expected_acks: Vec<SyncedEntryAck>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageSyncReceiverProof {
    PhysicalOwner(Did),
    StorageRouteNextHop(Did),
}

impl StorageSyncReceiverProof {
    // Invariant: this proof records exactly the receiver identity the sender can
    // justify at send time. PhysicalOwner is a final identity. PlacementKey is
    // only the storage-route next hop visible from the sender; reports from
    // farther nodes are not allowed to delete local storage at this boundary.
    fn from_destination(destination: StorageSyncDestination, route_next_hop: Did) -> Self {
        match destination {
            StorageSyncDestination::PhysicalOwner(owner) => Self::PhysicalOwner(owner),
            StorageSyncDestination::PlacementKey(_) => Self::StorageRouteNextHop(route_next_hop),
        }
    }

    fn permits(self, receiver: Did) -> bool {
        match self {
            Self::PhysicalOwner(owner) | Self::StorageRouteNextHop(owner) => owner == receiver,
        }
    }
}

fn storage_sync_ack_now_ms() -> i64 {
    Utc::now().timestamp_millis()
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
            receiver_proof: StorageSyncReceiverProof::from_destination(destination, route_next_hop),
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
        if !capability.receiver_proof.permits(signer) {
            return Err(Error::InvalidMessage(
                "storage sync report receiver does not match pending sync".to_string(),
            ));
        }
        validate_report_acks(&capability.expected_acks, &report.acks)?;

        let acks = report.acks.clone();
        pending.remove(&tx_id);
        Ok(acks)
    }

    /// Send a storage-sync payload and register cleanup acks only for hand-off sync.
    pub(crate) async fn send_storage_sync(
        &self,
        msg: SyncEntriesWithSuccessor,
    ) -> Result<uuid::Uuid> {
        let destination = msg.destination.did();
        let next_hop = self
            .dht
            .next_hop_for_storage_sync(msg.destination)?
            .ok_or_else(|| {
                Error::InvalidMessage(
                    "storage sync destination resolves to local branch at send boundary"
                        .to_string(),
                )
            })?;
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
        if let Err(e) = self.send_payload(payload).await {
            if records_cleanup_ack {
                self.remove_pending_storage_sync_ack(tx_id);
            }
            return Err(e);
        }
        Ok(tx_id)
    }
}
