use std::str::FromStr;

use async_trait::async_trait;
use serde::Serialize;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::consts::MAX_CHUNK_ENVELOPE_OVERHEAD;
use crate::consts::TRANSPORT_CUSTOM_OVERHEAD;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SyncEntriesWithSuccessor;

/// Maximum wire budget for one `SyncEntriesWithSuccessor` hand-off batch.
///
/// This is deliberately far below `TRANSPORT_MAX_SIZE` so stabilization does
/// not create a single all-or-nothing serialized message. The batch cost also
/// reserves the payload/chunk envelope bytes below.
pub(crate) const SYNC_BATCH_MAX_BYTES: usize = TRANSPORT_MAX_SIZE / 32;

const SYNC_BATCH_ENVELOPE_HEADROOM_BYTES: usize =
    MAX_CHUNK_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD;

fn serialized_wire_size<T: Serialize + ?Sized>(value: &T) -> Result<usize> {
    let bytes = bincode::serialized_size(value).map_err(Error::BincodeSerialize)?;
    usize::try_from(bytes).map_err(|_| Error::MessageTooLarge(usize::MAX))
}

fn add_wire_cost(total: usize, next: usize) -> Result<usize> {
    total
        .checked_add(next)
        .ok_or(Error::MessageTooLarge(usize::MAX))
}

fn sync_entries_fixed_wire_cost() -> Result<usize> {
    let empty_message =
        Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor { data: Vec::new() });
    add_wire_cost(
        serialized_wire_size(&empty_message)?,
        SYNC_BATCH_ENVELOPE_HEADROOM_BYTES,
    )
}

fn placed_entry_wire_cost(placed: &PlacedEntry) -> Result<usize> {
    serialized_wire_size(placed)
}

#[cfg(all(test, not(feature = "wasm")))]
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorageSync<PeerRingAction> for PeerRing {
    /// When the successor of a node is updated, it needs to check if there are
    /// `Entry`s that are no longer between current node and `new_successor`,
    /// and copy them to the new successor.
    async fn sync_entries_with_successor(&self, new_successor: Did) -> Result<PeerRingAction> {
        let mut data = Vec::<PlacedEntry>::new();
        let all_items: Vec<(String, Entry)> = self.storage.get_all().await?;

        // Pre: new_successor is the successor adopted by stabilization.
        // Post S1: forall key in local_before, local_after[key] =
        // local_before[key]; this transition emits copies only.
        // Post S2(copy): every emitted PlacedEntry keeps the exact local
        // placement key, so an eventual ack names the key whose durable copy was
        // reported by the receiver.
        // Preservation: sync is copy-before-ack-before-delete.
        // acknowledge_synced_entries is the only delete transition.
        for (entry_key_str, entry) in all_items.iter() {
            let entry_key = Did::from_str(entry_key_str)?;
            if self.bias(entry_key) > self.bias(new_successor) {
                data.push(PlacedEntry::new(entry_key, entry.clone()));
            }
        }

        let batches = sync_entries_batches(data, SYNC_BATCH_MAX_BYTES)?;
        Ok(batches
            .into_iter()
            .map(|batch| {
                PeerRingAction::RemoteAction(
                    new_successor,
                    RemoteAction::SyncEntriesWithSuccessor(batch),
                )
            })
            .collect::<Vec<_>>()
            .into())
    }

    async fn acknowledge_synced_entries(&self, acks: &[SyncedEntryAck]) -> Result<PeerRingAction> {
        // Pre S2': each ack in acks is contained in a
        // SyncEntriesWithSuccessorReport sent only after the receiver persisted
        // SyncedEntryAck { key, entry } at key.
        // Post S2': a local key is removed only if
        // local_before[key] == ack.entry. If local_before[key] differs, the
        // local value is preserved and will be offered again by a later
        // sync_entries_with_successor transition.
        // Preservation: a write racing between copy and ack changes
        // local_before[key], so confirms_local_value is false and delete is
        // skipped.
        for ack in acks {
            let Some(local_entry) = self.storage.get(&ack.key.to_string()).await? else {
                continue;
            };
            if ack.confirms_local_value(&local_entry) {
                self.storage.remove(&ack.key.to_string()).await?;
            }
        }

        Ok(PeerRingAction::None)
    }
}
