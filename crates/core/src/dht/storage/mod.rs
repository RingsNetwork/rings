//! DHT storage ownership, repair, and sync transitions.
//!
//! The Chord ring decides physical successor topology. This module decides
//! storage-specific ownership on top of that topology: affine replica
//! placement, storage virtual-node ownership, read repair, and sync hand-off.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::chord::PeerRing;
use super::chord::PeerRingAction;
use super::chord::RemoteAction;
use super::entry::PlacedEntry;
use super::topology;
use super::topology::FindSuccessorStep;
use super::topology::TopologyState;
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

/// Stable identity used to continue bounded storage repair across changing plans.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StorageSyncDeliveryCursor {
    purpose: StorageSyncPurpose,
    destination: StorageSyncDestination,
    placement_keys: Vec<Did>,
}

impl StorageSyncDelivery {
    fn from_parts(
        purpose: StorageSyncPurpose,
        destination: StorageSyncDestination,
        data: Vec<PlacedEntry>,
    ) -> Self {
        Self {
            purpose,
            destination,
            data,
        }
    }

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

    /// Return the stable repair cursor key for this delivery.
    ///
    /// Entry values are intentionally excluded. Replacing a value at the same
    /// placement preserves the delivery's scheduling identity while changes to
    /// batch membership produce a distinct key.
    pub(crate) fn cursor_key(&self) -> StorageSyncDeliveryCursor {
        let mut placement_keys = self
            .data
            .iter()
            .map(|placed| placed.key)
            .collect::<Vec<_>>();
        placement_keys.sort_unstable();
        StorageSyncDeliveryCursor {
            purpose: self.purpose,
            destination: self.destination,
            placement_keys,
        }
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

    /// Lower this action tree into storage-sync deliveries, merging delivery
    /// leaves that share the same wire purpose and destination.
    ///
    /// Safety law: coalescing is restricted to identical `(purpose,
    /// destination)` pairs. `PlacementKey` destinations therefore keep their
    /// placement identity, and physical-owner batches still let the receiver
    /// validate each placement independently before acking.
    pub(crate) fn coalesced_storage_sync_deliveries(self) -> Result<Vec<StorageSyncDelivery>> {
        let mut by_route =
            BTreeMap::<(StorageSyncPurpose, StorageSyncDestination), Vec<PlacedEntry>>::new();
        for delivery in self.storage_sync_deliveries()? {
            let (purpose, destination, data) = delivery.into_message_parts();
            by_route
                .entry((purpose, destination))
                .or_default()
                .extend(data);
        }

        let mut deliveries = Vec::new();
        for ((purpose, destination), data) in by_route {
            for batch in sync::sync_entries_batches(data, sync::SYNC_BATCH_MAX_BYTES)? {
                deliveries.push(StorageSyncDelivery::from_parts(purpose, destination, batch));
            }
        }
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

    fn storage_virtual_nodes(&self) -> Result<StorageVirtualNodes> {
        let state = self.topology_state()?;
        Ok(self.storage_virtual_nodes_for_topology(&state))
    }

    fn storage_virtual_nodes_for_topology(&self, state: &TopologyState) -> StorageVirtualNodes {
        let mut owners = BTreeSet::new();
        // Pre: `state` is this node's authenticated topology view.
        // Post: the virtual-owner set is exactly the physical DIDs currently
        // visible to storage routing: local, successors, predecessor, and
        // fingers. It is an observed view, not a global registry.
        owners.insert(state.local);
        owners.extend(state.successors.iter().copied());
        owners.extend(state.predecessor);
        owners.extend(state.fingers.iter().flatten().copied());
        StorageVirtualNodes::from_owners(self.storage_virtual_node_config(), owners)
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
        let state = self.topology_state()?;
        Ok(self.next_hop_for_storage_sync_in(&state, destination))
    }

    pub(crate) fn storage_sync_route_still_permits(
        &self,
        destination: StorageSyncDestination,
        next_hop: Did,
    ) -> Result<bool> {
        self.with_topology_state(|state| {
            self.storage_sync_route_permits_in(state, destination, next_hop)
        })
    }

    /// Execute `operation` only while one topology snapshot proves this route.
    ///
    /// The topology transition lock remains held through `operation`. This is
    /// the DHT half of final transport admission: the caller can synchronously
    /// check connection ownership and readiness without a route transition
    /// crossing that check.
    pub(crate) fn with_permitted_storage_sync_route<T>(
        &self,
        destination: StorageSyncDestination,
        next_hop: Did,
        operation: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        self.with_topology_state(|state| {
            self.storage_sync_route_permits_in(state, destination, next_hop)
                .then(operation)
        })
    }

    fn storage_sync_route_permits_in(
        &self,
        state: &TopologyState,
        destination: StorageSyncDestination,
        next_hop: Did,
    ) -> bool {
        Self::routing_peer_registered_in(state, next_hop)
            && self.next_hop_for_storage_sync_in(state, destination) == Some(next_hop)
    }

    fn routing_peer_registered_in(state: &TopologyState, peer: Did) -> bool {
        peer == state.local
            || state.successors.contains(&peer)
            || state.predecessor == Some(peer)
            || state.fingers.iter().flatten().any(|did| *did == peer)
    }

    // Pre: `state` is one authenticated topology snapshot.
    // Post: both route registration and next-hop selection use only `state`.
    fn next_hop_for_storage_sync_in(
        &self,
        state: &TopologyState,
        destination: StorageSyncDestination,
    ) -> Option<Did> {
        match destination {
            StorageSyncDestination::PhysicalOwner(owner) => {
                Self::next_hop_to_physical_owner_in(state, owner)
            }
            StorageSyncDestination::PlacementKey(key) => {
                self.next_hop_to_storage_placement_in(state, key)
            }
        }
    }

    fn next_hop_to_physical_owner_in(state: &TopologyState, owner: Did) -> Option<Did> {
        if owner == state.local {
            return None;
        }
        match topology::find_successor(state, owner) {
            // If this local view cannot prove a better physical next hop, try
            // the target owner directly rather than accepting the payload as
            // local work. Persisting happens only at relay destination.
            FindSuccessorStep::Local(next) if next == state.local => Some(owner),
            FindSuccessorStep::Local(next) | FindSuccessorStep::Remote { next, .. } => Some(next),
        }
    }

    fn next_hop_to_storage_placement_in(&self, state: &TopologyState, key: Did) -> Option<Did> {
        if let Some(owner) = self
            .storage_virtual_nodes_for_topology(state)
            .owner_for_key(key)
        {
            return (owner != state.local).then_some(owner);
        }
        match topology::find_successor(state, key) {
            FindSuccessorStep::Local(_) => None,
            FindSuccessorStep::Remote { next, .. } => Some(next),
        }
    }
}

#[cfg(all(not(all(feature = "wasm", target_family = "wasm")), test))]
mod tests;
