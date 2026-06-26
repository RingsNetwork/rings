//! Formal storage-replication model.
//!
//! State variables:
//! - `R = Z / 2^160`, represented by [`Did`].
//! - `place(id(e), N) = [k_0, ..., k_{N-1}]`, computed by
//!   [`Did::rotate_affine`].
//! - `sigma_n[k]` is the [`Entry`] stored by node `n` under placement key `k`.
//! - `succ(k)` is the owner returned by Chord routing for placement key `k`.
//!
//! Invariant REPLICATED(e, N):
//! `forall k in place(id(e), N), sigma_{succ(k)}[k] >= e_delta`, where `>=`
//! is the partial order induced by [`crate::algebra::JoinSemilattice`].
//!
//! Liveness S4:
//! In a quiescent window, if at least one placement copy of `e` remains at the
//! start of an anti-entropy period, one `republish_local_entries` round
//! delivers the entry's join state to every current owner in `place(id(e), N)`.
//!
//! Safety:
//! - S1 Additivity (#612): repair transitions in this module never call
//!   `storage.remove`; they only deliver additional joins.
//! - S2' No-update-loss (#611/#614 cleanup): the only deletion transition is
//!   `acknowledge_synced_entries`; the finite model
//!   `storage_sync_model_preserves_no_update_loss` in `dht_stateright` checks
//!   that ack-delete removes a local value only when the receiver state contains
//!   the same storage-canonical joined value.
//! - S3 Idempotence: duplicate repair delivery is observationally equivalent to
//!   one delivery because [`Entry::join`](crate::dht::entry::Entry::join) is
//!   idempotent.
//!
//! Read-repair:
//! Given lookup observation `o : place(id(e), N) -> {Hit(e), Miss, Unknown}`,
//! `repair_targets(o) = { k | o(k) = Miss }`. `read_repair_entry` copies only
//! `repair_targets(o)` and does not evaluate `place` or `succ`. Transport keeps
//! observations bounded by lookup round, TTL, and capacity, so a miss owner is
//! a fresh lookup witness rather than persistent routing state.

use async_trait::async_trait;

use super::PeerRing;
use super::PeerRingAction;
use super::RemoteAction;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::PlacementMiss;
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
        // Pre: peer is a terminal or departing DID under the caller's routing
        // view.
        // Post: true iff peer is observed in a routing position that can affect
        // storage responsibility: predecessor, successor list, finger table, or
        // owner of some locally held affine placement key.
        // Preservation S1: this predicate performs no storage writes/removes.
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

        // Departure repair is only an accelerator; periodic anti-entropy is
        // the authoritative backstop. This scan is O(entries * redundancy) and
        // may race with another terminal-state trigger, but repair only
        // delivers joins, so duplicate triggers preserve storage state.
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
        // Pre: placement_key belongs to place(id(entry), redundancy) for the
        // caller's anti-entropy or republish transition.
        // Post S1: no local key is removed.
        // Post S3: if self owns placement_key, sigma_self[placement_key] is
        // joined with entry after the transition; repeating the write preserves
        // sigma by join idempotence.
        // Post: if another node owns placement_key, the returned action carries
        // PlacedEntry { key: placement_key, entry } so placement identity is not
        // recomputed by the receiver.
        let placed = PlacedEntry::new(placement_key, entry.clone());
        match self.find_successor(placement_key)? {
            PeerRingAction::Some(owner) if owner == self.did => {
                self.join_storage_entry(placement_key, entry.clone())
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
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }

    async fn copy_entry_to_observed_miss(
        &self,
        miss: PlacementMiss,
        entry: &Entry,
    ) -> Result<PeerRingAction> {
        // Pre: miss was produced by entry_lookup/SearchEntry and is still
        // fresh under the transport observation TTL, so miss.owner was the
        // responsible owner for miss.key under the lookup's routing view.
        // Post R1/R2: exactly miss.key is repaired; Hit and Unknown placements
        // are not touched by this transition.
        // Post R4: this function performs no place()/succ() computation. It
        // reuses the owner observed by lookup and emits only a copy action.
        let placed = PlacedEntry::new(miss.key, entry.clone());
        if miss.owner == self.did {
            self.join_storage_entry(miss.key, entry.clone()).await?;
            Ok(PeerRingAction::None)
        } else {
            Ok(PeerRingAction::RemoteAction(
                miss.owner,
                RemoteAction::SyncEntriesWithSuccessor(vec![placed]),
            ))
        }
    }

    async fn republish_entry(&self, entry: Entry, redundancy: u16) -> Result<PeerRingAction> {
        if redundancy <= 1 {
            return Ok(PeerRingAction::None);
        }

        let entry = entry.try_into_storage_entry()?;
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

        // Pre: redundancy > 1 and every local storage value is an Entry.
        // Post S1: forall key in local_before, local_after[key] =
        // local_before[key]. This transition only emits join deliveries.
        // Post S3: repeating this transition produces the same sigma mapping as
        // one application because storage writes are Entry::join deliveries.
        // Post S4: for every local entry e, a copy action exists for each key
        // in place(id(e), redundancy) whose owner is not self; self-owned
        // placements are written locally.
        let mut actions = Vec::new();
        for (_, entry) in self.storage.get_all().await? {
            let action = self.republish_entry(entry, redundancy).await?;
            push_action(&mut actions, action);
        }
        Ok(merge_actions(actions))
    }

    async fn read_repair_entry(
        &self,
        entry: Entry,
        misses: &[PlacementMiss],
    ) -> Result<PeerRingAction> {
        // Pre: misses = repair_targets(o) for the lookup observation that found
        // entry, and each miss.owner was observed while querying miss.key.
        // Post R1: emitted copy actions are in one-to-one correspondence with
        // misses whose owner is remote; self-owned misses are written locally.
        // Post R2/R3: Hit and Unknown placements are absent from misses, so no
        // action can target them. A local-hit short circuit has misses = [].
        // Post R4: no placement vector or successor is recomputed here.
        // Preservation S1/S3: this transition never removes and duplicate copy
        // actions are duplicate Entry::join deliveries.
        let mut actions = Vec::new();
        for miss in misses.iter().copied() {
            let action = self.copy_entry_to_observed_miss(miss, &entry).await?;
            push_action(&mut actions, action);
        }
        Ok(merge_actions(actions))
    }
}
