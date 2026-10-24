//! Read-only views of a [`PeerRing`].
//!
//! Every view observes one committed state and mutates nothing. A pure view runs under the
//! transition lock ([`PeerRing::with_topology_state`]); a delivery decision, which consults the
//! transport's link predicate, runs on an owned snapshot after the lock is released:
//!
//! ```text
//! Law (Lock order) : lifecycle ≺ topology_transition
//! ```
//!
//! Admission and retirement hold the transport's connection lifecycle boundary while they
//! commit a topology transition, and the link predicate takes that boundary. Evaluating the
//! predicate under the transition lock would acquire the two locks in the opposite order, and
//! a routed send would deadlock against a concurrent admission (#946).

use super::PeerRing;
use crate::dht::delivery;
use crate::dht::delivery::NextHop;
use crate::dht::delivery::Origination;
use crate::dht::delivery::RouteStage;
use crate::dht::successor::SuccessorSeq;
use crate::dht::topology;
use crate::dht::topology::TopologyState;
use crate::dht::virtual_node::VirtualNodeConfig;
use crate::dht::Did;
use crate::error::Result;
use crate::lifecycle::epoch::Epoch;

/// Read-only views.
impl PeerRing {
    /// Return the successor sequence.
    pub fn successors(&self) -> SuccessorSeq {
        self.successor_seq.clone()
    }

    /// The overlay this ring belongs to; every stored value is admitted inside it.
    pub const fn network_id(&self) -> u32 {
        self.storage_virtual_node_config.network_id()
    }

    /// Storage virtual-node configuration used by the DHT storage layer.
    pub(in crate::dht) const fn storage_virtual_node_config(&self) -> VirtualNodeConfig {
        self.storage_virtual_node_config
    }

    /// The epoch of committed route changes (`Law (Epoch)` in the module documentation): a
    /// waiter that listens before routing is notified by every later route change.
    pub(crate) const fn topology_epoch(&self) -> &Epoch {
        &self.topology_epoch
    }

    /// An owned copy of the current topology state, assembled under the transition lock and
    /// returned after the lock is released.
    pub(crate) fn topology_state(&self) -> Result<TopologyState> {
        let _transition = self.lock_transition()?;
        self.snapshot_unlocked()
    }

    /// Run a pure observation `TopologyState → T` against one coherent snapshot.
    ///
    /// The transition lock is held while the snapshot is assembled, so
    /// `observe` sees exactly the state the next transition would start from,
    /// never a torn view in which one field has already moved on. `observe`
    /// borrows the snapshot, cannot reach the ring, and must not block, because
    /// every transition waits behind the lock while it runs. Nor may it acquire
    /// any lock ordered before `topology_transition` (`Law (Lock order)` in the
    /// module documentation): it may run under a lifecycle boundary its caller
    /// already holds, as the storage-sync route check does, but must not take
    /// one. An observation that needs the transport's link predicate runs on
    /// [`Self::topology_state`] instead.
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn with_topology_state<T>(
        &self,
        observe: impl FnOnce(&TopologyState) -> T,
    ) -> Result<T> {
        let _transition = self.lock_transition()?;
        let state = self.snapshot_unlocked()?;
        Ok(observe(&state))
    }

    /// Whether this node is the Chord successor of the position `did`, i.e.
    /// `did ∈ (predecessor, self]`. With no known predecessor the node is responsible only when
    /// it stands alone: a node that has successors but has not yet learned its predecessor is
    /// merely uninformed, not responsible for the whole ring. A message addressed to an
    /// unreachable `did` in this interval has reached the node that must hold it.
    pub(crate) fn is_responsible_for(&self, did: Did) -> Result<bool> {
        self.with_topology_state(|state| topology::is_responsible_for(state, did))
    }

    /// The delivery decision for a payload addressed to the node `destination` whose carrier
    /// is in `stage`, against one coherent snapshot; see [`delivery::delivery_step`] for the
    /// step and its laws. `linked` is the transport's direct-link predicate, evaluated after
    /// the transition lock is released (`Law (Lock order)`).
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn delivery_step(
        &self,
        destination: Did,
        stage: RouteStage,
        linked: impl Fn(Did) -> bool,
    ) -> Result<Option<NextHop>> {
        let view = self.topology_state()?;
        Ok(delivery::delivery_step(&view, destination, stage, linked))
    }

    /// The peer this node names for its answers while no node is known to route to it, among
    /// the peers `linked` holds; see [`delivery::reply_via`]. `linked` is evaluated after the
    /// transition lock is released (`Law (Lock order)`).
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn reply_via(&self, linked: impl Fn(Did) -> bool) -> Result<Option<Did>> {
        let view = self.topology_state()?;
        Ok(delivery::reply_via(&view, linked))
    }

    /// The first hop and the `reply_via` of a request this node originates toward
    /// `destination`, from one coherent snapshot; see [`delivery::origination`]. `linked` is
    /// evaluated after the transition lock is released (`Law (Lock order)`).
    ///
    /// # Errors
    ///
    /// Returns an error when a backing lock is poisoned.
    pub(crate) fn origination(
        &self,
        destination: Did,
        linked: impl Fn(Did) -> bool,
    ) -> Result<Origination> {
        let view = self.topology_state()?;
        Ok(delivery::origination(&view, destination, linked))
    }
}

#[cfg(test)]
mod test_views;
