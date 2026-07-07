#![warn(missing_docs)]
//! Chord-style virtual positions for storage ownership.
//!
//! These positions are not independent signing identities. They are derived
//! ring locations owned by an authenticated physical DID and are used only by
//! storage owner selection.

use std::collections::BTreeSet;

use ethereum_types::H160;
use sha1::Digest;
use sha1::Sha1;

use crate::dht::topology;
use crate::dht::Did;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;

const VIRTUAL_NODE_DOMAIN: &[u8] = b"rings:vnode";

/// Maximum virtual storage positions derived per physical owner.
pub const MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER: u16 = 256;

/// Default virtual storage positions derived per physical owner.
///
/// The Chord paper recommends mapping each real node to O(log N) virtual nodes.
/// A joining node does not know the stable network size N before it enters the
/// DHT, so Rings uses the Chord finger-table width as the network-wide default
/// O(log N) operating point. Operators can still set this to zero to disable
/// virtual storage ownership for a network.
pub const DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER: u16 = DEFAULT_FINGER_TABLE_SIZE as u16;

/// Return the default virtual storage positions derived per physical owner.
pub const fn default_storage_virtual_positions_per_owner() -> u16 {
    DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
}

/// Configuration for storage virtual nodes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualNodeConfig {
    /// Network namespace mixed into derived virtual positions.
    network_id: u32,
    /// Number of virtual positions derived for each physical owner.
    positions_per_owner: u16,
}

impl VirtualNodeConfig {
    /// Disable virtual-node storage ownership.
    pub const fn disabled() -> Self {
        Self {
            network_id: 0,
            positions_per_owner: 0,
        }
    }

    /// Build a virtual-node configuration.
    ///
    /// Post: `positions_per_owner <= MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER`.
    pub const fn new(network_id: u32, positions_per_owner: u16) -> Self {
        Self {
            network_id,
            positions_per_owner: Self::bounded_positions_per_owner(positions_per_owner),
        }
    }

    /// Returns whether this configuration enables virtual storage ownership.
    pub const fn is_enabled(self) -> bool {
        self.positions_per_owner > 0
    }

    /// Return the network namespace mixed into derived virtual positions.
    pub const fn network_id(self) -> u32 {
        self.network_id
    }

    /// Return the bounded number of positions derived for each physical owner.
    pub const fn positions_per_owner(self) -> u16 {
        self.positions_per_owner
    }

    /// Returns whether `positions_per_owner` is inside the configured cost bound.
    pub const fn positions_per_owner_within_limit(positions_per_owner: u16) -> bool {
        positions_per_owner <= MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    }

    const fn bounded_positions_per_owner(positions_per_owner: u16) -> u16 {
        if positions_per_owner > MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER {
            MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
        } else {
            positions_per_owner
        }
    }
}

/// One derived Chord ring position owned by a physical peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualNode {
    /// Physical peer DID that signs messages and owns the transport.
    pub owner_did: Did,
    /// Derived Chord ring position used for storage ownership.
    pub vnode_did: Did,
    /// Owner-local virtual-node index.
    pub index: u16,
}

impl VirtualNode {
    /// Derive one virtual position.
    pub fn derive(network_id: u32, owner_did: Did, index: u16) -> Self {
        let mut hasher = Sha1::new();
        hasher.update(VIRTUAL_NODE_DOMAIN);
        hasher.update(network_id.to_be_bytes());
        hasher.update(owner_did.as_bytes());
        hasher.update(index.to_be_bytes());
        let bytes: [u8; 20] = hasher.finalize().into();
        Self {
            owner_did,
            vnode_did: Did::from(H160::from(bytes)),
            index,
        }
    }
}

/// Storage-owner registry for derived virtual positions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageVirtualNodes {
    config: VirtualNodeConfig,
    owners: BTreeSet<Did>,
}

impl StorageVirtualNodes {
    /// Create an empty registry.
    pub fn new(config: VirtualNodeConfig) -> Self {
        Self {
            config,
            owners: BTreeSet::new(),
        }
    }

    /// Create a registry from known physical owners.
    pub fn from_owners(config: VirtualNodeConfig, owners: impl IntoIterator<Item = Did>) -> Self {
        let mut registry = Self::new(config);
        for owner in owners {
            registry.register_owner(owner);
        }
        registry
    }

    /// Return the active configuration.
    pub const fn config(&self) -> VirtualNodeConfig {
        self.config
    }

    /// Returns whether this registry enables virtual storage ownership.
    pub const fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    /// Register one physical owner.
    ///
    /// Post: if virtual nodes are disabled, the registry is unchanged.
    pub fn register_owner(&mut self, owner_did: Did) {
        if self.is_enabled() {
            self.owners.insert(owner_did);
        }
    }

