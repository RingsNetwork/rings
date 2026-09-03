use super::Stabilizer;
use super::STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS;
use super::STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP;
use crate::dht::types::ChordStorageRepair;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::StorageSyncDelivery;
use crate::error::Error;
use crate::error::Result;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::TrackedStorageSyncOutcome;
use crate::swarm::transport::TransportReadiness;
use crate::utils::get_epoch_ms_i64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Observable completion state for one bounded storage repair pass.
pub enum StorageRepairOutcome {
    /// Every selected delivery completed.
    Complete,
    /// At least one delivery was deferred; repair remains pending for a later pass.
    Deferred,
}

impl StorageRepairOutcome {
    /// Return whether this pass completed all selected deliveries.
    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

struct PlannedStorageRepairDelivery {
    delivery: StorageSyncDelivery,
}

#[derive(Clone, Copy, Debug)]
enum StorageRepairDeferReason {
    MissingNextHop,
    NextHopNotAdmitted,
    NextHopTransportMissing,
    NextHopTransportNotReady(TransportReadiness),
    NextHopFresh { connected_for_ms: i64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepairDeliveryResult {
    Sent,
    Deferred,
}

impl StorageRepairDeferReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingNextHop => "missing_next_hop",
            Self::NextHopNotAdmitted => "next_hop_not_admitted",
            Self::NextHopTransportMissing => "next_hop_transport_missing",
            Self::NextHopTransportNotReady(_) => "next_hop_transport_not_ready",
            Self::NextHopFresh { .. } => "next_hop_fresh",
        }
    }

    const fn connected_for_ms(self) -> Option<i64> {
        match self {
            Self::NextHopFresh { connected_for_ms } => Some(connected_for_ms),
            _ => None,
        }
    }

    const fn transport_readiness(self) -> Option<TransportReadiness> {
        match self {
            Self::NextHopTransportNotReady(readiness) => Some(readiness),
            _ => None,
        }
    }
}

fn is_storage_repair_deferral(error: &Error) -> bool {
    error.is_deferrable_data_plane_send()
}

