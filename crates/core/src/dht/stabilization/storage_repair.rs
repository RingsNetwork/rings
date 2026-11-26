use futures::future::select;
use futures::future::Either;

use super::Stabilizer;
use super::STABILIZATION_STOP_POLL_INTERVAL;
use super::STORAGE_REPAIR_MAX_DELIVERIES_PER_STEP;
use crate::dht::topology;
use crate::dht::types::Chord;
use crate::dht::types::ChordStorageRepair;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::StorageSyncDelivery;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::handlers::inbox::send_relay_hold;
use crate::message::handlers::relay_repair::offer_relay_repair;
use crate::message::handlers::relay_replica::resume_relay_replications;
use crate::message::AnnounceRelayHolder;
use crate::message::DeclareRelayCustody;
use crate::message::FindRelayHolder;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::message::PreviousRelayHolder;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::transport::StorageSyncOutcome;
use crate::swarm::transport::TransportReadiness;
use crate::swarm::transport::CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS;
use crate::utils::try_sleep;
use crate::utils::Instant;

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

#[derive(Clone, Copy, Debug)]
enum StorageRepairDeferReason {
    MissingNextHop,
    NextHopNotAdmitted,
    NextHopTransportMissing,
    NextHopTransportNotReady(TransportReadiness),
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
        }
    }

    const fn transport_readiness(self) -> Option<TransportReadiness> {
        match self {
            Self::NextHopTransportNotReady(readiness) => Some(readiness),
            _ => None,
        }
    }
}

/// The chain task's probe period, `CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS` as its monotonic clock
/// counts it.
const CHAIN_PROBE_PERIOD_MS: u64 = CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS.unsigned_abs();

impl Stabilizer {
    async fn handle_storage_repair_action(
        &self,
        act: PeerRingAction,
    ) -> Result<StorageRepairOutcome> {
        let topology_epoch = self.dht.topology_epoch().current();
        let now_ms = self.transport.now_ms_i64();
        let eligible = self.filter_cooled_down_deliveries(
            act.coalesced_storage_sync_deliveries()?,
            topology_epoch,
            now_ms,
        )?;
        let deliveries = self.storage_repair_window(eligible)?;
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
                .send_planned_storage_repair(planned, topology_epoch, now_ms)
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

    fn filter_cooled_down_deliveries(
        &self,
        deliveries: Vec<StorageSyncDelivery>,
        topology_epoch: u64,
        now_ms: i64,
    ) -> Result<Vec<StorageSyncDelivery>> {
        let mut eligible = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            if !self.transport.is_storage_repair_cooled_down(
                &delivery.cursor_key(),
                topology_epoch,
                now_ms,
            )? {
                eligible.push(delivery);
            }
        }
        Ok(eligible)
    }

