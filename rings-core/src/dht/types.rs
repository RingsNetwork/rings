use super::did::Did;
use super::vnode::VirtualNode;
use crate::err::Result;

pub trait Chord<A> {
    fn join(&mut self, id: Did) -> A;
    fn find_successor(&self, id: Did) -> Result<A>;
}

pub trait ChordStablize<A>: Chord<A> {
    fn closest_preceding_node(&self, id: Did) -> Result<Did>;
    fn check_predecessor(&self) -> A;
    fn stablilize(&mut self) -> A;
    fn notify(&mut self, id: Did) -> Option<Did>;
    fn fix_fingers(&mut self) -> Result<A>;
}

/// Protocol for Storage Data on Chord
pub trait ChordStorage<A>: Chord<A> {
    /// look up a resouce
    fn lookup(&self, id: &Did) -> Result<A>;
    /// Cache, cache fetched Data locally
    fn cache(&self, vnode: VirtualNode) -> ();
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
