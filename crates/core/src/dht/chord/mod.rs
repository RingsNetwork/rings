//! Chord peer-ring model and protocol implementations.

mod action;
mod carrier_store;
mod peer_ring;
mod storage;

pub use action::PeerRingAction;
pub use action::RemoteAction;
pub use action::TopoInfo;
pub use carrier_store::ChargedRegister;
pub use carrier_store::ChargedRemove;
pub use carrier_store::RemovalRecord;
pub use carrier_store::RemovalStorage;
pub(crate) use carrier_store::REMOVAL_SHARES;
pub use peer_ring::EntryStorage;
pub use peer_ring::PeerRing;
pub(crate) use peer_ring::ReplicaAcknowledgement;
pub(crate) use peer_ring::ReplicationStep;
pub(crate) use storage::LookupSettlement;
pub(crate) use storage::OperateRoute;
pub(crate) use storage::PlacementLookup;
pub(crate) use storage::StorageKey;

#[cfg(all(not(all(feature = "wasm", target_family = "wasm")), test))]
mod tests;
