//! Seed and SeedLoader use for getting peers from endpoint.
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
