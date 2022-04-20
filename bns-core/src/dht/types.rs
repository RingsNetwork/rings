use super::did::Did;
use super::peer::VirtualPeer;
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

pub trait ChordStorage<A>: Chord<A> {
    fn lookup(&self, id: Did) -> Result<A>;
    fn store(&self, peer: VirtualPeer) -> Result<A>;
}
