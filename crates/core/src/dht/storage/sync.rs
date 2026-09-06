use async_trait::async_trait;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use serde::Serialize;

use super::StorageSyncDestination;
use super::StorageSyncPurpose;
use super::StorageSyncTarget;
use crate::consts::MAX_CHUNK_ENVELOPE_OVERHEAD;
use crate::consts::TRANSPORT_CUSTOM_OVERHEAD;
use crate::dht::chord::PeerRing;
use crate::dht::chord::PeerRingAction;
use crate::dht::did::BiasId;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::utils::get_epoch_ms;

/// Maximum wire budget for one `SyncEntriesWithSuccessor` hand-off batch.
///
/// This stays below one interoperable WebRTC data-channel frame so storage
/// anti-entropy cannot monopolize the chunk sender. The batch cost also
/// reserves the payload/chunk envelope bytes below.
pub(crate) const SYNC_BATCH_MAX_BYTES: usize = MAX_DATA_CHANNEL_MESSAGE_SIZE / 4;

const SYNC_BATCH_ENVELOPE_HEADROOM_BYTES: usize =
    MAX_CHUNK_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD;

fn serialized_wire_size<T: Serialize>(value: &T) -> Result<usize> {
    let bytes = rings_codec::serialized_size(value).map_err(Error::CodecSerialize)?;
    usize::try_from(bytes).map_err(|_| Error::MessageSizeOverflow)
}

fn add_wire_cost(total: usize, next: usize) -> Result<usize> {
    total.checked_add(next).ok_or(Error::MessageSizeOverflow)
}

fn sync_entries_fixed_wire_cost() -> Result<usize> {
    let empty_message = Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(Did::from(0u32)),
        data: Vec::new(),
    });
    add_wire_cost(
        serialized_wire_size(&empty_message)?,
        SYNC_BATCH_ENVELOPE_HEADROOM_BYTES,
    )
}

fn placed_entry_wire_cost(placed: &PlacedEntry) -> Result<usize> {
    serialized_wire_size(placed)
}

#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
pub(super) fn sync_entries_batch_wire_cost(data: &[PlacedEntry]) -> Result<usize> {
    let mut cost = sync_entries_fixed_wire_cost()?;
    for placed in data {
        cost = add_wire_cost(cost, placed_entry_wire_cost(placed)?)?;
    }
    Ok(cost)
}

pub(super) fn sync_entries_batches(
    data: Vec<PlacedEntry>,
    max_batch_bytes: usize,
) -> Result<Vec<Vec<PlacedEntry>>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let fixed_cost = sync_entries_fixed_wire_cost()?;
    let mut current_cost = fixed_cost;

    // Pre: `data` is the migrating set M produced from local storage.
    // Post Coverage: concatenating all returned batches yields exactly M in
    // the same order; no PlacedEntry is duplicated or dropped.
    // Post Budget: every non-singleton batch, and every singleton whose own
    // cost fits, has sync_entries_batch_wire_cost(batch) <= max_batch_bytes.
    // Post Atomicity: each PlacedEntry is moved as a whole; no entry is split
    // across batches.
    // Post Progress: if one PlacedEntry exceeds max_batch_bytes by itself, it
    // is emitted as a one-entry batch so the chunk layer can still frame it.
    for placed in data {
        let placed_cost = placed_entry_wire_cost(&placed)?;
        let candidate_cost = add_wire_cost(current_cost, placed_cost)?;
        if current.is_empty() {
            current.push(placed);
            current_cost = candidate_cost;
            continue;
        }

        if candidate_cost <= max_batch_bytes {
            current.push(placed);
            current_cost = candidate_cost;
        } else {
            batches.push(current);
            current = vec![placed];
            current_cost = add_wire_cost(fixed_cost, placed_cost)?;
        }
    }

    if !current.is_empty() {
        batches.push(current);
    }

    Ok(batches)
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl ChordStorageSync<PeerRingAction> for PeerRing {
    /// When the successor of a node is updated, it needs to check if there are
    /// `Entry`s that are no longer between current node and `new_successor`,
    /// and copy them to the new successor.
    async fn sync_entries_with_successor(&self, new_successor: Did) -> Result<PeerRingAction> {
        let all_items = self.live_storage_entries(get_epoch_ms()).await?;
        // Relay inboxes are placed by the ring geometry in every storage mode
        // (see the `inbox` module); data topics follow the configured mode.
        let (relay, data): (Vec<_>, Vec<_>) = all_items
            .into_iter()
            .partition(|(_, entry)| entry.kind.is_relay_inbox());
        let mut actions = vec![self.hand_off_beyond_successor(new_successor, relay)?];
        actions.push(if self.storage_virtual_nodes_enabled()? {
            self.copy_entries_to_observed_virtual_storage_owners(data)?
        } else {
            self.hand_off_beyond_successor(new_successor, data)?
        });
        Ok(actions.into())
    }

    async fn acknowledge_synced_entries(&self, acks: &[SyncedEntryAck]) -> Result<PeerRingAction> {
        // Pre S2': each ack in acks is contained in a
        // SyncEntriesWithSuccessorReport sent only after the receiver persisted
        // SyncedEntryAck { key, entry } at key.
        // Post S2': a local key is removed only if canonical(local_before[key])
        // == canonical(ack.entry). If the canonical local value differs, the
        // local value is preserved and will be offered again by a later
        // sync_entries_with_successor transition.
        // Preservation #614: a write racing between copy and ack changes the
        // canonical local value, so confirms_local_value is false and delete
        // is skipped.
        let now_ms = get_epoch_ms();
        for ack in acks {
            let key = StorageKey::new(ack.entry.kind, ack.key);
            self.remove_storage_entry_confirmed_by(key, now_ms, ack)
                .await?;
        }

        Ok(PeerRingAction::None)
    }
}

