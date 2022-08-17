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

#[cfg_attr(feature = "default", async_trait)]
#[cfg_attr(not(feature = "default"), async_trait(?Send))]
pub trait SeedLoader {
    async fn load(source: &str) -> anyhow::Result<Self>
    where Self: Sized;
}

#[cfg(feature = "default")]
mod loader {
    use reqwest::Url;

    use super::*;

    #[async_trait]
    impl SeedLoader for Seed {
        async fn load(source: &str) -> anyhow::Result<Self> {
            let url = Url::parse(source).map_err(|e| anyhow::anyhow!("{}", e))?;

            if let Ok(path) = url.to_file_path() {
                let data = std::fs::read_to_string(path)
                    .map_err(|_| anyhow::anyhow!("Unable to read seed file"))?;

                serde_json::from_str(&data).map_err(|e| anyhow::anyhow!("{}", e))
            } else {
                let resp = reqwest::get(source)
                    .await
                    .map_err(|_| anyhow::anyhow!("failed to get seed from {}", source))?;
                resp.json()
                    .await
                    .map_err(|_| anyhow::anyhow!("failed to load seed from {}", source))
            }
        }
    }
}