    async fn send_planned_storage_repair(
        &self,
        delivery: StorageSyncDelivery,
        topology_epoch: u64,
        now_ms: i64,
    ) -> Result<RepairDeliveryResult> {
        let cursor = delivery.cursor_key();
        let msg = SyncEntriesWithSuccessor::from_delivery(delivery);
        let purpose = msg.purpose;
        let destination = msg.destination;
        let entries = msg.data.len();
        let next_hop = self.dht.next_hop_for_storage_sync(destination)?;
        if let Some(reason) = self.storage_repair_defer_reason(next_hop)? {
            self.log_storage_repair_deferred(destination, next_hop, entries, reason);
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
            Ok(StorageSyncOutcome::Sent(tx_id)) => {
                self.transport
                    .record_storage_repair_sent(cursor, topology_epoch, now_ms)?;
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
            Ok(StorageSyncOutcome::PersistedLocally) => {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    destination = ?destination,
                    entries,
                    "STABILIZATION storage repair persisted locally"
                );
                Ok(RepairDeliveryResult::Sent)
            }
            Ok(StorageSyncOutcome::Deferred) => {
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
            Err(error) if error.is_deferrable_data_plane_send() => {
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
    ) {
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            destination = ?destination,
            next_hop = ?next_hop,
            entries,
            reason = reason.as_str(),
            transport_readiness = ?reason.transport_readiness(),
            "STABILIZATION storage repair deferred"
        );
    }

    fn storage_repair_window(
        &self,
        deliveries: Vec<StorageSyncDelivery>,
    ) -> Result<Vec<StorageSyncDelivery>> {
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
            .map(|(_, delivery)| delivery)
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
    ) -> Result<Option<StorageRepairDeferReason>> {
        let Some(next_hop) = next_hop else {
            return Ok(Some(StorageRepairDeferReason::MissingNextHop));
        };
        if self.transport.active_attempt(next_hop)?.is_none() {
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
        Ok(None)
    }

    /// Pass the previous relay holder this node knows (itself, when it holds relay mail) to its
    /// successor head (`dht::relay_holder`), and forget the relay declarations of peers it no
    /// longer deals with. A record that is not queued is passed again by the next pass, as the
    /// periodic repair pass repeats it anyway, so a node that restarted learns its run again.
    async fn maintain_relay_custody(&self) -> Result<()> {
        let mut stale = Vec::new();
        for peer in self.dht.relay_storage_less_peers()? {
            if !self.transport.has_connection_attempt(peer)? {
                stale.push(peer);
            }
        }
        self.dht.forget_stale_relay_custody(&stale)?;
        self.declare_opened_relay_custody().await?;
        self.dht.prune_relay_replications(self.transport.now_ms())?;
        self.resend_unacknowledged_relay_holds().await?;
        self.repair_relay_chain().await?;
        self.ask_owners_of_unplaced_relay_keys().await?;
        let Some((head, previous)) = self.dht.previous_relay_holder_to_pass()? else {
            return Ok(());
        };
        let record = Message::AnnounceRelayHolder(AnnounceRelayHolder {
            previous: previous
                .iter()
                .map(|link| PreviousRelayHolder {
                    holder: link.holder,
                    run_start: link.run_start,
                })
                .collect(),
        });
        if let Err(error) = self
            .transport
            .protocol_egress()
            .send_message(record, head)
            .await
        {
            tracing::debug!(
                target: "rings_core::dht::stabilization",
                local = %self.dht.did,
                %head,
                error = ?error,
                "STABILIZATION previous relay holder not queued; passed again next pass"
            );
        }
        Ok(())
    }

    /// Run the chain exchange at once on every change of this node's chains, until `stop` (`Law
    /// (Event-driven repair)`, #777): a chain neighbour lost, suspected or cleared. It resumes
    /// the holds whose next member changed and offers one digest set to each chain neighbour,
    /// without waiting for the maintenance schedule's repair window, which guards bulk storage
    /// sync and not these two messages. The periodic repair pass remains the backstop. Every
    /// `CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS` it also probes the chain neighbours and suspects one
    /// that has not answered (#1070), which is such a change.
    ///
    /// `Law (Wake)`: the listener is registered before the due flag is checked, so no loss is
    /// missed; `stop` is observed between waits of at most `STABILIZATION_STOP_POLL_INTERVAL`.
    ///
    /// Bound: exchanges per second ≤ chain changes per second (losses, suspicions, clearings).
    /// Each exchange is run by taking the due flag, which only a change sets, and changes that
    /// arrive before it is taken share one exchange. It cannot amplify itself: an exchange sends
    /// digest offers, copies and forwards, removes no peer and probes none, while a change is a
    /// topology removal, an unanswered probe, or an answer, which no exchange causes.
    pub(crate) async fn repair_relay_chain_on_loss(&self, stop: &StopToken) {
        let origin = Instant::now();
        let mut next_probe_ms: u64 = 0;
        loop {
            if stop.should_stop() {
                return;
            }
            let elapsed_ms = u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX);
            if elapsed_ms >= next_probe_ms {
                next_probe_ms = elapsed_ms.saturating_add(CHAIN_PROBE_PERIOD_MS);
                if let Err(error) = self.probe_relay_chain_neighbours().await {
                    tracing::warn!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        error = ?error,
                        "STABILIZATION relay chain neighbour probe failed; probed again next period"
                    );
                }
            }
            let loss = self.dht.listen_relay_chain_loss();
            if self.dht.take_relay_chain_repair_due() {
                if let Err(error) = self.repair_relay_chain().await {
                    tracing::warn!(
                        target: "rings_core::dht::stabilization",
                        local = %self.dht.did,
                        error = ?error,
                        "STABILIZATION relay chain repair after a loss failed; the pass repeats it"
                    );
                }
                continue;
            }
            let poll = Box::pin(try_sleep(STABILIZATION_STOP_POLL_INTERVAL));
            if let Either::Right((false, _)) = select(loss, poll).await {
                return;
            }
        }
    }

    /// The chain exchange: resume the holds whose next member is to be resolved again, ensure
    /// direct links to the current chain neighbours (#1182), and offer one digest set to each
    /// chain neighbour (`Law (Chain repair)`).
    pub(crate) async fn repair_relay_chain(&self) -> Result<()> {
        resume_relay_replications(&self.transport).await?;
        offer_relay_repair(&self.transport).await
    }

    /// Declare to every connected peer not declared to yet that this node holds relay mail, once
    /// its age gate opened (`Law (Age gate)`, #777): those peers may have read its handshake while
    /// it held none. A peer is recorded with the declaration's transaction ID before queueing and
    /// cleared if the transfer fails or is dropped before first-frame admission, so a lost
    /// declaration is sent again on the next pass to that peer alone.
    async fn declare_opened_relay_custody(&self) -> Result<()> {
        let custody = self.dht.relay_custody();
        let connected = self.transport.get_connection_ids();
        let egress = self.transport.protocol_egress();
        for peer in self.dht.undeclared_relay_custody_peers(connected)? {
            let declaration = Message::DeclareRelayCustody(DeclareRelayCustody { custody });
            let payload = egress.originate(declaration, peer, None).await?;
            let tx_id = payload.transaction.tx_id;
            self.dht.record_relay_custody_declaration(peer, tx_id)?;
            if let Err(error) = egress.send_payload(payload).await {
                self.dht.clear_relay_custody_declaration(peer, tx_id)?;
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    %peer,
                    error = ?error,
                    "STABILIZATION relay custody declaration not queued; sent again next pass"
                );
            }
        }
        Ok(())
    }

