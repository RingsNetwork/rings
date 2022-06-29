use super::did::Did;
use super::subring::SubRing;
use super::vnode::VirtualNode;
use crate::err::Result;

pub trait Chord<A> {
    fn join(&mut self, id: Did) -> A;
    fn find_successor(&self, id: Did) -> Result<A>;
}

pub trait ChordStablize<A>: Chord<A> {
    fn closest_preceding_node(&self, id: Did) -> Result<Did>;
    fn check_predecessor(&self) -> A;
    fn notify(&mut self, id: Did) -> Option<Did>;
    fn fix_fingers(&mut self) -> Result<A>;
}

/// Protocol for Storage Data on Chord
pub trait ChordStorage<A>: Chord<A> {
    /// look up a resouce
    fn lookup(&self, id: &Did) -> Result<A>;
    /// Cache, cache fetched Data locally
    fn cache(&self, vnode: VirtualNode);
    /// Check localCache
    fn fetch_cache(&self, id: &Did) -> Option<VirtualNode>;
    /// store VNode to it's successor
    /// A VNode's successor should store the data
    fn store(&self, peer: VirtualNode) -> Result<A>;
    /// Batch store
    fn store_vec(&self, peer: Vec<VirtualNode>) -> Result<A>;
    /// When A Node's successor is updated, it should check the storage that
    /// if exist some VNode's address is in (self.id, new_successor), then
    /// sync the data to the new successor
    fn sync_with_successor(&self, new_successor: Did) -> Result<A>;
}

/// Trait for how dht manage SubRing
pub trait SubRingManager<A>: ChordStorage<A> {
    /// get subring from storage by id
    fn get_subring(&self, id: &Did) -> Option<Result<SubRing>>;
    /// get subring from storage by name
    fn get_subring_by_name(&self, name: &str) -> Option<Result<SubRing>>;
    /// store a subring to storage
    fn store_subring(&self, subring: &SubRing) -> Result<()>;
    /// get a subring for update
    fn get_subring_for_update(
        &self,
        id: &Did,
        callback: Box<dyn FnOnce(SubRing) -> SubRing>,
    ) -> Result<bool>;
    /// get a subring for update by name
    fn get_subring_for_update_by_name(
        &self,
        name: &str,
        callback: Box<dyn FnOnce(SubRing) -> SubRing>,
    ) -> Result<bool>;

    /// join a node to subring via given name
    /// When Node A join Channel C which's vnode is stored on Node B
    /// A send JoinSubRing to Address C, Node B got the Message And
    /// Update the Chord Finger Table, then, Node B Response it's finger table to A
    /// And Noti closest preceding node that A is Joined
    fn join_subring(&self, id: &Did, rid: &Did) -> Result<A>;

    /// search a cloest preceding node
    fn cloest_preceding_node_for_subring(&self, id: &Did, rid: &Did) -> Option<Result<Did>>;
}