    /// Remove one physical owner.
    pub fn unregister_owner(&mut self, owner_did: Did) {
        self.owners.remove(&owner_did);
    }

    /// Returns whether this physical owner is registered.
    pub fn contains_owner(&self, owner_did: Did) -> bool {
        self.owners.contains(&owner_did)
    }

    /// Return all virtual positions for `owner_did`.
    pub fn positions_for_owner(&self, owner_did: Did) -> Vec<VirtualNode> {
        if !self.contains_owner(owner_did) {
            return Vec::new();
        }
        self.derive_owner_positions(owner_did)
    }

    /// Return all registered virtual positions.
    pub fn positions(&self) -> Vec<VirtualNode> {
        let mut positions = Vec::new();
        for owner in self.owners.iter().copied() {
            positions.extend(self.derive_owner_positions(owner));
        }
        positions.sort_by_key(|position| (position.vnode_did, position.owner_did, position.index));
        positions
    }

    /// Resolve the physical owner responsible for `key`.
    ///
    /// This uses Chord successor ownership: the selected virtual position is
    /// the registered position with minimum clockwise distance from `key`.
    pub fn owner_for_key(&self, key: Did) -> Option<Did> {
        self.positions()
            .into_iter()
            .min_by(|left, right| {
                topology::dist(key, left.vnode_did)
                    .cmp(&topology::dist(key, right.vnode_did))
                    .then_with(|| left.owner_did.cmp(&right.owner_did))
                    .then_with(|| left.index.cmp(&right.index))
            })
            .map(|position| position.owner_did)
    }

    fn derive_owner_positions(&self, owner_did: Did) -> Vec<VirtualNode> {
        (0..self.config.positions_per_owner())
            .map(|index| VirtualNode::derive(self.config.network_id(), owner_did, index))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_node_derivation_is_domain_separated_by_network() {
        let owner = Did::from(42u32);
        let first = VirtualNode::derive(7, owner, 0);
        let repeated = VirtualNode::derive(7, owner, 0);
        let other_network = VirtualNode::derive(8, owner, 0);

        assert_eq!(first, repeated);
        assert_ne!(first.vnode_did, owner);
        assert_ne!(first.vnode_did, other_network.vnode_did);
    }

    #[test]
    fn virtual_node_config_caps_positions_at_cost_bound() {
        let config =
            VirtualNodeConfig::new(1, MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER.saturating_add(1));

        assert_eq!(
            config.positions_per_owner(),
            MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
        );
    }

    #[test]
    fn storage_virtual_nodes_resolve_owner_from_virtual_position() -> crate::error::Result<()> {
        let owner_a = Did::from(10u32);
        let owner_b = Did::from(20u32);
        let registry =
            StorageVirtualNodes::from_owners(VirtualNodeConfig::new(1, 2), [owner_a, owner_b]);

        let owner_b_position = registry
            .positions_for_owner(owner_b)
            .into_iter()
            .next()
            .map(|position| position.vnode_did)
            .ok_or_else(|| {
                crate::error::Error::InvalidMessage(
                    "owner should have virtual positions".to_string(),
                )
            })?;

        assert_eq!(registry.owner_for_key(owner_b_position), Some(owner_b));
        Ok(())
    }

    #[test]
    fn storage_virtual_nodes_resolve_successor_owner_for_interval_key() -> crate::error::Result<()>
    {
        let owners = [Did::from(10u32), Did::from(20u32), Did::from(30u32)];
        let registry = StorageVirtualNodes::from_owners(VirtualNodeConfig::new(1, 2), owners);
        let positions = registry.positions();

        let Some((left, successor)) =
            positions
                .iter()
                .zip(positions.iter().cycle().skip(1))
                .find(|(left, successor)| {
                    let key = left.vnode_did + Did::from(1u32);
                    key != successor.vnode_did
                })
        else {
            return Err(crate::error::Error::InvalidMessage(
                "expected a non-empty virtual interval".to_string(),
            ));
        };

        let key = left.vnode_did + Did::from(1u32);
        assert_eq!(registry.owner_for_key(key), Some(successor.owner_did));
        Ok(())
    }

    #[test]
    fn unregister_owner_removes_virtual_positions() {
        let owner = Did::from(10u32);
        let mut registry = StorageVirtualNodes::new(VirtualNodeConfig::new(1, 3));
        registry.register_owner(owner);
        registry.unregister_owner(owner);

        assert!(registry.positions().is_empty());
        assert_eq!(registry.owner_for_key(Did::from(11u32)), None);
    }
}
