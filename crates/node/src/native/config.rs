use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeType;
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::SessionSk;
use crate::processor::ProcessorConfig;
use crate::processor::ProcessorConfigSerialized;
use crate::util::ensure_parent_dir;
use crate::util::expand_home;

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

pub const DEFAULT_NETWORK_ID: u32 = 1;
pub const DEFAULT_INTERNAL_API_PORT: u16 = 50000;
pub const DEFAULT_EXTERNAL_API_ADDR: &str = "127.0.0.1:50001";
pub const DEFAULT_ENDPOINT_URL: &str = "http://127.0.0.1:50000";
pub const DEFAULT_ICE_SERVERS: &str = "stun://stun.l.google.com:19302";
pub const DEFAULT_STABILIZE_INTERVAL: u64 = 3;
pub const DEFAULT_STORAGE_CAPACITY: u32 = 200000000;

pub fn get_storage_location<P>(prefix: P, path: P) -> String
where P: AsRef<std::path::Path> {
    let home_dir = env::var_os("HOME").map(PathBuf::from);
    let storage_path = match home_dir {
        Some(dir) => dir.join(prefix).join(path),
        None => std::path::Path::new("data").join(prefix).join(path),
    };
    storage_path.to_string_lossy().to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub network_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ecdsa_key: Option<SecretKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_manager: Option<String>,
    pub session_sk: Option<String>,
    pub internal_api_port: u16,
    pub external_api_addr: String,
    pub endpoint_url: String,
    pub ice_servers: String,
    pub stabilize_interval: u64,
    #[serde(default = "crate::registration::default_online_node_heartbeat_interval_secs")]
    pub online_node_heartbeat_interval_secs: u64,
    #[serde(default = "crate::registration::default_online_node_ttl_secs")]
    pub online_node_ttl_secs: u64,
    #[serde(default = "crate::registration::default_online_node_type")]
    pub online_node_type: OnlineNodeType,
    #[serde(default = "crate::registration::default_advertise_presence")]
    pub advertise_presence: bool,
    pub dht_virtual_nodes: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webrtc_udp_port_min: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webrtc_udp_port_max: Option<u16>,
    pub data_storage: StorageConfig,
    pub measure_storage: StorageConfig,
}

impl TryFrom<Config> for ProcessorConfigSerialized {
    type Error = Error;
    fn try_from(config: Config) -> Result<Self> {
        // Support old version
        let session_sk: String = if let Some(sk) = config.ecdsa_key {
            tracing::warn!("Field `ecdsa_key` is deprecated, use `session_sk` instead.");
            SessionSk::new_with_seckey(&sk)
                .and_then(|session_sk| session_sk.dump())
                .map_err(|e| Error::VerifyError(e.to_string()))?
        } else if let Some(ssk) = config.session_manager {
            tracing::warn!("Field `session_manager` is deprecated, use `session_sk` instead.");
            ssk
        } else {
            let Some(ssk_file) = config.session_sk else {
                return Err(Error::InvalidData);
            };
            let ssk_file_expand_home = expand_home(&ssk_file)?;
            fs::read_to_string(ssk_file_expand_home).unwrap_or_else(|e| {
                tracing::warn!("Read session_sk file failed: {e:?}. Handling it as raw session_sk string. This mode is deprecated. please use a file path.");
                ssk_file
            })
        };

        let mut cs = Self::new(
            config.network_id,
            config.ice_servers,
            session_sk,
            config.stabilize_interval,
        )
        .online_node_heartbeat_interval_secs(config.online_node_heartbeat_interval_secs)
        .online_node_ttl_secs(config.online_node_ttl_secs)
        .online_node_type(config.online_node_type)
        .advertise_presence(config.advertise_presence)
        .dht_virtual_nodes(config.dht_virtual_nodes);

        cs = if let Some(ext_ip) = config.external_ip {
            cs.external_address(ext_ip)
        } else {
            cs
        };
        let udp_range = crate::processor::parse_webrtc_udp_port_range(
            config.webrtc_udp_port_min,
            config.webrtc_udp_port_max,
        )?;
        cs = if let Some(range) = udp_range {
            cs.webrtc_udp_port_range(range)
        } else {
            cs
        };

        Ok(cs)
    }
}

impl TryFrom<Config> for ProcessorConfig {
    type Error = Error;
    fn try_from(config: Config) -> Result<Self> {
        ProcessorConfigSerialized::try_from(config).and_then(Self::try_from)
    }
}

