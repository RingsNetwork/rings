//! DHT types about `Storage` and `Subring`.
#![warn(missing_docs)]
use async_trait::async_trait;

use super::did::Did;
use super::subring::SubRing;
use super::vnode::VirtualNode;
use crate::err::Result;

/// Chord is a distributed hash table (DHT) algorithm that is designed to efficiently
/// distribute data across peer-to-peer network nodes. You may want to browse its
/// [wiki](https://en.wikipedia.org/wiki/Chord_(peer-to-peer)) before you read this.
///
/// A basic usage of Chord in rings network is to assist the nodes in passing messages
/// so that they can forward data with fewer connections. In this situation, the key
/// of Chord is the unique identifier of a node, which we call [Did]. Then if we connect
/// all the nodes in the finger table for every node, we construct a [PeerRing](super::PeerRing).
/// It's the basic construction of the rings network. When passing a message to a
/// destination node, we can simply use `find_successor` to find the next node that is
/// responsible for passing a message. And it takes O(log n) time complexity and O(log n)
/// connections to pass a message from one node to destination node.
///
/// Some methods return an `Action` which is used to tell outer the extra action to take
/// after handling data inside the struct. It's useful since the struct may not work
/// for managing whole data but for giving strategies by data inside.
pub trait Chord<Action> {
    /// Join a DHT containing a node identified by `did`.
    fn join(&self, did: Did) -> Result<Action>;

    /// Ask DHT for the successor of Did.
    /// May return a remote action for the successor is recorded in another node.
    fn find_successor(&self, did: Did) -> Result<Action>;

    /// Notify the DHT that a node is its predecessor.
    /// According to the paper, this method should be called periodically.
    fn notify(&self, did: Did) -> Result<Option<Did>>;

    /// Fix finger table by finding the successor for each finger.
    /// According to the paper, this method should be called periodically.
    /// According to the paper, only one finger should be fixed at a time.
    fn fix_fingers(&self) -> Result<Action>;

    //TODO: Why `check_predecessor` is not using?

    /// Check if predecessor is alive.
    /// According to the paper, this method should be called periodically.
    fn check_predecessor(&self) -> Result<Action>;
}

/// ChordStorage is a distributed storage protocol based on Chord algorithm.
///
/// The core concept is to find the node that is responsible for storing a resource. In
/// ChordStorage protocol, we will generate a Did for a resource. Then find the node
/// whose Did is the predecessor of that resource's Did. Save the resource in its
/// predecessor node.
///
/// To accomplish this, all resources stored by this protocol will be wrapped in
/// [VirtualNode](super::vnode::VirtualNode).
///
/// Known that although the Did of a `VirtualNode` has the same data type as the Did of a
/// node (they both can be used as key for the DHT), since the `VirtualNode` is only a
/// logical node, it will not be selected to be connected as a real node, but will only
/// be classified as the predecessor of a real node.
///
/// Some methods return an `Action`. It's because the real storing node may not be this
/// node. The outer should take the action to forward the request to the real storing
/// node.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorage<Action>: Chord<Action> {
    /// Look up a VirtualNode by its Did.
    /// Always finds resource by DHT, ignoring the local cache.
    async fn lookup(&self, vid: Did) -> Result<Action>;
    /// Store `vnode` if it's between current node and the successor of current node,
    /// otherwise find the responsible node and return as Action.
    async fn store(&self, vnode: VirtualNode) -> Result<Action>;
    /// When the successor of a node is updated, it needs to check if there are
    /// `VirtualNode`s that are no longer between current node and `new_successor`,
    /// and sync them to the new successor.
    async fn sync_with_successor(&self, new_successor: Did) -> Result<Action>;
    /// Cache fetched resource locally.
    fn local_cache_set(&self, vnode: VirtualNode);
    /// Get local cache.
    fn local_cache_get(&self, vid: Did) -> Option<VirtualNode>;
}

/// Trait for how dht manage SubRing
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait SubRingManager<Action>: ChordStorage<Action> {
    /// get subring from storage by id
    async fn get_subring(&self, rid: Did) -> Result<SubRing>;
    /// get subring from storage by name
    async fn get_subring_by_name(&self, name: &str) -> Result<SubRing>;
    /// store a subring to storage
    async fn store_subring(&self, subring: &SubRing) -> Result<()>;
    /// get a subring for update
    // async fn get_subring_for_update(
    //     &self,
    //     did: Did,
    //     callback: Arc<dyn FnOnce(SubRing) -> SubRing>,
    // ) -> Result<bool>;
    // /// get a subring for update by name
    // async fn get_subring_for_update_by_name(
    //     &self,
    //     name: &str,
    //     callback: Box<dyn FnOnce(SubRing) -> SubRing>,
    // ) -> Result<bool>;

    /// join a node to subring via given name
    /// When Node A join Channel C which's vnode is stored on Node B
    /// A send JoinSubRing to Address C, Node B got the Message And
    /// Update the Chord Finger Table, then, Node B Response it's finger table to A
    /// And Noti closest preceding node that A is Joined
    async fn join_subring(&self, did: Did, rid: Did) -> Result<Action>;
}
