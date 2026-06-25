use std::str::FromStr;

use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
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

        // Preservation: sync is copy-before-ack-before-delete. This step only
        // copies entries; acknowledge_synced_entries removes acked keys later.
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

    async fn acknowledge_synced_entries(&self, keys: &[Did]) -> Result<PeerRingAction> {
        // Invariant gap: ack currently proves "successor durably stored this
        // placement key", not "the local value is unchanged since copy".
        // A write racing between copy and ack could be newer than the acked
        // value and still be removed here. Closing that requires Entry
        // version/timestamp metadata so delete can be conditional on equality.
        // The storage durability work in #612 keeps repair additive until that
        // versioned delete proof exists.
        for key in keys {
            self.storage.remove(&key.to_string()).await?;
        }

        Ok(PeerRingAction::None)
    }
}