impl Config {
    pub fn new<P>(session_sk: P) -> Self
    where P: AsRef<std::path::Path> {
        let session_sk = session_sk.as_ref().to_string_lossy().to_string();
        Self {
            network_id: DEFAULT_NETWORK_ID,
            ecdsa_key: None,
            session_manager: None,
            session_sk: Some(session_sk),
            internal_api_port: DEFAULT_INTERNAL_API_PORT,
            external_api_addr: DEFAULT_EXTERNAL_API_ADDR.to_string(),
            endpoint_url: DEFAULT_ENDPOINT_URL.to_string(),
            ice_servers: DEFAULT_ICE_SERVERS.to_string(),
            stabilize_interval: DEFAULT_STABILIZE_INTERVAL,
            online_node_heartbeat_interval_secs:
                crate::registration::default_online_node_heartbeat_interval_secs(),
            online_node_ttl_secs: crate::registration::default_online_node_ttl_secs(),
            online_node_type: crate::registration::default_online_node_type(),
            advertise_presence: crate::registration::default_advertise_presence(),
            dht_virtual_nodes: 0,
            external_ip: None,
            webrtc_udp_port_min: None,
            webrtc_udp_port_max: None,
            data_storage: DEFAULT_DATA_STORAGE_CONFIG.clone(),
            measure_storage: DEFAULT_MEASURE_STORAGE_CONFIG.clone(),
        }
    }

    pub fn write_fs<P>(&self, path: P) -> Result<String>
    where P: AsRef<std::path::Path> {
        let path = expand_home(path)?;
        ensure_parent_dir(&path)?;
        let f =
            fs::File::create(path.as_path()).map_err(|e| Error::CreateFileError(e.to_string()))?;
        let f_writer = io::BufWriter::new(f);
        serde_yaml::to_writer(f_writer, self).map_err(|_| Error::EncodeError)?;
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::PathUtf8Error(path.display().to_string()))
    }

    pub fn read_fs<P>(path: P) -> Result<Config>
    where P: AsRef<std::path::Path> {
        let path = expand_home(path)?;
        tracing::debug!("Read config from: {:?}", path);
        let f = fs::File::open(path).map_err(|e| Error::OpenFileError(e.to_string()))?;
        let f_rdr = io::BufReader::new(f);
        serde_yaml::from_reader(f_rdr).map_err(|_| Error::EncodeError)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    pub path: String,
    pub capacity: u32,
}

impl StorageConfig {
    pub fn new(path: &str, capacity: u32) -> Self {
        Self {
            path: path.to_string(),
            capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dumped_session_sk() -> String {
        let key = SecretKey::random();
        let session = match SessionSk::new_with_seckey(&key) {
            Ok(session) => session,
            Err(error) => panic!("session key construction failed: {error}"),
        };
        match session.dump() {
            Ok(dump) => dump,
            Err(error) => panic!("session key dump failed: {error}"),
        }
    }

    #[test]
    fn deserialization_defaults_online_registration_fields() {
        let yaml = r#"
network_id: 1
session_sk: session_sk
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 3
dht_virtual_nodes: 0
external_ip: null
webrtc_udp_port_min: null
webrtc_udp_port_max: null
data_storage:
  path: /Users/foo/.rings/data
  capacity: 200000000
measure_storage:
  path: /Users/foo/.rings/measure
  capacity: 200000000
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.network_id, 1);
        assert!(cfg.advertise_presence);
    }

    #[test]
    fn deserialization_requires_dht_virtual_nodes() {
        let yaml = r#"
network_id: 1
session_sk: session_sk
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 3
external_ip: null
webrtc_udp_port_min: null
webrtc_udp_port_max: null
data_storage:
  path: /Users/foo/.rings/data
  capacity: 200000000
measure_storage:
  path: /Users/foo/.rings/measure
  capacity: 200000000
"#;

        let result = serde_yaml::from_str::<Config>(yaml);

        assert!(matches!(
            result,
            Err(error) if error.to_string().contains("dht_virtual_nodes")
        ));
    }

    #[test]
    fn config_with_valid_webrtc_udp_range_builds_processor_config() {
        let mut config = Config::new(dumped_session_sk());
        config.webrtc_udp_port_min = Some(49160);
        config.webrtc_udp_port_max = Some(49200);

        let processor_config = ProcessorConfig::try_from(config);

        assert!(matches!(
            processor_config.and_then(|config| config.webrtc_udp_port_range()),
            Ok(Some(range)) if range.min() == 49160 && range.max() == 49200
        ));
    }

    #[test]
    fn config_with_partial_webrtc_udp_range_is_rejected() {
        let mut config = Config::new(dumped_session_sk());
        config.webrtc_udp_port_min = Some(49160);

        let processor_config = ProcessorConfig::try_from(config);

        assert!(matches!(
            processor_config,
            Err(Error::IncompleteWebrtcUdpPortRange {
                min: Some(49160),
                max: None
            })
        ));
    }
}
