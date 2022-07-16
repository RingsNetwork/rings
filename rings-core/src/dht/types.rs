use async_trait::async_trait;

use super::did::Did;
use super::subring::SubRing;
use super::vnode::VirtualNode;
use crate::err::Result;

pub trait Chord<A> {
    fn join(&self, id: Did) -> Result<A>;
    fn find_successor(&self, id: Did) -> Result<A>;
}

pub trait ChordStabilize<A>: Chord<A> {
    fn closest_preceding_node(&self, id: Did) -> Result<Did>;
    fn check_predecessor(&self) -> Result<A>;
    fn notify(&self, id: Did) -> Result<Option<Did>>;
    fn fix_fingers(&self) -> Result<A>;
}

/// Protocol for Storage Data on Chord
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ChordStorage<A>: Chord<A> {
    /// look up a resouce
    async fn lookup(&self, id: &Did) -> Result<A>;
    /// Cache, cache fetched Data locally
    fn cache(&self, vnode: VirtualNode);
    /// Check localCache
    fn fetch_cache(&self, id: &Did) -> Option<VirtualNode>;
    /// store VNode to it's successor
    /// A VNode's successor should store the data
    async fn store(&self, peer: VirtualNode) -> Result<A>;
    /// Batch store
    async fn store_vec(&self, peer: Vec<VirtualNode>) -> Result<A>;
    /// When A Node's successor is updated, it should check the storage that
    /// if exist some VNode's address is in (self.id, new_successor), then
    /// sync the data to the new successor
    async fn sync_with_successor(&self, new_successor: Did) -> Result<A>;
}

/// Trait for how dht manage SubRing
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait SubRingManager<A>: ChordStorage<A> {
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
    async fn join_subring(&self, id: &Did, rid: &Did) -> Result<A>;

    /// search a cloest preceding node
    async fn cloest_preceding_node_for_subring(&self, id: &Did, rid: &Did) -> Option<Result<Did>>;
}
