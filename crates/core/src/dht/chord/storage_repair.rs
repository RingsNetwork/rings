use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::successor::SuccessorReader;
use crate::dht::Chord;
use crate::dht::ChordStorageRepair;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

fn merge_actions(actions: Vec<PeerRingAction>) -> PeerRingAction {
    if actions.is_empty() {
        PeerRingAction::None
    } else {
        PeerRingAction::MultiActions(actions)
    }
}

fn push_action(actions: &mut Vec<PeerRingAction>, action: PeerRingAction) {
    match action {
        PeerRingAction::None => {}
        PeerRingAction::MultiActions(inner) => {
            for action in inner {
                push_action(actions, action);
            }
        }
        action => actions.push(action),
    }
}

impl PeerRing {
    /// Returns whether a departed peer was near enough to local storage
    /// responsibility that local entries should be republished after removing it.
    pub(crate) async fn peer_may_share_storage_responsibility(
        &self,
        peer: Did,
        redundancy: u16,
    ) -> Result<bool> {
        if self
            .lock_predecessor()?
            .is_some_and(|predecessor| predecessor == peer)
        {
            return Ok(true);
        }
        if self.successors().contains(&peer)? {
            return Ok(true);
        }
        if self.lock_finger()?.contains(Some(peer)) {
            return Ok(true);
        }

        if redundancy <= 1 {
            return Ok(false);
        }

        for (_, entry) in self.storage.get_all().await? {
            for placement_key in entry.did.rotate_affine(redundancy)? {
                match self.find_successor(placement_key)? {
                    PeerRingAction::Some(owner) if owner == peer => return Ok(true),
                    PeerRingAction::RemoteAction(next, _) if next == peer => return Ok(true),
                    _ => {}
                }
            }
        }
        Ok(false)
    }

    async fn copy_entry_to_placement(
        &self,
        placement_key: Did,
        entry: &Entry,
    ) -> Result<PeerRingAction> {
        let placed = PlacedEntry::new(placement_key, entry.clone());
        match self.find_successor(placement_key)? {
            PeerRingAction::Some(owner) if owner == self.did => {
                self.storage
                    .put(
                        &placement_key.to_string(),
                        &entry.clone().into_storage_entry(),
                    )
                    .await?;
                Ok(PeerRingAction::None)
            }
            PeerRingAction::Some(_)
            | PeerRingAction::RemoteAction(_, RemoteAction::FindSuccessor(_)) => {
                Ok(PeerRingAction::RemoteAction(
                    placement_key,
                    RemoteAction::SyncEntriesWithSuccessor(vec![placed]),
                ))
            }
            action => Err(Error::PeerRingUnexpectedAction(action)),
        }
    }

    async fn republish_entry(&self, entry: Entry, redundancy: u16) -> Result<PeerRingAction> {
        if redundancy <= 1 {
            return Ok(PeerRingAction::None);
        }

        let entry = entry.into_storage_entry();
        let mut actions = Vec::new();
        for placement_key in entry.did.rotate_affine(redundancy)? {
            let action = self.copy_entry_to_placement(placement_key, &entry).await?;
            push_action(&mut actions, action);
        }
        Ok(merge_actions(actions))
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorageRepair<PeerRingAction> for PeerRing {
    async fn republish_local_entries(&self, redundancy: u16) -> Result<PeerRingAction> {
        if redundancy <= 1 {
            return Ok(PeerRingAction::None);
        }

        // State variables:
        // - local: entries durably stored on this node before this transition.
        // - owners(entry): affine placement keys under the current routing view.
        //
        // Preservation: forall key in local, key remains in local after this
        // transition. Repair emits only copy actions; deletion remains isolated
        // to acknowledge_synced_entries after #611 ack.
        let mut actions = Vec::new();
        for (_, entry) in self.storage.get_all().await? {
            let action = self.republish_entry(entry, redundancy).await?;
            push_action(&mut actions, action);
        }
        Ok(merge_actions(actions))
    }

    async fn read_repair_entry(&self, entry: Entry, redundancy: u16) -> Result<PeerRingAction> {
        self.republish_entry(entry, redundancy).await
    }
}
