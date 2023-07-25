use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::backend::extension::ExtensionConfig;
use crate::backend::service::http_server::HiddenServerConfig;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::DelegatedSk;
use crate::processor::ProcessorConfig;

lazy_static::lazy_static! {
  static ref DEFAULT_DATA_STORAGE_CONFIG: StorageConfig = StorageConfig {
    path: get_storage_location(".rings", "data"),
    capacity: DEFAULT_STORAGE_CAPACITY,
  };
  static ref DEFAULT_MEASURE_STORAGE_CONFIG: StorageConfig = StorageConfig {
    path: get_storage_location(".rings", "measure"),
    capacity: DEFAULT_STORAGE_CAPACITY,
  };
}

pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1:50000";
pub const DEFAULT_ENDPOINT_URL: &str = "http://127.0.0.1:50000";
pub const DEFAULT_ICE_SERVERS: &str = "stun://stun.l.google.com:19302";
pub const DEFAULT_STABILIZE_TIMEOUT: usize = 3;
pub const DEFAULT_STORAGE_CAPACITY: usize = 200000000;

pub fn get_storage_location<P>(prefix: P, path: P) -> String
where P: AsRef<std::path::Path> {
    let home_dir = env::var_os("HOME").map(PathBuf::from);
    let expect = match home_dir {
        Some(dir) => dir.join(prefix).join(path),
        None => std::path::Path::new("data").join(prefix).join(path),
    };
    expect.to_str().unwrap().to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub delegated_sk: String,
    #[serde(rename = "bind")]
    pub http_addr: String,
    pub endpoint_url: String,
    pub ice_servers: String,
    pub stabilize_timeout: usize,
    pub external_ip: Option<String>,
    /// When there is no configuration in the YAML file,
    /// its deserialization is equivalent to `vec![]` in Rust.
    #[serde(default)]
    pub backend: Vec<HiddenServerConfig>,
    pub data_storage: StorageConfig,
    pub measure_storage: StorageConfig,
    /// When there is no configuration in the YAML file,
    /// its deserialization is equivalent to `ExtensionConfig(vec![])` in Rust.
    #[serde(default)]
    pub extension: ExtensionConfig,
}

impl From<&Config> for ProcessorConfig {
    fn from(config: &Config) -> Self {
        Self {
            ice_servers: config.ice_servers.clone(),
            external_address: config.external_ip.clone(),
            delegated_sk: config.delegated_sk.clone(),
            stabilize_timeout: config.stabilize_timeout,
        }
    }
}

impl Config {
    pub fn new_with_key(key: SecretKey) -> Self {
        let delegated_sk = DelegatedSk::new_with_seckey(&key)
            .expect("create delegated sk failed")
            .dump()
            .expect("dump delegated sk failed");

        Self {
            delegated_sk,
            http_addr: DEFAULT_BIND_ADDRESS.to_string(),
            endpoint_url: DEFAULT_ENDPOINT_URL.to_string(),
            ice_servers: DEFAULT_ICE_SERVERS.to_string(),
            stabilize_timeout: DEFAULT_STABILIZE_TIMEOUT,
            external_ip: None,
            backend: vec![],
            data_storage: DEFAULT_DATA_STORAGE_CONFIG.clone(),
            measure_storage: DEFAULT_MEASURE_STORAGE_CONFIG.clone(),
            extension: ExtensionConfig::default(),
        }
    }

    pub fn write_fs<P>(&self, path: P) -> Result<String>
    where P: AsRef<std::path::Path> {
        let path = match path.as_ref().strip_prefix("~") {
            Ok(stripped) => {
                let home_dir = env::var_os("HOME").map(PathBuf::from);
                home_dir.map(|mut p| {
                    p.push(stripped);
                    p
                })
            }
            Err(_) => Some(path.as_ref().to_owned()),
        }
        .unwrap();
        let parent = path.parent().expect("no parent directory");
        if !parent.is_dir() {
            fs::create_dir_all(parent).map_err(|e| Error::CreateFileError(e.to_string()))?;
        };
        let f =
            fs::File::create(path.as_path()).map_err(|e| Error::CreateFileError(e.to_string()))?;
        let f_writer = io::BufWriter::new(f);
        serde_yaml::to_writer(f_writer, self).map_err(|_| Error::EncodeError)?;
        Ok(path.to_str().unwrap().to_owned())
    }

    pub fn read_fs<P>(path: P) -> Result<Config>
    where P: AsRef<std::path::Path> {
        let path = match path.as_ref().strip_prefix("~") {
            Ok(stripped) => {
                let home_dir = env::var_os("HOME").map(PathBuf::from);
                home_dir.map(|mut p| {
                    p.push(stripped);
                    p
                })
            }
            Err(_) => Some(path.as_ref().to_owned()),
        }
        .unwrap();
        tracing::debug!("Read config from: {:?}", path);
        let f = fs::File::open(path).map_err(|e| Error::OpenFileError(e.to_string()))?;
        let f_rdr = io::BufReader::new(f);
        serde_yaml::from_reader(f_rdr).map_err(|_| Error::EncodeError)
    }
}

impl Default for Config {
    fn default() -> Self {
        let ecdsa_key = SecretKey::random();
        Self::new_with_key(ecdsa_key)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    pub path: String,
    pub capacity: usize,
}

impl StorageConfig {
    pub fn new(path: &str, capacity: usize) -> Self {
        Self {
            path: path.to_string(),
            capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialization_with_missed_field() {
        let yaml = r#"
delegated_sk: delegated_sk
bind: 127.0.0.1:50000
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_timeout: 3
external_ip: null
data_storage:
  path: /Users/foo/.rings/data
  capacity: 200000000
measure_storage:
  path: /Users/foo/.rings/measure
  capacity: 200000000
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.extension, ExtensionConfig::default());
        assert_eq!(cfg.backend, vec![]);
    }
}
