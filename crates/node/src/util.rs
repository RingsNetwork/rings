//! Utilities for configuration and build.
#![warn(missing_docs)]

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

#[cfg(feature = "node")]
use crate::error::Error;

pub(crate) fn serialize_forward<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    value.serialize(serializer)
}

pub(crate) fn deserialize_forward<'de, T, D>(deserializer: D) -> Result<T, D::Error>
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
