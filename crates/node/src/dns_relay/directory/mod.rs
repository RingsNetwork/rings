//! The relay's directory: an owner's announcement, read from its descriptor in the DHT.
//!
//! ```text
//!   announcement(d) = announcement(σ)    σ = the newest descriptor on service_topic(d) that is
//!                                            signed for d, verifies and is live
//!                   = ⊥                  no such descriptor, or it does not validate
//!                   = Err                the DHT fetch failed
//! ```
//!
//! Each call is one fetch: the relay keeps its own memory of answers in front of it
//! ([`Recall`](rings_dns_relay::Recall)), so a query or a connection rarely reaches the DHT.

use std::sync::Arc;

use async_trait::async_trait;
use rings_core::dht::entry::Entry;
use rings_core::utils::get_epoch_ms;
use rings_dns_relay::Announcement;
use rings_dns_relay::Directory;
use thiserror::Error;

use super::descriptor::service_topic;
use super::descriptor::ServiceDescriptor;
use super::DidOwner;
use crate::descriptor::RegistryElement;
use crate::prelude::rings_core::dht::Did;
use crate::processor::Processor;

/// Why the DHT could not tell what an owner announces.
#[derive(Debug, Error)]
pub enum DirectoryError {
    /// The DHT key of the owner's service topic cannot be derived.
    #[error("no DHT key for {did}'s service topic: {source}")]
    Key {
        /// The owner.
        did: Did,
        /// Why.
        source: rings_core::error::Error,
    },
    /// The fetch of the owner's service topic failed.
    #[error("cannot fetch {did}'s service descriptor: {source}")]
    Fetch {
        /// The owner.
        did: Did,
        /// Why.
        source: crate::error::Error,
    },
}

/// The announcement the newest valid descriptor among `elements` makes for `did`, at `now_ms`
/// in overlay `network_id`.
fn announcement_in(
    elements: &[bytes::Bytes],
    did: Did,
    now_ms: u128,
    network_id: u32,
) -> Option<Announcement> {
    let descriptors = elements
        .iter()
        .filter_map(|element| ServiceDescriptor::from_element(element).ok());
    let latest = ServiceDescriptor::latest_valid_for(descriptors, did, now_ms, network_id)?;
    match latest.announcement() {
        Ok(announcement) => Some(announcement),
        Err(error) => {
            tracing::debug!("dns relay: {did} announces an invalid descriptor: {error}");
            None
        }
    }
}

/// The directory over a node's DHT.
pub struct DhtDirectory {
    processor: Arc<Processor>,
}

impl DhtDirectory {
    /// The directory reading `processor`'s DHT.
    pub fn new(processor: Arc<Processor>) -> Self {
        Self { processor }
    }
}

#[async_trait]
impl Directory for DhtDirectory {
    type Error = DirectoryError;
    type Owner = DidOwner;

    async fn announcement(&self, owner: DidOwner) -> Result<Option<Announcement>, DirectoryError> {
        let did = owner.0;
        let key = Entry::gen_did(&service_topic(did))
            .map_err(|source| DirectoryError::Key { did, source })?;
        let entry = self
            .processor
            .fetch_storage_entry(key)
            .await
            .map_err(|source| DirectoryError::Fetch { did, source })?;
        let now_ms = get_epoch_ms();
        let network_id = self.processor.swarm.network_id();
        Ok(entry.and_then(|entry| announcement_in(&entry.data, did, now_ms, network_id)))
    }
}

#[cfg(test)]
mod test_directory;
