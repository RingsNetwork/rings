//! Runtime inspection snapshots and compact topology projections.

mod compress;
mod snapshot;

pub use compress::compress_iter;
pub use snapshot::ConnectionInspect;
pub use snapshot::DHTInspect;
pub use snapshot::StorageInspect;
pub use snapshot::SwarmInspect;

#[cfg(test)]
mod tests;