impl PeerRing {
    /// Offer every item placed beyond `(self, new_successor]` to `new_successor` as an
    /// ownership hand-off.
    ///
    /// Pre: new_successor is the current successor head, whichever input
    /// moved it. The storage repair pass runs this, so a delivery deferred
    /// or lost before the head admitted this node is offered again.
    /// Post S1: forall key in local_before, local_after[key] =
    /// local_before[key]; this transition emits join deliveries only.
    /// Post S2(copy): every emitted PlacedEntry keeps the exact local
    /// placement key, so an eventual ack names the key whose durable copy was
    /// reported by the receiver.
    /// Preservation #611/#614: sync hand-off is join-before-ack-before-local
    /// cleanup. acknowledge_synced_entries is the only value-dependent local
    /// cleanup transition and does not define storage convergence; retention
    /// expiry retires values independently of their content.
    fn hand_off_beyond_successor(
        &self,
        new_successor: Did,
        items: Vec<(StorageKey, Entry)>,
    ) -> Result<PeerRingAction> {
        let mut data = Vec::<PlacedEntry>::new();
        for (key, entry) in items {
            if self.placed_beyond(key.placement(), new_successor) {
                data.push(PlacedEntry::new(key.placement(), entry));
            }
        }

        let batches = sync_entries_batches(data, SYNC_BATCH_MAX_BYTES)?;
        Ok(batches
            .into_iter()
            .map(|batch| {
                PeerRingAction::sync_entries_for_handoff(
                    StorageSyncDestination::PhysicalOwner(new_successor),
                    batch,
                )
            })
            .collect::<Vec<_>>()
            .into())
    }

    /// `key ∉ (self, head]`: the placement lies beyond this node's interval up to `head`, so
    /// `head` (or a node past it) owns it now.
    fn placed_beyond(&self, key: Did, head: Did) -> bool {
        BiasId::cmp_from_observer(self.did, key, head) == std::cmp::Ordering::Greater
    }

    fn copy_entries_to_observed_virtual_storage_owners(
        &self,
        all_items: Vec<(StorageKey, Entry)>,
    ) -> Result<PeerRingAction> {
        let mut by_target =
            std::collections::BTreeMap::<StorageSyncDestination, Vec<PlacedEntry>>::new();

        // Pre: storage virtual nodes are enabled.
        // Model: storage_sync_target computes ownership under this node's
        // authenticated local topology view, not a globally complete membership
        // relation.
        // Post S1: local entries are retained without action; entries whose
        // observed virtual owner is remote are emitted as additive anti-entropy
        // copies to that physical owner.
        // Preservation S1'': this transition cannot create a delete-capable
        // report. Only non-virtual physical handoff has an ownership proof
        // strong enough to permit source cleanup.
        for (key, entry) in all_items {
            if let StorageSyncTarget::Remote(target) = self.storage_sync_target(key.placement())? {
                by_target
                    .entry(target)
                    .or_default()
                    .push(PlacedEntry::new(key.placement(), entry));
            }
        }

        let mut actions = Vec::new();
        for (target, data) in by_target {
            for batch in sync_entries_batches(data, SYNC_BATCH_MAX_BYTES)? {
                actions.push(PeerRingAction::sync_entries_for_repair(target, batch));
            }
        }
        Ok(actions.into())
    }
}
