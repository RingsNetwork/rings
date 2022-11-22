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
/// all the nodes in the finger table, we can construct a [PeerRing](super::PeerRing).
/// It's the basic construction of the rings network. On every connected node, we can
/// simply use `find_successor` to find the next node that is responsible for passing
/// a message. And it takes O(log n) time complexity and O(log n) connections to pass
/// a message from one node to destination node.
///
/// This trait defines interfaces for Chord.
/// The implementation of Chord is in [PeerRing](super::PeerRing).
///
/// Some methods return an `Action` which is used to tell outer the extra action to take
/// after handling datas inside the struct. It's useful since the struct may not work
/// for managing whole datas but for giving strategies by datas inside.
pub trait Chord<Action> {
    /// Join another Did into the DHT.
    fn join(&self, id: Did) -> Result<Action>;

    /// Ask DHT for the successor of Did. May return a remote action for the successor is
    /// recorded in another node.
    fn find_successor(&self, id: Did) -> Result<Action>;

    //TODO: why closest_preceding_node and check_predecessor is not using.

    /// Find the predecessor of the DHT.
    fn closest_preceding_node(&self, id: Did) -> Result<Did>;

    /// Check if predecessor is alive.
    /// According to the paper, this method should be called periodically.
    fn check_predecessor(&self) -> Result<Action>;

    /// Notify the DHT that a node is its predecessor.
    /// According to the paper, this method should be called periodically.
    fn notify(&self, id: Did) -> Result<Option<Did>>;

    /// Fix finger table by finding the successor for each finger.
    /// According to the paper, this method should be called periodically.
    /// According to the paper, only one finger should be fixed at a time.
    fn fix_fingers(&self) -> Result<Action>;
}

/// Protocol for Storage Data on Chord
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorage<Action>: Chord<Action> {
    /// look up a resouce
    async fn lookup(&self, id: &Did) -> Result<Action>;
    /// Cache, cache fetched Data locally
    fn cache(&self, vnode: VirtualNode);
    /// Check localCache
    fn fetch_cache(&self, id: &Did) -> Option<VirtualNode>;
    /// store VNode to it's successor
    /// A VNode's successor should store the data
    async fn store(&self, peer: VirtualNode) -> Result<Action>;
    /// Batch store
    async fn store_vec(&self, peer: Vec<VirtualNode>) -> Result<Action>;
    /// When A Node's successor is updated, it should check the storage that
    /// if exist some VNode's address is in (self.id, new_successor), then
    /// sync the data to the new successor
    async fn sync_with_successor(&self, new_successor: Did) -> Result<Action>;
}

/// Trait for how dht manage SubRing
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait SubRingManager<Action>: ChordStorage<Action> {
    /// get subring from storage by id
    async fn get_subring(&self, id: &Did) -> Result<SubRing>;
    /// get subring from storage by name
    async fn get_subring_by_name(&self, name: &str) -> Result<SubRing>;
    /// store a subring to storage
    async fn store_subring(&self, subring: &SubRing) -> Result<()>;
    /// get a subring for update
    // async fn get_subring_for_update(
    //     &self,
    //     id: &Did,
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
    async fn join_subring(&self, id: &Did, rid: &Did) -> Result<Action>;

    /// search a cloest preceding node
    async fn cloest_preceding_node_for_subring(&self, id: &Did, rid: &Did) -> Option<Result<Did>>;
}
