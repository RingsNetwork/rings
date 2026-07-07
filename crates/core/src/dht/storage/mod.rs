//! DHT storage ownership, repair, and sync transitions.
//!
//! The Chord ring decides physical successor topology. This module decides
//! storage-specific ownership on top of that topology: affine replica
//! placement, storage virtual-node ownership, read repair, and sync hand-off.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::chord::PeerRing;
use super::chord::PeerRingAction;
use super::chord::RemoteAction;
use super::entry::PlacedEntry;
use super::types::Chord;
use super::virtual_node::StorageVirtualNodes;
use super::virtual_node::VirtualNode;
use super::Did;
use crate::error::Error;
use crate::error::Result;

mod repair;
mod sync;

/// Storage-sync transition kind.
///
/// Cleanup law: only [`StorageSyncPurpose::OwnershipHandoff`] reports can prove
/// source-side deletion. [`StorageSyncPurpose::AdditiveRepair`] is copy-only and
/// must never create a delete-capable ack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum StorageSyncPurpose {
    /// Ownership changed and the sender may delete after a durable matching ack.
    OwnershipHandoff,
    /// Additive read-repair or anti-entropy copy.
    AdditiveRepair,
}

impl StorageSyncPurpose {
    /// Returns whether reports for this sync kind may drive source cleanup.
    pub const fn permits_source_cleanup(self) -> bool {
        matches!(self, Self::OwnershipHandoff)
    }
}

/// Destination semantics for a storage sync hand-off.
///
/// Routing law:
/// - [`StorageSyncDestination::PhysicalOwner`] is routed as a node DID through
///   physical Chord membership.
/// - [`StorageSyncDestination::PlacementKey`] is routed through storage
///   ownership for that placement key.
///
/// Safety: a physical-owner receiver still validates each placement before
/// acking, so a stale sender cannot trigger local cleanup for a key the receiver
/// does not own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum StorageSyncDestination {
    /// Route to a physical node DID, then let the receiver validate entry ownership.
    PhysicalOwner(Did),
    /// Route through storage ownership for this placement key.
    PlacementKey(Did),
}

impl StorageSyncDestination {
    /// Build a physical-owner sync destination.
    pub const fn physical_owner(did: Did) -> Self {
        Self::PhysicalOwner(did)
    }

    /// Build a placement-key sync destination.
    pub const fn placement_key(did: Did) -> Self {
        Self::PlacementKey(did)
    }

    /// Return the DID placed in the relay destination.
    pub fn did(self) -> Did {
        match self {
            Self::PhysicalOwner(did) | Self::PlacementKey(did) => did,
        }
    }

    /// Return the routing semantics for this destination.
    pub const fn route(self) -> StorageSyncRoute {
        match self {
            Self::PhysicalOwner(_) => StorageSyncRoute::PhysicalOwner,
            Self::PlacementKey(_) => StorageSyncRoute::PlacementKey,
        }
    }
}

/// Routing semantics for a storage sync hand-off.
///
/// The route is paired with the outer [`PeerRingAction::RemoteAction`] target,
/// so the action tree carries the destination DID exactly once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum StorageSyncRoute {
    /// Interpret the action target as a physical node DID.
    PhysicalOwner,
    /// Interpret the action target as a storage placement key.
    PlacementKey,
}

impl StorageSyncRoute {
    /// Combine this route with the action target DID to form a wire destination.
    pub const fn destination(self, target: Did) -> StorageSyncDestination {
        match self {
            Self::PhysicalOwner => StorageSyncDestination::physical_owner(target),
            Self::PlacementKey => StorageSyncDestination::placement_key(target),
        }
    }
}

/// Lowered storage-sync delivery ready for the message layer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StorageSyncDelivery {
    purpose: StorageSyncPurpose,
    destination: StorageSyncDestination,
    data: Vec<PlacedEntry>,
}

impl StorageSyncDelivery {
    fn from_route(
        purpose: StorageSyncPurpose,
        target: Did,
        route: StorageSyncRoute,
        data: Vec<PlacedEntry>,
    ) -> Self {
        // Invariant: `destination` is the unique wire interpretation of
        // `(target, route)`. Transport computes the physical next hop from the
        // destination at send time, so this lowered value does not pretend that
        // the action target is already a relay hop.
        Self {
            purpose,
            destination: route.destination(target),
            data,
        }
    }

    /// Consume this delivery into the wire purpose, destination, and payload data.
    pub(crate) fn into_message_parts(
        self,
    ) -> (StorageSyncPurpose, StorageSyncDestination, Vec<PlacedEntry>) {
        (self.purpose, self.destination, self.data)
    }
}

pub(super) enum StorageSyncTarget {
    Local,
    Remote(StorageSyncDestination),
}

impl PeerRingAction {
    pub(crate) fn sync_entries_for_handoff(
        destination: StorageSyncDestination,
        data: Vec<PlacedEntry>,
    ) -> Self {
        Self::sync_entries(StorageSyncPurpose::OwnershipHandoff, destination, data)
    }

    pub(crate) fn sync_entries_for_repair(
        destination: StorageSyncDestination,
        data: Vec<PlacedEntry>,
    ) -> Self {
        Self::sync_entries(StorageSyncPurpose::AdditiveRepair, destination, data)
    }

    fn sync_entries(
        purpose: StorageSyncPurpose,
        destination: StorageSyncDestination,
        data: Vec<PlacedEntry>,
    ) -> Self {
        Self::RemoteAction(destination.did(), RemoteAction::SyncEntriesWithSuccessor {
            purpose,
            route: destination.route(),
            data,
        })
    }

