//! Implementation of Ring's DHT, which is based on CHORD
//! ref: <https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf>
//! With high probability, the number of nodes that must be contacted to find a successor in an N-node network is O(log N).

mod did;
pub use did::Did;
mod chord;
mod successor;
mod types;
pub use {chord::PeerRing, chord::PeerRingAction, chord::RemoteAction as PeerRingRemoteAction};
pub use {types::Chord, types::ChordStablize, types::ChordStorage};

mod stabilization;
pub use stabilization::{Stabilization, TStabilize};
/// Implement SubRing with VNode
pub mod subring;
/// VNode is a special node that only has virtual address
pub mod vnode;
