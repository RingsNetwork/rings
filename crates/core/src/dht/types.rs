//! DHT types about `Storage` and `PeerRing`.
#![warn(missing_docs)]
use async_trait::async_trait;

use super::chord::TopoInfo;
use super::did::Did;
use super::vnode::VNodeOperation;
use super::vnode::VirtualNode;
use crate::error::Result;

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
    /// This method should return the predecessor after updating.
    fn notify(&self, did: Did) -> Result<Did>;

    /// Fix finger table by finding the successor for each finger.
    /// According to the paper, this method should be called periodically.
    /// According to the paper, only one finger should be fixed at a time.
    fn fix_fingers(&self) -> Result<Action>;
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
pub trait ChordStorage<Action, const REDUNDANT: u16>: Chord<Action> {
    /// Look up a VirtualNode by its Did.
    /// Always finds resource by DHT, ignoring the local cache.
    async fn vnode_lookup(&self, vid: Did) -> Result<Action>;
    /// Store `vnode` if it's between current node and the successor of current node,
    /// otherwise find the responsible node and return as Action.
    async fn vnode_operate(&self, op: VNodeOperation) -> Result<Action>;
}

/// ChordStorageSync defines the synchronous vnode storage behavior.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorageSync<Action>: Chord<Action> {
    /// When the successor of a node is updated, it needs to check if there are
    /// `VirtualNode`s that are no longer between current node and `new_successor`,
    /// and sync them to the new successor.
    async fn sync_vnode_with_successor(&self, new_successor: Did) -> Result<Action>;
}

/// ChordStorageCache defines the basic API for getting and setting DHT cache storage.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorageCache<Action>: Chord<Action> {
    /// Cache fetched resource locally.
    async fn local_cache_put(&self, vnode: VirtualNode) -> Result<()>;
    /// Get local cache.
    async fn local_cache_get(&self, vid: Did) -> Result<Option<VirtualNode>>;
}

/// Chord online correction that inspired by Pamela Zave's work.
/// Ref: [How to Make Chord Correct](https://arxiv.org/pdf/1502.06461.pdf)
///
/// Correct Chord reveals two facts:
///
/// 1. Chord must be initialized with a ring containing a minimum of r + 1 nodes,
///    where r is the length of each node's list of successors. To be proven correct,
///    a Chord network must maintain a "stable base" of r + 1 nodes that remain members
///    of the network throughout its lifetime.
///
/// 2. The Chord paper defined the maintenance and use of finger tables, which improve
///    lookup speed by providing pointers that cross the ring like chords of a circle.
///    Because finger tables are an optimization and they are built from successors and
///    predecessors, correctness does not depend on them.
///
/// Based on the above facts, trait CorrectChord only focuses on handling join and stabilization
/// operations of Chord.
///
/// This trait defines three operations referred to in the paper:
///
/// - Join Operation
/// - Rectify Operation
/// - Stabilize Operation
///
/// This trait also defines two more methods:
///
/// - The `pre_stabilize` is the precondition of Stabilize Operation.
/// - `topo_info` is a helper function to get the topological info of the chord.
///
/// Some methods return an `Action`. The reason is the same as [Chord].
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait CorrectChord<Action>: Chord<Action> {
    /// Join Operation in the paper.
    ///
    /// First, the node asks the known node to look up the node's did and get its proper
    /// successor, storing the value as new successor. The node then queries new successor
    /// for its successor list (same as the original Chord). Finally, the node constructs
    /// its own successor list by concatenating new successor and new successor's successor
    /// list, with the last element of the list trimmed off to produce a result of fixed length.
    async fn join_then_sync(&self, did: impl LiveDid) -> Result<Action>;

    /// Rectify Operation in the paper.
    ///
    /// A node rectifies when it is notified.
    fn rectify(&self, pred: Did) -> Result<()>;

    /// Steps before Stabilize Operation.
    ///
    /// When a node fails or leaves, it ceases to stabilize, notify, or respond to queries
    /// from other nodes. When a node rejoins, it re-initializes its Chord variables. The node
    /// (self) queries its successor for its successor's predecessor and successor list.
    fn pre_stabilize(&self) -> Result<Action>;

    /// Stabilize operation in the paper.
    ///
    /// The node first updates its successor list with its successor's list. It then checks
    /// to see if the new pointer it has learned, its successor's predecessor, is an improved
    /// successor. If so, and if new successor is live, it adopts newSucc as its new successor.
    /// Thus the stabilize operation requires one or two queries for each traversal of the
    /// outer loop. Whether or not there is a live improved successor, the node notifies its
    /// successor of its own identity.
    fn stabilize(&self, succ: TopoInfo) -> Result<Action>;

    /// A helper function to get the topological
    /// info about the chord.
    fn topo_info(&self) -> Result<TopoInfo>;

    /// Hook of updating successor
    async fn update_successor(&self, did: impl LiveDid) -> Result<Action>;
    /// Hook of updating successor
    async fn extend_successor(&self, did: &[impl LiveDid]) -> Result<Action>;
}

/// Trait `LiveDid` defines a wrapper for `Did` that can check whether the `Did` is live or not.
///
/// Implementors of this trait must also be convertible into a `Did` type using the `Into` trait, and
/// must satisfy some additional constraints (see below).
#[cfg(feature = "wasm")]
#[async_trait(?Send)]
pub trait LiveDid: Into<Did> + Clone {
    /// Necessary method, should return true if a wrapped did is live.
    async fn live(&self) -> bool;
}

/// Trait `LiveDid` defines a wrapper for `Did` that can check whether the `Did` is live or not.
///
/// Implementors of this trait must also be convertible into a `Did` type using the `Into` trait, and
/// must satisfy some additional constraints (see below).
#[cfg(not(feature = "wasm"))]
#[async_trait]
pub trait LiveDid: Into<Did> + Clone + Send + Sync {
    /// Necessary method, should return true if a wrapped did is live.
    async fn live(&self) -> bool;
}