    /// Lower this action tree into storage-sync deliveries.
    pub(crate) fn storage_sync_deliveries(self) -> Result<Vec<StorageSyncDelivery>> {
        let mut deliveries = Vec::new();
        self.collect_storage_sync_deliveries(&mut deliveries)?;
        Ok(deliveries)
    }

    fn collect_storage_sync_deliveries(
        self,
        deliveries: &mut Vec<StorageSyncDelivery>,
    ) -> Result<()> {
        match self {
            Self::None => Ok(()),
            Self::RemoteAction(
                target,
                RemoteAction::SyncEntriesWithSuccessor {
                    purpose,
                    route,
                    data,
                },
            ) => {
                deliveries.push(StorageSyncDelivery::from_route(
                    purpose, target, route, data,
                ));
                Ok(())
            }
            Self::MultiActions(actions) => {
                for action in actions {
                    action.collect_storage_sync_deliveries(deliveries)?;
                }
                Ok(())
            }
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }
}

impl PeerRing {
    /// Return whether the storage virtual-node registry is enabled.
    pub fn storage_virtual_nodes_enabled(&self) -> Result<bool> {
        Ok(self.storage_virtual_node_config().is_enabled())
    }

    /// Return virtual storage positions owned by `owner`.
    pub fn storage_virtual_positions(&self, owner: Did) -> Result<Vec<VirtualNode>> {
        Ok(self.storage_virtual_nodes()?.positions_for_owner(owner))
    }

    pub(super) fn observed_storage_virtual_owner(&self, placement_key: Did) -> Result<Option<Did>> {
        Ok(self.storage_virtual_nodes()?.owner_for_key(placement_key))
    }

    pub(super) fn observed_storage_virtual_owner_registered(&self, owner: Did) -> Result<bool> {
        Ok(self.storage_virtual_nodes()?.contains_owner(owner))
    }

    fn storage_virtual_nodes(&self) -> Result<StorageVirtualNodes> {
        let state = self.topology_state()?;
        let mut owners = BTreeSet::new();
        // Pre: `state` is this node's authenticated topology view.
        // Post: the virtual-owner set is exactly the physical DIDs currently
        // visible to storage routing: local, successors, predecessor, and
        // fingers. It is an observed view, not a global registry.
        owners.insert(state.local);
        owners.extend(state.successors);
        owners.extend(state.predecessor);
        owners.extend(state.fingers.into_iter().flatten());
        Ok(StorageVirtualNodes::from_owners(
            self.storage_virtual_node_config(),
            owners,
        ))
    }

    pub(crate) fn find_storage_owner(&self, placement_key: Did) -> Result<PeerRingAction> {
        if let Some(owner) = self.observed_storage_virtual_owner(placement_key)? {
            if owner == self.did {
                Ok(PeerRingAction::Some(owner))
            } else {
                Ok(PeerRingAction::RemoteAction(
                    owner,
                    RemoteAction::FindSuccessor(placement_key),
                ))
            }
        } else {
            self.find_successor(placement_key)
        }
    }

    pub(super) fn storage_sync_target(&self, placement_key: Did) -> Result<StorageSyncTarget> {
        if let Some(owner) = self.observed_storage_virtual_owner(placement_key)? {
            if owner == self.did {
                Ok(StorageSyncTarget::Local)
            } else {
                Ok(StorageSyncTarget::Remote(
                    StorageSyncDestination::PhysicalOwner(owner),
                ))
            }
        } else {
            match self.find_successor(placement_key)? {
                // In non-virtual storage, `Some(_)` means this node's local
                // Chord view has reached the terminal storage branch. The
                // witness DID may be the successor for lookup fallback, not a
                // remote owner that should receive this placement.
                PeerRingAction::Some(_) => Ok(StorageSyncTarget::Local),
                PeerRingAction::RemoteAction(_, RemoteAction::FindSuccessor(_)) => Ok(
                    StorageSyncTarget::Remote(StorageSyncDestination::PlacementKey(placement_key)),
                ),
                action => Err(Error::unexpected_peer_ring_action(action)),
            }
        }
    }

    pub(crate) fn next_hop_for_storage_sync(
        &self,
        destination: StorageSyncDestination,
    ) -> Result<Option<Did>> {
        // Pre: destination.did() is the relay destination signed in the payload.
        // Post: PhysicalOwner routes by physical membership; PlacementKey routes
        // by storage ownership. The two relations are intentionally distinct.
        match destination {
            StorageSyncDestination::PhysicalOwner(owner) => self.next_hop_to_physical_owner(owner),
            StorageSyncDestination::PlacementKey(key) => self.next_hop_to_storage_placement(key),
        }
    }

    fn next_hop_to_physical_owner(&self, owner: Did) -> Result<Option<Did>> {
        if owner == self.did {
            return Ok(None);
        }

        match self.find_successor(owner)? {
            // If this local view cannot prove a better physical next hop, try
            // the target owner directly rather than accepting the payload as
            // local work. Persisting happens only at relay destination.
            PeerRingAction::Some(next) if next == self.did => Ok(Some(owner)),
            PeerRingAction::Some(next) => Ok(Some(next)),
            PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessor(_)) => Ok(Some(next)),
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }

    fn next_hop_to_storage_placement(&self, key: Did) -> Result<Option<Did>> {
        match self.find_storage_owner(key)? {
            PeerRingAction::Some(_) => Ok(None),
            PeerRingAction::RemoteAction(next, RemoteAction::FindSuccessor(_)) => Ok(Some(next)),
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }
}

#[cfg(all(not(feature = "wasm"), test))]
mod tests;
