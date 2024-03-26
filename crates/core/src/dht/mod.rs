#![warn(missing_docs)]
//! Implementation of Ring's DHT
//! which is based on CHORD, ref: <https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf>
//! With high probability, the number of nodes that must be contacted to find a successor in an N-node network is O(log N).

mod chord;
pub mod did;
/// Finger table for Rings
pub mod finger;
mod stabilization;
/// Implement Subring with VNode
pub mod subring;
pub mod successor;
pub mod types;
/// VNode is a special node that only has virtual address
pub mod vnode;

pub use chord::PeerRing;
pub use chord::PeerRingAction;
pub use chord::RemoteAction as PeerRingRemoteAction;
pub use chord::TopoInfo;
pub use chord::VNodeStorage;
pub use did::Did;
pub use finger::FingerTable;
pub use stabilization::Stabilizer;
pub use successor::SuccessorReader;
pub use successor::SuccessorWriter;
pub use types::Chord;
pub use types::ChordStorage;
pub use types::ChordStorageCache;
pub use types::ChordStorageSync;
pub use types::CorrectChord;
pub use types::LiveDid;

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::ecc::tests::gen_ordered_keys;

    pub fn gen_ordered_dids(n: usize) -> Vec<Did> {
        gen_ordered_keys(n)
            .iter()
            .map(|x| x.address().into())
            .collect()
    }
}