impl Stabilizer {
    async fn handle_storage_repair_action(
        &self,
        act: PeerRingAction,
    ) -> Result<StorageRepairOutcome> {
        let deliveries = self.storage_repair_window(act.coalesced_storage_sync_deliveries()?)?;
        if deliveries.is_empty() {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                "STABILIZATION storage repair has no deliveries"
            );
            return Ok(StorageRepairOutcome::Complete);
        }

        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            deliveries = deliveries.len(),
            "STABILIZATION storage repair deliveries prepared"
        );

        let mut sent = 0usize;
        let mut deferred = 0usize;
        for planned in deliveries {
            match self
                .send_planned_storage_repair(planned, get_epoch_ms_i64())
                .await?
            {
                RepairDeliveryResult::Sent => sent = sent.saturating_add(1),
                RepairDeliveryResult::Deferred => deferred = deferred.saturating_add(1),
            }
        }

        if deferred > 0 {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                sent,
                deferred,
                "STABILIZATION storage repair deliveries finished with deferrals"
            );
            return Ok(StorageRepairOutcome::Deferred);
        }
        Ok(StorageRepairOutcome::Complete)
    }

    async fn send_planned_storage_repair(
        &self,
        planned: PlannedStorageRepairDelivery,
        now_ms: i64,
    ) -> Result<RepairDeliveryResult> {
        let msg = SyncEntriesWithSuccessor::from_delivery(planned.delivery);
        let purpose = msg.purpose;
        let destination = msg.destination;
        let entries = msg.data.len();
        let next_hop = self.dht.next_hop_for_storage_sync(destination)?;
        if let Some(reason) = self.storage_repair_defer_reason(next_hop, now_ms)? {
            self.log_storage_repair_deferred(destination, next_hop, entries, reason, None);
            return Ok(RepairDeliveryResult::Deferred);
        }
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            purpose = ?purpose,
            destination = ?destination,
            next_hop = ?next_hop,
            entries,
            "STABILIZATION storage repair send start"
        );
        match self.transport.send_storage_sync_tracked(msg).await {
            Ok(TrackedStorageSyncOutcome::Delivered(tx_id)) => {
                #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                crate::simulation::record_repair_entries(entries);
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    tx_id = %tx_id,
                    destination = ?destination,
                    next_hop = ?next_hop,
                    entries,
                    "STABILIZATION storage repair send complete"
                );
                Ok(RepairDeliveryResult::Sent)
            }
            Ok(TrackedStorageSyncOutcome::PersistedLocally) => {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    destination = ?destination,
                    entries,
                    "STABILIZATION storage repair persisted locally"
                );
                Ok(RepairDeliveryResult::Sent)
            }
            Ok(TrackedStorageSyncOutcome::Deferred) => {
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    destination = ?destination,
                    next_hop = ?next_hop,
                    entries,
                    "STABILIZATION storage repair delivery cancelled and deferred"
                );
                Ok(RepairDeliveryResult::Deferred)
            }
            Err(error) if is_storage_repair_deferral(&error) => {
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    destination = ?destination,
                    next_hop = ?next_hop,
                    entries,
                    error = ?error,
                    "STABILIZATION storage repair deferred by transport readiness"
                );
                Ok(RepairDeliveryResult::Deferred)
            }
            Err(error) => Err(error),
        }
    }

    fn log_storage_repair_deferred(
        &self,
        destination: crate::dht::StorageSyncDestination,
        next_hop: Option<Did>,
        entries: usize,
        reason: StorageRepairDeferReason,
        error: Option<&Error>,
    ) {
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            destination = ?destination,
            next_hop = ?next_hop,
            entries,
            reason = reason.as_str(),
            transport_readiness = ?reason.transport_readiness(),
            connected_for_ms = ?reason.connected_for_ms(),
            grace_ms = STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS,
            error = ?error,
            "STABILIZATION storage repair deferred"
        );
    }

    fn storage_repair_window(
        &self,
        deliveries: Vec<StorageSyncDelivery>,
    ) -> Result<Vec<PlannedStorageRepairDelivery>> {
        let total = deliveries.len();
        if total == 0 {
            return Ok(Vec::new());
        }

        let mut keyed = deliveries
            .into_iter()
            .map(|delivery| (delivery.cursor_key(), delivery))
            .collect::<Vec<_>>();
        keyed.sort_by(|(left, _), (right, _)| left.cmp(right));
        let ordered = keyed
            .iter()
            .map(|(cursor, _)| cursor.clone())
            .collect::<Vec<_>>();
        let start = self
            .transport
            .storage_repair_window_start(&ordered, STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP)?;
        keyed.rotate_left(start);
        keyed.truncate(STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP);
        let next_cursor = keyed.last().map(|(cursor, _)| cursor.clone());
        let selected = keyed
            .into_iter()
            .map(|(_, delivery)| PlannedStorageRepairDelivery { delivery })
            .collect::<Vec<_>>();
        if let Some(cursor) = next_cursor {
            self.transport.advance_storage_repair_cursor(cursor)?;
        }
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            total_deliveries = total,
            selected_deliveries = selected.len(),
            start,
            "STABILIZATION storage repair delivery window selected"
        );
        Ok(selected)
    }

    fn storage_repair_defer_reason(
        &self,
        next_hop: Option<Did>,
        now_ms: i64,
    ) -> Result<Option<StorageRepairDeferReason>> {
        let Some(next_hop) = next_hop else {
            return Ok(Some(StorageRepairDeferReason::MissingNextHop));
        };
        if !self.transport.is_admitted_connection(next_hop) {
            return Ok(Some(StorageRepairDeferReason::NextHopNotAdmitted));
        }
        let Some(next_hop_connection) = self.transport.admitted_connection(next_hop)? else {
            return Ok(Some(StorageRepairDeferReason::NextHopTransportMissing));
        };
        let readiness = next_hop_connection.readiness();
        if !readiness.can_make_progress() {
            return Ok(Some(StorageRepairDeferReason::NextHopTransportNotReady(
                readiness,
            )));
        }
        if let Some(connected_for_ms) = self.peer_connected_for_ms(next_hop, now_ms) {
            if connected_for_ms < STORAGE_REPAIR_FRESH_CONNECTION_GRACE_MS {
                return Ok(Some(StorageRepairDeferReason::NextHopFresh {
                    connected_for_ms,
                }));
            }
        }

        Ok(None)
    }

    fn peer_connected_for_ms(&self, peer: Did, now_ms: i64) -> Option<i64> {
        match self.transport.peer_connected_for_ms(peer, now_ms) {
            Ok(age) => age,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    peer = %peer,
                    error = %error,
                    "STABILIZATION storage repair connection age check failed"
                );
                None
            }
        }
    }

    /// Republish locally-held entries to their current affine owners.
    pub async fn repair_storage(&self) -> Result<StorageRepairOutcome> {
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            redundancy = self.transport.storage_redundancy(),
            "STABILIZATION repair_storage republish start"
        );
        let action = self
            .dht
            .republish_local_entries(self.transport.storage_redundancy())
            .await?;
        let (action_kind, action_count) = match &action {
            PeerRingAction::None => ("None", 0),
            PeerRingAction::Some(_) => ("Some", 1),
            PeerRingAction::SomeEntry(_) => ("SomeEntry", 1),
            PeerRingAction::EntryMisses(misses) => ("EntryMisses", misses.len()),
            PeerRingAction::RemoteAction(_, _) => ("RemoteAction", 1),
            PeerRingAction::MultiActions(actions) => ("MultiActions", actions.len()),
        };
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            action_kind,
            action_count,
            "STABILIZATION repair_storage republish action prepared"
        );
        let outcome = self.handle_storage_repair_action(action).await?;
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            "STABILIZATION repair_storage republish complete"
        );
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::dht::entry::EntryKind;
    use crate::dht::entry::PlacedEntry;
    use crate::dht::StorageSyncDestination;
    use crate::ecc::SecretKey;
    use crate::session::SessionSk;
    use crate::storage::MemStorage;
    use crate::swarm::SwarmBuilder;

    fn repair_deliveries(values: &[u32]) -> Result<Vec<StorageSyncDelivery>> {
        let actions = values
            .iter()
            .copied()
            .map(|value| {
                let destination = StorageSyncDestination::PlacementKey(Did::from(value));
                let entry =
                    crate::tests::live_entry(Did::from(value + 100), vec![], EntryKind::Data);
                PeerRingAction::sync_entries_for_repair(destination, vec![PlacedEntry::new(
                    destination.did(),
                    entry,
                )])
            })
            .collect();
        PeerRingAction::MultiActions(actions).coalesced_storage_sync_deliveries()
    }

    fn selected_destination(stabilizer: &Stabilizer, values: &[u32]) -> Result<Did> {
        let mut selected = stabilizer.storage_repair_window(repair_deliveries(values)?)?;
        let Some(planned) = selected.pop() else {
            return Err(crate::error::Error::InvalidMessage(
                "repair window selected no delivery".to_string(),
            ));
        };
        let destination = planned.delivery.into_message_parts().1.did();
        Ok(destination)
    }

    #[test]
    fn test_changing_delivery_sets_preserve_repair_progress_across_stabilizers() -> Result<()> {
        let session = SessionSk::new_with_seckey(&SecretKey::random())?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                "stun://stun.l.google.com:19302",
                Box::new(MemStorage::new()),
                session,
            )
            .build(),
        );

        let selected = [
            selected_destination(&swarm.stabilizer()?, &[1, 2, 3])?,
            selected_destination(&swarm.stabilizer()?, &[2, 3])?,
            selected_destination(&swarm.stabilizer()?, &[1, 2, 3])?,
            selected_destination(&swarm.stabilizer()?, &[1, 2, 3])?,
        ];

        assert_eq!(selected, [
            Did::from(1u32),
            Did::from(2u32),
            Did::from(3u32),
            Did::from(1u32),
        ]);
        Ok(())
    }

    #[test]
    fn test_deferred_delivery_rotates_without_losing_its_retry() -> Result<()> {
        let session = SessionSk::new_with_seckey(&SecretKey::random())?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                "stun://stun.l.google.com:19302",
                Box::new(MemStorage::new()),
                session,
            )
            .build(),
        );
        let stabilizer = swarm.stabilizer()?;

        let selected = [
            selected_destination(&stabilizer, &[1, 2])?,
            selected_destination(&stabilizer, &[1, 2])?,
            selected_destination(&stabilizer, &[1, 2])?,
        ];

        assert_eq!(selected, [
            Did::from(1u32),
            Did::from(2u32),
            Did::from(1u32)
        ]);
        Ok(())
    }
}
