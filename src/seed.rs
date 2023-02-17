//! Seed and SeedLoader use for getting peers from endpoint.
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

use crate::prelude::rings_core::dht::Did;

/// A list contains SeedPeer.
#[derive(Deserialize, Serialize, Debug)]
pub struct Seed {
    pub peers: Vec<SeedPeer>,
}

/// SeedPeer contain `Did` and `endpoint`.
#[derive(Deserialize, Serialize, Debug)]
pub struct SeedPeer {
    /// an unique identify.
    pub did: Did,
    /// remote client endpoint
    pub endpoint: String,
}

/// implement load method.
#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(feature = "browser", async_trait(?Send))]
pub trait SourceLoader {
    async fn load(source: &str) -> anyhow::Result<Self>
    where
        Self: Sized;
}