    /// Send again every hold this node keeps unacknowledged, to the holder its view now names
    /// (`Law (Acknowledged)`, `Law (Convergence)`). A hold whose sender proof has expired, which
    /// no holder admits any longer, is forgotten and logged at `warn`.
    async fn resend_unacknowledged_relay_holds(&self) -> Result<()> {
        let now_ms = self.transport.now_ms();
        for (digest, hold) in self.dht.unacknowledged_relay_holds()? {
            if !hold.hold_is_live_at(now_ms) {
                self.dht.forget_relay_hold(digest)?;
                tracing::warn!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    "STABILIZATION relay hold dropped: sender proof expired before a holder took it"
                );
                continue;
            }
            if let Err(error) = send_relay_hold(&self.transport, hold).await {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    error = ?error,
                    "STABILIZATION relay hold not placed on this pass"
                );
            }
        }
        Ok(())
    }

    /// Ask the owner of every relay key kept here whose owner this view does not place for the
    /// key's holder (`FindRelayHolder`, `Law (Owner places)`); its answer names the holder the
    /// next pass offers the carrier to. A query that is not queued is asked again next pass.
    /// Bound: at most one query per kept key whose owner this view does not place, per pass.
    async fn ask_owners_of_unplaced_relay_keys(&self) -> Result<()> {
        let excluded = self.dht.relay_excluded_holders()?;
        for carrier_key in self
            .dht
            .unplaced_relay_keys(self.transport.now_ms())
            .await?
        {
            let mut query_key = self.dht.relay_holder_query_key(carrier_key)?;
            let mut action = self.dht.find_successor(query_key)?;
            if !matches!(action, PeerRingAction::RemoteAction(_, _)) {
                self.dht.reset_relay_holder_query(carrier_key)?;
                if query_key != carrier_key {
                    query_key = carrier_key;
                    action = self.dht.find_successor(query_key)?;
                }
            }
            let PeerRingAction::RemoteAction(next, _) = action else {
                continue;
            };
            let (query_key, request_id) = self.dht.ask_relay_holder_query(carrier_key)?;
            let query = Message::FindRelayHolder(FindRelayHolder {
                key: query_key,
                request_id,
                excluded: excluded.clone(),
            });
            if let Err(error) = self
                .transport
                .protocol_egress()
                .send_message(query, next)
                .await
            {
                tracing::debug!(
                    target: "rings_core::dht::stabilization",
                    local = %self.dht.did,
                    %carrier_key,
                    %query_key,
                    error = ?error,
                    "STABILIZATION relay holder query not queued; asked again next pass"
                );
            }
        }
        Ok(())
    }

    /// One bounded pass restoring the placement invariant of local storage.
    ///
    /// Invariant: every live local data entry lies in `(self, head]` and at each of its affine
    /// owners, and every relay carrier lies with its holder (`dht::relay_holder`). The pass
    /// offers the data entries beyond the head to the head and each relay carrier this node does
    /// not hold to its holder (ownership hand-offs, whose local cleanup is ack-gated), and
    /// republishes local entries to missing affine owners (additive), then sends the deliveries
    /// through one repair window; last, it passes the previous relay holder it knows to the head.
    /// Placement is a function
    /// of the ring state, so the pass is the same whichever input moved the head; a head change
    /// only requests it. A ready, locally admitted next hop needs no connection-age grace:
    /// the receiver's bounded pre-admission hold queues early frames until its own admission
    /// completes. Repetition is idempotent: deliveries are joins and every local removal is
    /// acknowledged first.
    pub async fn repair_storage(&self) -> Result<StorageRepairOutcome> {
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            redundancy = self.transport.storage_redundancy(),
            "STABILIZATION repair_storage start"
        );
        let handoff = match self.dht.with_topology_state(topology::successor_head)? {
            Some(head) => self.dht.sync_entries_with_successor(head).await?,
            None => PeerRingAction::None,
        };
        let republish = self
            .dht
            .republish_local_entries(self.transport.storage_redundancy())
            .await?;
        let action = PeerRingAction::MultiActions(vec![handoff, republish]);
        let outcome = self.handle_storage_repair_action(action).await?;
        self.maintain_relay_custody().await?;
        tracing::debug!(
            target: "rings_core::dht::stabilization",
            local = %self.dht.did,
            "STABILIZATION repair_storage complete"
        );
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::delegation::DelegateeKey;
    use crate::dht::entry::EntryKind;
    use crate::dht::entry::PlacedEntry;
    use crate::dht::StorageSyncDestination;
    use crate::ecc::SecretKey;
    use crate::storage::MemStorage;
    use crate::swarm::SwarmBuilder;
    use crate::tests::live_entry;

    fn repair_deliveries(values: &[u32]) -> Result<Vec<StorageSyncDelivery>> {
        let actions = values
            .iter()
            .copied()
            .map(|value| {
                let destination = StorageSyncDestination::PlacementKey(Did::from(value));
                let entry = live_entry(Did::from(value + 100), vec![], EntryKind::Data);
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
        let destination = planned.into_message_parts().1.did();
        Ok(destination)
    }

    #[test]
    fn test_changing_delivery_sets_preserve_repair_progress_across_stabilizers() -> Result<()> {
        let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                crate::tests::test_transport("stun://stun.l.google.com:19302"),
                Box::new(MemStorage::new()),
                session,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .build(),
        );

        let selected = [
            selected_destination(&swarm.stabilizer(), &[1, 2, 3])?,
            selected_destination(&swarm.stabilizer(), &[2, 3])?,
            selected_destination(&swarm.stabilizer(), &[1, 2, 3])?,
            selected_destination(&swarm.stabilizer(), &[1, 2, 3])?,
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
        let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                crate::tests::test_transport("stun://stun.l.google.com:19302"),
                Box::new(MemStorage::new()),
                session,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .build(),
        );
        let stabilizer = swarm.stabilizer();

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

    fn handoff_deliveries(values: &[u32]) -> Result<Vec<StorageSyncDelivery>> {
        let actions = values
            .iter()
            .copied()
            .map(|value| {
                let destination = StorageSyncDestination::PhysicalOwner(Did::from(value));
                let entry = live_entry(Did::from(value + 100), vec![], EntryKind::Data);
                PeerRingAction::sync_entries_for_handoff(destination, vec![PlacedEntry::new(
                    destination.did(),
                    entry,
                )])
            })
            .collect();
        PeerRingAction::MultiActions(actions).coalesced_storage_sync_deliveries()
    }

    #[test]
    fn test_repair_cooldown_suppresses_unchanged_topology_expires_and_resets_on_epoch() -> Result<()>
    {
        use crate::swarm::transport::STORAGE_REPAIR_REBROADCAST_COOLDOWN_MS;
        use crate::utils::ManualClock;

        let clock = Arc::new(ManualClock::new(1_000_000));
        let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                crate::tests::test_transport("stun://stun.l.google.com:19302"),
                Box::new(MemStorage::new()),
                session,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .clock(clock.clone())
            .build(),
        );
        let stabilizer = swarm.stabilizer();
        let repair_action = PeerRingAction::MultiActions(vec![
            PeerRingAction::sync_entries_for_repair(
                StorageSyncDestination::PlacementKey(Did::from(1u32)),
                vec![PlacedEntry::new(
                    Did::from(1u32),
                    live_entry(Did::from(101u32), vec![], EntryKind::Data),
                )],
            ),
            PeerRingAction::sync_entries_for_repair(
                StorageSyncDestination::PlacementKey(Did::from(2u32)),
                vec![PlacedEntry::new(
                    Did::from(2u32),
                    live_entry(Did::from(102u32), vec![], EntryKind::Data),
                )],
            ),
        ]);
        let deliveries = repair_action.clone().coalesced_storage_sync_deliveries()?;
        let epoch0 = swarm.dht().topology_epoch().current();
        let now_ms = swarm.transport.now_ms_i64();

        // Recording only delivery[0] cools delivery[0] while leaving delivery[1] eligible.
        swarm
            .transport
            .record_storage_repair_sent(deliveries[0].cursor_key(), epoch0, now_ms)?;
        let filtered = stabilizer.filter_cooled_down_deliveries(
            repair_action.clone().coalesced_storage_sync_deliveries()?,
            epoch0,
            now_ms,
        )?;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].cursor_key(), deliveries[1].cursor_key());

        // Still inside cooldown just before expiry boundary.
        clock.set(
            1_000_000 + u128::try_from(STORAGE_REPAIR_REBROADCAST_COOLDOWN_MS - 1).unwrap_or(0),
        );
        let before_expiry = swarm.transport.now_ms_i64();
        let filtered_before = stabilizer.filter_cooled_down_deliveries(
            repair_action.clone().coalesced_storage_sync_deliveries()?,
            epoch0,
            before_expiry,
        )?;
        assert_eq!(filtered_before.len(), 1);

        // Once cooldown elapses, both deliveries become eligible again.
        clock.set(1_000_000 + u128::try_from(STORAGE_REPAIR_REBROADCAST_COOLDOWN_MS).unwrap_or(0));
        let at_expiry = swarm.transport.now_ms_i64();
        let filtered_at_expiry = stabilizer.filter_cooled_down_deliveries(
            repair_action.clone().coalesced_storage_sync_deliveries()?,
            epoch0,
            at_expiry,
        )?;
        assert_eq!(filtered_at_expiry.len(), 2);

        // Re-record delivery[0], then advance topology_epoch: cooldown clears immediately.
        swarm.transport.record_storage_repair_sent(
            deliveries[0].cursor_key(),
            epoch0,
            at_expiry,
        )?;
        let peer: Did = SecretKey::random().address().into();
        let _ = swarm.dht().admit_connected(peer, None)?;
        let epoch1 = swarm.dht().topology_epoch().current();
        assert!(epoch1 > epoch0);
        let filtered_new_epoch = stabilizer.filter_cooled_down_deliveries(
            repair_action.coalesced_storage_sync_deliveries()?,
            epoch1,
            at_expiry,
        )?;
        assert_eq!(filtered_new_epoch.len(), 2);

        // OwnershipHandoff deliveries are never throttled by cooldown.
        let handoff = handoff_deliveries(&[3, 4])?;
        for item in &handoff {
            swarm
                .transport
                .record_storage_repair_sent(item.cursor_key(), epoch1, at_expiry)?;
        }
        assert_eq!(
            stabilizer
                .filter_cooled_down_deliveries(handoff, epoch1, at_expiry)?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn test_repair_cooldown_map_is_bounded() -> Result<()> {
        let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                crate::tests::test_transport("stun://stun.l.google.com:19302"),
                Box::new(MemStorage::new()),
                session,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .build(),
        );
        let epoch = swarm.dht().topology_epoch().current();
        for i in 1..=600u32 {
            let delivery = PeerRingAction::sync_entries_for_repair(
                StorageSyncDestination::PlacementKey(Did::from(i)),
                vec![PlacedEntry::new(
                    Did::from(i),
                    live_entry(Did::from(i), vec![], EntryKind::Data),
                )],
            )
            .coalesced_storage_sync_deliveries()?
            .pop()
            .ok_or_else(|| crate::error::Error::InvalidMessage("expected delivery".to_string()))?;
            swarm.transport.record_storage_repair_sent(
                delivery.cursor_key(),
                epoch,
                10_000 + i64::from(i),
            )?;
        }
        assert_eq!(swarm.transport.storage_repair_cooldown_len_for_test()?, 512);
        Ok(())
    }

    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    async fn test_deferred_repair_delivery_is_not_recorded_in_cooldown() -> Result<()> {
        let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let swarm = Arc::new(
            SwarmBuilder::new(
                0,
                crate::tests::test_transport("stun://stun.l.google.com:19302"),
                Box::new(MemStorage::new()),
                session,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .build(),
        );
        let stabilizer = swarm.stabilizer();
        let peer: Did = SecretKey::random().address().into();
        let _ = swarm.dht().admit_connected(peer, None)?;
        let delivery = PeerRingAction::sync_entries_for_repair(
            StorageSyncDestination::PhysicalOwner(peer),
            vec![PlacedEntry::new(
                peer,
                live_entry(peer, vec![], EntryKind::Data),
            )],
        );
        // No transport connection exists to `peer`, so delivery is Deferred.
        let outcome = stabilizer.handle_storage_repair_action(delivery).await?;
        assert_eq!(outcome, StorageRepairOutcome::Deferred);
        assert_eq!(swarm.transport.storage_repair_cooldown_len_for_test()?, 0);
        Ok(())
    }
}
