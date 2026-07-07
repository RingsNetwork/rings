#![warn(missing_docs)]
//! Implementation of Ring's DHT
//! which is based on CHORD, ref: <https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf>
//! With high probability, the number of nodes that must be contacted to find a successor in an N-node network is O(log N).

mod chord;
pub mod did;
/// Storage entry model used by Chord-backed DHT storage.
pub mod entry;
/// Finger table for Rings
pub mod finger;
mod stabilization;
mod storage;
/// Subring model stored through DHT entries.
pub mod subring;
pub mod successor;
/// Pure Chord topology transition model.
pub mod topology;
pub mod types;
/// Chord-style virtual positions for storage ownership.
pub mod virtual_node;

pub use chord::EntryStorage;
pub use chord::PeerRing;
pub use chord::PeerRingAction;
pub use chord::RemoteAction as PeerRingRemoteAction;
pub use chord::TopoInfo;
pub use did::Did;
pub use finger::FingerTable;
pub use finger::DEFAULT_FINGER_TABLE_SIZE;
pub use stabilization::Stabilizer;
pub(crate) use storage::StorageSyncDelivery;
pub use storage::StorageSyncDestination;
pub use storage::StorageSyncPurpose;
pub use storage::StorageSyncRoute;
pub use successor::SuccessorReader;
pub use successor::SuccessorWriter;
pub use types::Chord;
pub use types::ChordStorage;
pub use types::ChordStorageCache;
pub use types::ChordStorageRepair;
pub use types::ChordStorageSync;
pub use types::CorrectChord;
pub use types::LiveDid;
pub use virtual_node::StorageVirtualNodes;
pub use virtual_node::VirtualNode;
pub use virtual_node::VirtualNodeConfig;
pub use virtual_node::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;

#[cfg(test)]
pub mod tests {
    //! test
    use super::*;
    use crate::ecc::tests::gen_ordered_keys;

    /// Test get ordered did list
    pub fn gen_ordered_dids(n: usize) -> Vec<Did> {
        gen_ordered_keys(n)
            .iter()
            .map(|x| x.address().into())
            .collect()
    }
}
