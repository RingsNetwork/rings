//! Utilities for configuration and build.

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

#[cfg(feature = "node")]
use crate::error::Error;

#[allow(dead_code)]
/// Serialize a value through Serde without an intermediate wrapper.
pub fn serialize_forward<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    value.serialize(serializer)
}

#[allow(dead_code)]
/// Deserialize a value through Serde without an intermediate wrapper.
pub fn deserialize_forward<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    T::deserialize(deserializer)
}

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

/// Expand path with "~" to absolute path.
#[cfg(feature = "node")]
pub fn expand_home<P>(path: P) -> Result<std::path::PathBuf, Error>
where P: AsRef<std::path::Path> {
    let Ok(stripped) = path.as_ref().strip_prefix("~") else {
        return Ok(path.as_ref().to_path_buf());
    };

    let Some(mut p) = home::home_dir() else {
        return Err(Error::HomeDirError);
    };

    p.push(stripped);

    Ok(p)
}

/// Create parent directory of a path if not exists.
#[cfg(feature = "node")]
pub fn ensure_parent_dir<P>(path: P) -> Result<(), Error>
where P: AsRef<std::path::Path> {
    let path = expand_home(path)?;
    let parent = path.parent().ok_or(Error::ParentDirError)?;
    if !parent.is_dir() {
        std::fs::create_dir_all(parent).map_err(|e| Error::CreateFileError(e.to_string()))?;
    };
    Ok(())
}

#[cfg(feature = "node")]
pub mod loader {
    //! A module to help user load config from local file or remote url.

    use std::io::Read;
    use std::time::Duration;

    use async_trait::async_trait;
    use reqwest::Url;
    use serde::de::DeserializeOwned;

    use crate::seed::Seed;

    const MAX_RESOURCE_BYTES: usize = 4 * 1_024 * 1_024;
    const RESOURCE_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

    /// Load config from local file or remote url.
    /// To use this trait, derive DeserializeOwned then implement this trait.
    #[async_trait]
    pub trait ResourceLoader {
        /// Load config from local file or remote url.
        async fn load(source: &str) -> anyhow::Result<Self>
        where Self: Sized + DeserializeOwned {
            let url = Url::parse(source).map_err(|e| anyhow::anyhow!("{}", e))?;

            if url.scheme() == "file" {
                let path = url
                    .to_file_path()
                    .map_err(|_| anyhow::anyhow!("Invalid file resource path"))?;
                let file = std::fs::File::open(path)
                    .map_err(|_| anyhow::anyhow!("Unable to read resource file"))?;
                let mut bytes = Vec::new();
                file.take((MAX_RESOURCE_BYTES as u64).saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(|_| anyhow::anyhow!("Unable to read resource file"))?;
                if bytes.len() > MAX_RESOURCE_BYTES {
                    anyhow::bail!("resource file exceeds maximum {MAX_RESOURCE_BYTES} bytes");
                }
                return serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("{}", e));
            }

            if !matches!(url.scheme(), "http" | "https") {
                anyhow::bail!("unsupported resource scheme {}", url.scheme());
            }
            if !url.username().is_empty() || url.password().is_some() {
                anyhow::bail!("resource URL must not contain userinfo credentials");
            }
            rings_network_policy::validate_public_url_host(&url)
                .map_err(|e| anyhow::anyhow!("resource URL blocked by network policy: {e}"))?;
            let host = url
                .host_str()
                .ok_or_else(|| anyhow::anyhow!("resource URL missing host"))?;
            let port = url
                .port_or_known_default()
                .ok_or_else(|| anyhow::anyhow!("resource URL missing port"))?;
            let resolved: Vec<std::net::SocketAddr> = tokio::time::timeout(
                RESOURCE_FETCH_TIMEOUT,
                tokio::net::lookup_host((host, port)),
            )
            .await
            .map_err(|_| anyhow::anyhow!("timed out resolving resource host"))?
            .map_err(|e| anyhow::anyhow!("failed to resolve resource host: {e}"))?
            .collect();
            if resolved.is_empty()
                || resolved
                    .iter()
                    .any(|addr| !rings_network_policy::is_public_ip(addr.ip()))
            {
                anyhow::bail!("resource URL resolved to a non-public IP address");
            }

            let mut builder = reqwest::Client::builder()
                .timeout(RESOURCE_FETCH_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy();
            if matches!(url.host(), Some(url::Host::Domain(_))) {
                builder = builder.resolve_to_addrs(host, &resolved);
            }
            let client = builder
                .build()
                .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))?;
            let mut resp = client
                .get(url)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("failed to get resource from {}", source))?
                .error_for_status()
                .map_err(|_| anyhow::anyhow!("failed to get resource from {}", source))?;
            if let Some(len) = resp.content_length() {
                if len > MAX_RESOURCE_BYTES as u64 {
                    anyhow::bail!(
                        "remote resource length {len} exceeds maximum {MAX_RESOURCE_BYTES} bytes"
                    );
                }
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|_| anyhow::anyhow!("failed to load resource from {}", source))?
            {
                if body.len().saturating_add(chunk.len()) > MAX_RESOURCE_BYTES {
                    anyhow::bail!("remote resource exceeds maximum {MAX_RESOURCE_BYTES} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&body)
                .map_err(|_| anyhow::anyhow!("failed to load resource from {}", source))
        }
    }

    impl ResourceLoader for Seed {}
}

#[cfg(test)]
#[cfg(feature = "node")]
mod tests {
    use super::*;

    #[test]
    fn test_expand_home_with_tilde() {
        let input = "~";
        let mut expected = std::env::var("HOME").unwrap();
        expected.push('/');
        let result = expand_home(input).unwrap();
        assert_eq!(result.to_str(), Some(expected.as_str()));
    }

    #[test]
    fn test_expand_home_with_relative_path() {
        let input = "~/path/to/file.txt";
        let mut expected = std::env::var("HOME").unwrap();
        expected.push_str("/path/to/file.txt");
        let result = expand_home(input).unwrap();
        assert_eq!(result.to_str(), Some(expected.as_str()));
    }

    #[test]
    fn test_expand_home_with_absolute_path() {
        let input = "/absolute/path/to/file.txt";
        let expected = std::path::PathBuf::from(input);
        let result = expand_home(input).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_expand_home_with_invalid_path() {
        let input = "path/does/not/exist.txt";
        let expected = std::path::PathBuf::from(input);
        let result = expand_home(input).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_expand_home_with_empty_path() {
        let input = "";
        let expected = std::path::PathBuf::from("");
        let result = expand_home(input).unwrap();
        assert_eq!(result, expected);
    }
}
