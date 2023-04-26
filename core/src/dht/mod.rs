//! Implementation of Ring's DHT
//!
//! which is based on CHORD, ref: <https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf>
//! With high probability, the number of nodes that must be contacted to find a successor in an N-node network is O(log N).
pub mod did;
pub use did::Did;
mod chord;
/// Finger table for Rings
pub mod finger;
pub mod successor;
pub use successor::SuccessorReader;
pub use successor::SuccessorWriter;
mod types;
pub use chord::PeerRing;
pub use chord::PeerRingAction;
pub use chord::RemoteAction as PeerRingRemoteAction;
pub use chord::TopoInfo;
pub use finger::FingerTable;
pub use types::Chord;
pub use types::ChordStorage;
pub use types::CorrectChord;
mod stabilization;
pub use stabilization::Stabilization;
pub use stabilization::TStabilizeExecute;
pub use stabilization::TStabilizeWait;

/// Implement Subring with VNode
pub mod subring;
/// VNode is a special node that only has virtual address
pub mod vnode;

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
