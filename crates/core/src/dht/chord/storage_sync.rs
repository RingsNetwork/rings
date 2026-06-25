use std::str::FromStr;

use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::ChordStorageSync;
use crate::dht::Did;
use crate::error::Result;

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

        if !data.is_empty() {
            Ok(PeerRingAction::RemoteAction(
                new_successor,
                RemoteAction::SyncEntriesWithSuccessor(data), // TODO: This might be too large.
            ))
        } else {
            Ok(PeerRingAction::None)
        }
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
