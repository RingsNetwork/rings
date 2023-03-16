//! Utilities for configuration and build.
#![warn(missing_docs)]

/// build_version of program
pub fn build_version() -> String {
    let mut infos = vec![];
    if let Some(version) = option_env!("CARGO_PKG_VERSION") {
        infos.push(version);
    };
    if let Some(git_hash) = option_env!("GIT_SHORT_HASH") {
        infos.push(git_hash);
    }
    infos.join("-")
}

#[cfg(feature = "node")]
pub mod loader {
    //! A module to help user load config from local file or remote url.

    use async_trait::async_trait;
    use reqwest::Url;
    use serde::de::DeserializeOwned;

    use crate::seed::Seed;

    /// Load config from local file or remote url.
    /// To use this trait, derive DeserializeOwned then implement this trait.
    #[async_trait]
    pub trait ResourceLoader {
        /// Load config from local file or remote url.
        async fn load(source: &str) -> anyhow::Result<Self>
        where Self: Sized + DeserializeOwned {
            let url = Url::parse(source).map_err(|e| anyhow::anyhow!("{}", e))?;

            if let Ok(path) = url.to_file_path() {
                let data = std::fs::read_to_string(path)
                    .map_err(|_| anyhow::anyhow!("Unable to read resource file"))?;

                serde_json::from_str(&data).map_err(|e| anyhow::anyhow!("{}", e))
            } else {
                let resp = reqwest::get(source)
                    .await
                    .map_err(|_| anyhow::anyhow!("failed to get resource from {}", source))?;
                resp.json()
                    .await
                    .map_err(|_| anyhow::anyhow!("failed to load resource from {}", source))
            }
        }
    }

    impl ResourceLoader for Seed {}
}
