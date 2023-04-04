//! DHT types about `Storage` and `PeerRing`.
#![warn(missing_docs)]
use async_trait::async_trait;

use super::chord::TopoInfo;
use super::did::Did;
use super::vnode::VNodeOperation;
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
    async fn vnode_lookup(&self, vid: Did) -> Result<Action>;
    /// Store `vnode` if it's between current node and the successor of current node,
    /// otherwise find the responsible node and return as Action.
    async fn vnode_operate(&self, op: VNodeOperation) -> Result<Action>;
    /// When the successor of a node is updated, it needs to check if there are
    /// `VirtualNode`s that are no longer between current node and `new_successor`,
    /// and sync them to the new successor.
    async fn sync_vnode_with_successor(&self, new_successor: Did) -> Result<Action>;
    /// Cache fetched resource locally.
    fn local_cache_set(&self, vnode: VirtualNode);
    /// Get local cache.
    fn local_cache_get(&self, vid: Did) -> Option<VirtualNode>;
}

/// Chord implementation from Pamela Zave's work.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait CorrectChord<Action>: Chord<Action> {
    /// Join Operation
    async fn join_then_sync(&self, did: Did) -> Result<Action>;
    /// Rectify Operation
    async fn rectify(&self, pred: Did) -> Result<()>;
    /// Steps before stablize
    async fn pre_stablize(&self) -> Result<Action>;
    /// Stabilize operation for successor list
    async fn stabilize(&self, succ: TopoInfo) -> Result<()>;
}
