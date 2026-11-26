//! Runtime inspection snapshots and compact topology projections.

/// Compact iterator compression for inspection snapshots.
mod compress;
/// Runtime inspection snapshot data structures.
mod snapshot;

pub use compress::compress_iter;
pub use snapshot::ConnectionInspect;
pub use snapshot::DHTInspect;
pub use snapshot::MailboxStorageInspect;
pub use snapshot::StorageInspect;
pub use snapshot::SwarmInspect;

#[cfg(test)]
mod test_inspect;
