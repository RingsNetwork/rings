//! Implementation of Ring's DHT, which is based on CHORD
//! ref: <https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf>
//! With high probability, the number of nodes that must be contacted to find a successor in an N-node network is O(log N).

mod did;
pub use did::Did;
mod chord;
/// Finger table for Rings
pub mod finger;
mod successor;
mod types;
pub use chord::PeerRing;
pub use chord::PeerRingAction;
pub use chord::RemoteAction as PeerRingRemoteAction;
pub use finger::FingerTable;
pub use types::Chord;
pub use types::ChordStabilize;
pub use types::ChordStorage;
pub use types::SubRingManager;
mod stabilization;
pub use stabilization::Stabilization;
pub use stabilization::TStabilize;
/// Implement SubRing with VNode
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
