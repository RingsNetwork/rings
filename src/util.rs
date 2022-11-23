//! Utilities for configuration and build.
#![warn(missing_docs)]

use std::env;

use crate::prelude::RTCIceConnectionState;

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

/// load_config env file from path if available
pub fn load_config() {
    let mut v = env::args();
    while let Some(item) = v.next() {
        if item.eq("-c") || item.eq("--config_file") {
            let config = v.next();
            if let Some(c) = config {
                dotenv::from_path(c).ok();
            }
            break;
        }
    }
}

/// convert RTCIceConnectionState to string
pub(crate) fn from_rtc_ice_connection_state(state: RTCIceConnectionState) -> String {
    match state {
        RTCIceConnectionState::New => "new",
        RTCIceConnectionState::Checking => "checking",
        RTCIceConnectionState::Connected => "connected",
        RTCIceConnectionState::Completed => "completed",
        RTCIceConnectionState::Failed => "failed",
        RTCIceConnectionState::Disconnected => "disconnected",
        RTCIceConnectionState::Closed => "closed",
        _ => "unknown",
    }
    .to_owned()
}

/// convert string to RTCIceConnectionState
#[allow(dead_code)]
pub(crate) fn into_rtc_ice_connection_state(value: &str) -> Option<RTCIceConnectionState> {
    Some(match value {
        "new" => RTCIceConnectionState::New,
        "checking" => RTCIceConnectionState::Checking,
        "connected" => RTCIceConnectionState::Connected,
        "completed" => RTCIceConnectionState::Completed,
        "failed" => RTCIceConnectionState::Failed,
        "disconnected" => RTCIceConnectionState::Disconnected,
        "closed" => RTCIceConnectionState::Closed,
        _ => return None,
    })
}

#[cfg(feature = "node")]
pub mod loader {
    //! A module to help user load config from local file or remote url.

    use async_trait::async_trait;
    use reqwest::Url;
    use serde::de::DeserializeOwned;

    use crate::backend::BackendConfig;
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
    impl ResourceLoader for BackendConfig {}
}
