use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

use crate::prelude::rings_core::dht::Did;

#[derive(Deserialize, Serialize, Debug)]
pub struct Seed {
    pub peers: Vec<SeedPeer>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SeedPeer {
    pub did: Did,
    pub endpoint: String,
}

#[cfg_attr(feature = "node", async_trait)]
#[cfg_attr(not(feature = "node"), async_trait(?Send))]
pub trait SourceLoader {
    async fn load(source: &str) -> anyhow::Result<Self>
    where Self: Sized;
}

#[cfg(feature = "node")]
mod loader {
    use super::*;
    use crate::util::loader::ResourceLoader;

    impl ResourceLoader for Seed {}
}
