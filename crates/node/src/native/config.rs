use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use rings_gateway::GatewayConfig;
use rings_gateway::GatewayPlan;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
use crate::prelude::rings_core::dht::default_storage_virtual_positions_per_owner;
use crate::prelude::rings_core::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
#[cfg(test)]
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::message::OriginQuotaConfig;
#[cfg(test)]
use crate::prelude::DelegateeKey;
use crate::processor::ProcessorConfig;
use crate::processor::ProcessorConfigSerialized;
use crate::seed::SeedPeer;
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

/// Default Rings network identifier for native nodes.
pub const DEFAULT_NETWORK_ID: u32 = 1;
/// Default internal JSON-RPC API port.
pub const DEFAULT_INTERNAL_API_PORT: u16 = 50000;
/// Default external JSON-RPC listener address.
pub const DEFAULT_EXTERNAL_API_ADDR: &str = "127.0.0.1:50001";
/// Default internal endpoint URL used by CLI clients.
pub const DEFAULT_ENDPOINT_URL: &str = "http://127.0.0.1:50000";
/// Default WebRTC ICE server list.
pub const DEFAULT_ICE_SERVERS: &str = "stun://stun.l.google.com:19302";
/// Default Chord stabilization interval in seconds.
pub const DEFAULT_STABILIZE_INTERVAL: u64 = 15;
/// Default storage capacity in bytes for native storage backends.
pub const DEFAULT_STORAGE_CAPACITY: u32 = 200000000;
/// Default interval for refreshing gateway status.
pub const DEFAULT_GATEWAY_STATUS_REFRESH_SECS: u64 = 2;

/// Native foreground-gateway configuration: the `gateway:` section of the node config file.
///
/// `rings init` writes this section in full through [`NativeGatewayConfig::disabled_default`],
/// so the generated file is the one place an operator edits and `rings run --gateway` works on
/// it unchanged. Presence of the section is not consent to start a TUN device:
///
/// ```text
/// gateway starts ⟺ section present ∧ (enabled = true ∨ --gateway)
/// ```
///
/// A section that omits `enabled` is therefore inert, and a config without the section loads
/// with `gateway = None`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NativeGatewayConfig {
    /// Whether plain `rings run` starts the gateway. Absent means `false`; only an explicit
    /// `enabled: true` or `rings run --gateway` starts a TUN device.
    #[serde(default)]
    pub enabled: bool,
    /// Platform-neutral routing, TCP, and flow limits.
    #[serde(flatten)]
    pub runtime: GatewayConfig,
    /// Requested Wintun interface name; on Unix the helper's `--interface` is authoritative.
    #[serde(default)]
    pub interface_name: Option<String>,
    /// Durable journal used directly on Windows; on Unix the helper's `--ledger` is authoritative.
    #[serde(default = "default_gateway_route_ledger_path")]
    pub route_ledger_path: String,
    /// Foreground `gateway-config-unix` control socket on Linux and macOS.
    #[serde(default = "default_gateway_unix_helper_socket")]
    pub unix_helper_socket: String,
    /// Optional explicit Wintun DLL path on Windows; overrides `RINGS_GATEWAY_WINTUN_DLL`.
    #[serde(default)]
    pub wintun_dll_path: Option<String>,
    /// Interval for refreshing Onion exit availability in gateway status.
    #[serde(default = "default_gateway_status_refresh_secs")]
    pub status_refresh_secs: u64,
    /// Onion TCP exit service selected for captured flows.
    #[serde(default = "OnionServiceName::tcp")]
    pub onion_service: OnionServiceName,
}

impl NativeGatewayConfig {
    /// The section `rings init` writes: the gateway crate's interface-only plan under its default
    /// limits, the node's default paths, and `enabled: false`, with every field stated so the
    /// generated file documents the whole surface. Optional fields are written as `null`.
    pub fn disabled_default() -> Self {
        Self {
            enabled: false,
            runtime: GatewayConfig::with_default_limits(GatewayPlan::interface_only()),
            interface_name: None,
            route_ledger_path: default_gateway_route_ledger_path(),
            unix_helper_socket: default_gateway_unix_helper_socket(),
            wintun_dll_path: None,
            status_refresh_secs: default_gateway_status_refresh_secs(),
            onion_service: OnionServiceName::tcp(),
        }
    }

    /// Render this section as the top-level `gateway:` mapping of a config file, so a message
    /// telling an operator what to add quotes the shape `rings init` writes rather than a copy.
    pub fn to_yaml_section(&self) -> Result<String> {
        serde_yaml::to_string(&GatewaySection { gateway: self }).map_err(|_| Error::EncodeError)
    }
}

/// A config document consisting of the `gateway:` section alone.
#[derive(Serialize)]
struct GatewaySection<'a> {
    gateway: &'a NativeGatewayConfig,
}

/// Keys of the `gateway:` section: those of [`NativeGatewayConfig`] and of the flattened
/// [`GatewayConfig`], in the order `rings init` writes them.
const GATEWAY_KEYS: [&str; 11] = [
    "enabled",
    "plan",
    "max_flows",
    "flow_idle_timeout",
    "tcp_buffer_bytes",
    "interface_name",
    "route_ledger_path",
    "unix_helper_socket",
    "wintun_dll_path",
    "status_refresh_secs",
    "onion_service",
];

/// Keys of the `gateway:` section removed by the onion loop cutover (#834 D5), rejected with a
/// pointer to the loop shape.
const REMOVED_GATEWAY_KEYS: [&str; 2] = ["onion_hop_count", "onion_allow_short_paths"];

/// Deserialize the `gateway:` section, rejecting every key outside [`GATEWAY_KEYS`].
///
/// The section flattens [`GatewayConfig`], so serde cannot deny unknown keys there; without this
/// a misspelt key (say `onion_services`) would silently fall back to its default.
fn deserialize_gateway_section<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<NativeGatewayConfig>, D::Error>
where D: serde::Deserializer<'de> {
    let Some(section) = Option::<serde_yaml::Mapping>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if let Some(key) = section
        .keys()
        .map(|key| key.as_str().unwrap_or_default())
        .find(|key| !GATEWAY_KEYS.contains(key))
    {
        return Err(serde::de::Error::custom(
            if REMOVED_GATEWAY_KEYS.contains(&key) {
                format!(
                    "gateway.{key} was removed: the onion route length is fixed by the loop shape \
                     (#834 D5)"
                )
            } else {
                format!(
                    "unknown gateway key {key:?}; expected one of {}",
                    GATEWAY_KEYS.join(", ")
                )
            },
        ));
    }
    serde_yaml::from_value(serde_yaml::Value::Mapping(section))
        .map(Some)
        .map_err(serde::de::Error::custom)
}

const fn default_gateway_status_refresh_secs() -> u64 {
    DEFAULT_GATEWAY_STATUS_REFRESH_SECS
}

fn default_gateway_route_ledger_path() -> String {
    get_storage_location(".rings", "gateway-routes.json")
}

fn default_gateway_unix_helper_socket() -> String {
    get_storage_location(".rings", "gateway-helper.sock")
}

/// Builds the default storage path under the user home directory.
pub fn get_storage_location<P>(prefix: P, path: P) -> String
where P: AsRef<std::path::Path> {
    let home_dir = env::var_os("HOME").map(PathBuf::from);
    let storage_path = match home_dir {
        Some(dir) => dir.join(prefix).join(path),
        None => std::path::Path::new("data").join(prefix).join(path),
    };
    storage_path.to_string_lossy().to_string()
}

/// The `bootstrap` section: targets `rings run` keeps reachable for the life of the process.
/// `rings init` writes it empty; see [`crate::native::bootstrap`].
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct BootstrapConfig {
    /// Managed targets, in the same shape as the entries of a seed document.
    #[serde(default)]
    pub peers: Vec<SeedPeer>,
}

/// Serializable native-node configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Rings network identifier this node joins.
    pub network_id: u32,
    /// Delegation secret key file path.
    pub delegatee_key: String,
    /// Internal JSON-RPC API port.
    pub internal_api_port: u16,
    /// External JSON-RPC listener address.
    pub external_api_addr: String,
    /// Internal endpoint URL used by local clients.
    pub endpoint_url: String,
    /// Optional API token file path; relative paths are resolved next to this config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token_path: Option<String>,
    /// Exact browser origins permitted to call the authenticated API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_allowed_origins: Vec<String>,
    /// Explicitly allow the external API listener to bind a non-loopback address.
    #[serde(default)]
    pub allow_remote_external_api: bool,
    /// WebRTC ICE server list, independent from optional gateway ingress.
    pub ice_servers: String,
    /// Chord stabilization interval in seconds.
    pub stabilize_interval: u64,
    /// Presence descriptor heartbeat interval in seconds.
    #[serde(default = "crate::registration::default_online_node_heartbeat_interval_secs")]
    pub online_node_heartbeat_interval_secs: u64,
    /// Presence descriptor time-to-live in seconds.
    #[serde(default = "crate::registration::default_online_node_ttl_secs")]
    pub online_node_ttl_secs: u64,
    /// Node type advertised in online-node descriptors.
    #[serde(default = "crate::registration::default_online_node_type")]
    pub online_node_type: OnlineNodeType,
    /// Whether this node publishes online-node descriptors.
    #[serde(default = "crate::registration::default_advertise_presence")]
    pub advertise_presence: bool,
    /// Whether this node advertises onion relay capability.
    #[serde(default = "crate::onion::default_advertise_onion_relay")]
    pub advertise_onion_relay: bool,
    /// Whether this node advertises onion exit capability.
    #[serde(default = "crate::onion::default_advertise_onion_exit")]
    pub advertise_onion_exit: bool,
    /// Onion-exit descriptor heartbeat interval in seconds.
    #[serde(default = "crate::onion::default_onion_exit_heartbeat_interval_secs")]
    pub onion_exit_heartbeat_interval_secs: u64,
    /// Onion-exit descriptor time-to-live in seconds.
    #[serde(default = "crate::onion::default_onion_exit_ttl_secs")]
    pub onion_exit_ttl_secs: u64,
    /// Onion-exit services this node can publish.
    #[serde(default = "crate::onion::default_onion_exit_services")]
    pub onion_exit_services: Vec<OnionServiceName>,
    /// Onion-exit target and resource policy.
    #[serde(default = "crate::onion::default_onion_exit_policy")]
    pub onion_exit_policy: OnionExitPolicy,
    /// Optional local HTTP CONNECT proxy listener address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onion_http_proxy_addr: Option<String>,
    /// Onion service name used by the HTTP CONNECT proxy.
    #[serde(default = "OnionServiceName::tcp")]
    pub onion_http_proxy_service: OnionServiceName,
    /// Timeout for reading HTTP CONNECT headers in seconds.
    #[serde(default = "crate::onion::proxy::http::default_connect_header_timeout_secs")]
    pub onion_http_proxy_header_timeout_secs: u64,
    /// Maximum simultaneous HTTP CONNECT proxy connections.
    #[serde(default = "crate::onion::proxy::http::default_max_connect_connections")]
    pub onion_http_proxy_max_connections: usize,
    /// Native TUN gateway section; `rings init` writes it disabled, and it starts a gateway in
    /// the same foreground lifecycle only under `enabled: true` or `--gateway`. Older configs
    /// without the section load as `None`.
    #[serde(
        default,
        deserialize_with = "deserialize_gateway_section",
        skip_serializing_if = "Option::is_none"
    )]
    pub gateway: Option<NativeGatewayConfig>,
    /// Managed bootstrap targets `rings run` keeps reachable for the life of the process.
    #[serde(default)]
    pub bootstrap: BootstrapConfig,
    /// Virtual DHT positions per storage owner.
    #[serde(default = "default_storage_virtual_positions_per_owner")]
    pub dht_virtual_nodes: u16,
    /// Runtime-local final-destination quotas keyed by verified origin and logical lane.
    #[serde(default)]
    pub origin_quota: OriginQuotaConfig,
    /// Optional externally reachable IP address hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_ip: Option<String>,
    /// Optional lower bound for WebRTC UDP port allocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webrtc_udp_port_min: Option<u16>,
    /// Optional upper bound for WebRTC UDP port allocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webrtc_udp_port_max: Option<u16>,
    /// Persistent DHT data storage configuration.
    pub data_storage: StorageConfig,
    /// Peer measurement storage configuration.
    pub measure_storage: StorageConfig,
}

impl TryFrom<Config> for ProcessorConfigSerialized {
    type Error = Error;
    fn try_from(config: Config) -> Result<Self> {
        let session_path = expand_home(&config.delegatee_key)?;
        let delegatee_key = fs::read_to_string(&session_path).map_err(|error| {
            Error::OpenFileError(format!("{}: {error}", session_path.display()))
        })?;

        let udp_range = crate::processor::parse_webrtc_udp_port_range(
            config.webrtc_udp_port_min,
            config.webrtc_udp_port_max,
        )?;

        Ok(Self {
            network_id: config.network_id,
            ice_servers: config.ice_servers,
            external_address: config.external_ip,
            webrtc_udp_port_min: udp_range.map(|range| range.min()),
            webrtc_udp_port_max: udp_range.map(|range| range.max()),
            delegatee_key,
            stabilize_interval: config.stabilize_interval,
            online_node_heartbeat_interval_secs: config.online_node_heartbeat_interval_secs,
            online_node_ttl_secs: config.online_node_ttl_secs,
            online_node_type: config.online_node_type,
            advertise_presence: config.advertise_presence,
            dht_virtual_nodes: config.dht_virtual_nodes,
            origin_quota: config.origin_quota,
            advertise_onion_relay: config.advertise_onion_relay,
            advertise_onion_exit: config.advertise_onion_exit,
            onion_exit_heartbeat_interval_secs: config.onion_exit_heartbeat_interval_secs,
            onion_exit_ttl_secs: config.onion_exit_ttl_secs,
            onion_exit_services: config.onion_exit_services,
            onion_exit_policy: config.onion_exit_policy,
        })
    }
}

impl TryFrom<Config> for ProcessorConfig {
    type Error = Error;
    fn try_from(config: Config) -> Result<Self> {
        ProcessorConfigSerialized::try_from(config).and_then(Self::try_from)
    }
}

impl Config {
    /// Creates a default native-node configuration using the supplied delegatee key path.
    pub fn new<P>(delegatee_key: P) -> Self
    where P: AsRef<std::path::Path> {
        let delegatee_key = delegatee_key.as_ref().to_string_lossy().to_string();
        Self {
            network_id: DEFAULT_NETWORK_ID,
            delegatee_key,
            internal_api_port: DEFAULT_INTERNAL_API_PORT,
            external_api_addr: DEFAULT_EXTERNAL_API_ADDR.to_string(),
            endpoint_url: DEFAULT_ENDPOINT_URL.to_string(),
            api_token_path: None,
            api_allowed_origins: Vec::new(),
            allow_remote_external_api: false,
            ice_servers: DEFAULT_ICE_SERVERS.to_string(),
            stabilize_interval: DEFAULT_STABILIZE_INTERVAL,
            online_node_heartbeat_interval_secs:
                crate::registration::default_online_node_heartbeat_interval_secs(),
            online_node_ttl_secs: crate::registration::default_online_node_ttl_secs(),
            online_node_type: crate::registration::default_online_node_type(),
            advertise_presence: crate::registration::default_advertise_presence(),
            advertise_onion_relay: crate::onion::default_advertise_onion_relay(),
            advertise_onion_exit: crate::onion::default_advertise_onion_exit(),
            onion_exit_heartbeat_interval_secs:
                crate::onion::default_onion_exit_heartbeat_interval_secs(),
            onion_exit_ttl_secs: crate::onion::default_onion_exit_ttl_secs(),
            onion_exit_services: crate::onion::default_onion_exit_services(),
            onion_exit_policy: crate::onion::default_onion_exit_policy(),
            onion_http_proxy_addr: None,
            onion_http_proxy_service: OnionServiceName::tcp(),
            onion_http_proxy_header_timeout_secs:
                crate::onion::proxy::http::default_connect_header_timeout_secs(),
            onion_http_proxy_max_connections:
                crate::onion::proxy::http::default_max_connect_connections(),
            gateway: Some(NativeGatewayConfig::disabled_default()),
            bootstrap: BootstrapConfig::default(),
            dht_virtual_nodes: DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER,
            origin_quota: OriginQuotaConfig::default(),
            external_ip: None,
            webrtc_udp_port_min: None,
            webrtc_udp_port_max: None,
            data_storage: DEFAULT_DATA_STORAGE_CONFIG.clone(),
            measure_storage: DEFAULT_MEASURE_STORAGE_CONFIG.clone(),
        }
    }

    /// The gateway section `rings run` starts a runner from, if any.
    ///
    /// Law: `enabled_gateway() = Some(g) ⟺ gateway = Some(g) ∧ g.enabled`. `--gateway` sets
    /// `enabled` before this is consulted, so the flag and the field select the same runner.
    pub fn enabled_gateway(&self) -> Option<&NativeGatewayConfig> {
        self.gateway.as_ref().filter(|gateway| gateway.enabled)
    }

    /// Writes this configuration to a YAML file and returns the written path.
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

    /// Reads a native-node configuration from a YAML file.
    pub fn read_fs<P>(path: P) -> Result<Config>
    where P: AsRef<std::path::Path> {
        let path = expand_home(path)?;
        tracing::debug!("Read config from: {:?}", path);
        let f = fs::File::open(path).map_err(|e| Error::OpenFileError(e.to_string()))?;
        let f_rdr = io::BufReader::new(f);
        serde_yaml::from_reader(f_rdr).map_err(|_| Error::EncodeError)
    }
}

/// Configuration for a node storage backend.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// Storage directory path.
    pub path: String,
    /// Storage capacity in bytes.
    pub capacity: u32,
}

impl StorageConfig {
    /// Creates a storage configuration from a path and capacity.
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

    fn dumped_delegatee_key() -> String {
        let key = SecretKey::random();
        let session = match DelegateeKey::new_with_seckey(&key) {
            Ok(session) => session,
            Err(error) => panic!("delegatee key construction failed: {error}"),
        };
        match session.dump() {
            Ok(dump) => dump,
            Err(error) => panic!("delegatee key dump failed: {error}"),
        }
    }

    /// Write a valid delegatee key to an isolated file and return its config plus cleanup path.
    fn config_with_session_file() -> (Config, PathBuf) {
        let path =
            std::env::temp_dir().join(format!("rings-session-{}.yaml", uuid::Uuid::new_v4()));
        fs::write(&path, dumped_delegatee_key()).expect("write test delegatee key");
        (Config::new(&path), path)
    }

    #[test]
    fn test_deserialization_defaults_online_registration_fields() {
        let yaml = r#"
network_id: 1
delegatee_key: delegatee_key
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 15
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
        assert_eq!(
            cfg.dht_virtual_nodes,
            DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
        );
        assert!(cfg.advertise_presence);
        assert!(!cfg.advertise_onion_relay);
        assert!(!cfg.advertise_onion_exit);
        assert_eq!(cfg.onion_http_proxy_addr, None);
        assert_eq!(cfg.onion_http_proxy_service, OnionServiceName::tcp());
        assert_eq!(
            cfg.onion_http_proxy_header_timeout_secs,
            crate::onion::proxy::http::default_connect_header_timeout_secs()
        );
        assert_eq!(
            cfg.onion_http_proxy_max_connections,
            crate::onion::proxy::http::default_max_connect_connections()
        );
        assert_eq!(
            cfg.onion_exit_services,
            crate::onion::default_onion_exit_services()
        );
        assert!(cfg.gateway.is_none());
        assert_eq!(cfg.api_token_path, None);
        assert!(cfg.api_allowed_origins.is_empty());
        assert!(!cfg.allow_remote_external_api);
    }

    #[test]
    fn test_deserialization_preserves_explicit_disabled_dht_virtual_nodes() {
        let yaml = r#"
network_id: 1
delegatee_key: delegatee_key
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 15
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

        assert_eq!(cfg.dht_virtual_nodes, 0);
    }

    const CONFIG_WITHOUT_GATEWAY_SECTION: &str = r#"
network_id: 1
delegatee_key: delegatee_key
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 15
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

    /// A hand-written section stating only what the gateway crate requires.
    const GATEWAY_SECTION_WITHOUT_ENABLED: &str = r#"
gateway:
  plan:
    addresses:
    - 100.64.0.1/32
    included_routes: []
    mtu: 1280
"#;

    const GATEWAY_SECTION_ENABLED: &str = r#"
gateway:
  enabled: true
  plan:
    addresses:
    - 100.64.0.1/32
    included_routes: []
    mtu: 1280
"#;

    fn config_from(document: &str) -> Config {
        match serde_yaml::from_str(document) {
            Ok(config) => config,
            Err(error) => panic!("config document must parse: {error}"),
        }
    }

    #[test]
    fn generated_config_round_trips_with_the_gateway_disabled() {
        let root = std::env::temp_dir().join(format!("rings-config-{}", uuid::Uuid::new_v4()));
        let path = root.join("config.yaml");
        let written = Config::new("delegatee_key").write_fs(&path);
        let restored = written.and_then(Config::read_fs);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(root);

        let restored = match restored {
            Ok(config) => config,
            Err(error) => panic!("generated config must round-trip: {error:?}"),
        };
        let Some(gateway) = restored.gateway.as_ref() else {
            panic!("generated config must carry a gateway section");
        };
        assert!(!gateway.enabled);
        assert_eq!(gateway.runtime.validate(), Ok(()));
        assert!(restored.enabled_gateway().is_none());
        assert_eq!(restored.origin_quota, OriginQuotaConfig::default());
    }

    /// The document `rings init` writes for a fresh delegatee key.
    fn generated_document() -> String {
        serde_yaml::to_string(&Config::new("delegatee_key")).expect("generated config serializes")
    }

    /// `rings init` states the bootstrap section explicitly, with no managed targets.
    #[test]
    fn generated_config_writes_an_empty_bootstrap_section() {
        let document = generated_document();
        assert!(document.contains("bootstrap:\n  peers: []\n"));
        assert_eq!(
            Config::new("delegatee_key").bootstrap,
            BootstrapConfig::default()
        );
    }

    /// A config written before the section existed loads with no managed targets, and an
    /// explicit section round-trips its peers including an optional token.
    #[test]
    fn bootstrap_section_defaults_to_empty_and_round_trips_peers() {
        let base = generated_document();
        let without = base.replace("bootstrap:\n  peers: []\n", "");
        assert!(!without.contains("bootstrap:"));
        assert!(config_from(&without).bootstrap.peers.is_empty());

        let with = base.replace(
            "bootstrap:\n  peers: []\n",
            "bootstrap:\n  peers:\n  - did: 0x1\n    url: https://seed.example.org/\n  - did: \
             0x2\n    url: https://seed2.example.org/\n    api_token: secret\n",
        );
        let peers = config_from(&with).bootstrap.peers;
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].did, "0x1");
        assert_eq!(peers[0].url, "https://seed.example.org/");
        assert_eq!(peers[0].api_token, None);
        assert_eq!(peers[1].api_token.as_deref(), Some("secret"));
    }

    #[test]
    fn generated_gateway_section_states_every_field() {
        let document = match serde_yaml::to_value(Config::new("delegatee_key")) {
            Ok(document) => document,
            Err(error) => panic!("generated config must serialize: {error}"),
        };
        let keys = document
            .get("gateway")
            .and_then(serde_yaml::Value::as_mapping)
            .map(|section| {
                section
                    .keys()
                    .filter_map(serde_yaml::Value::as_str)
                    .collect::<Vec<_>>()
            });

        assert_eq!(keys, Some(GATEWAY_KEYS.to_vec()));
    }

    /// Any key outside the gateway section's own is rejected, not ignored by the flattened
    /// section, so a misspelt key cannot silently fall back to its default.
    #[test]
    fn gateway_section_rejects_unknown_keys() {
        let document = format!(
            "{CONFIG_WITHOUT_GATEWAY_SECTION}{GATEWAY_SECTION_WITHOUT_ENABLED}  \
             onion_services: https\n"
        );

        let error = match serde_yaml::from_str::<Config>(&document) {
            Ok(_) => panic!("a misspelt gateway key must be rejected"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("unknown gateway key \"onion_services\""));
    }

    /// The route-length keys removed by the onion loop cutover are rejected by name, not ignored
    /// by the flattened gateway section.
    #[test]
    fn gateway_section_rejects_removed_route_length_keys() {
        for (key, value) in [
            ("onion_hop_count", "3"),
            ("onion_allow_short_paths", "true"),
        ] {
            let document = format!(
                "{CONFIG_WITHOUT_GATEWAY_SECTION}{GATEWAY_SECTION_WITHOUT_ENABLED}  \
                 {key}: {value}\n"
            );

            let error = match serde_yaml::from_str::<Config>(&document) {
                Ok(_) => panic!("gateway.{key} must be rejected"),
                Err(error) => error.to_string(),
            };

            assert!(error.contains(&format!("gateway.{key} was removed")));
            assert!(error.contains("loop shape"));
        }
    }

    #[test]
    fn gateway_section_without_enabled_is_inert() {
        let document = format!("{CONFIG_WITHOUT_GATEWAY_SECTION}{GATEWAY_SECTION_WITHOUT_ENABLED}");
        let config = config_from(&document);

        assert!(matches!(config.gateway, Some(ref gateway) if !gateway.enabled));
        assert!(config.enabled_gateway().is_none());
    }

    #[test]
    fn explicitly_enabled_gateway_section_selects_a_runner() {
        let document = format!("{CONFIG_WITHOUT_GATEWAY_SECTION}{GATEWAY_SECTION_ENABLED}");
        let config = config_from(&document);

        assert!(config.enabled_gateway().is_some());
    }

    #[test]
    fn enabling_the_generated_section_selects_a_runner() {
        let mut config = Config::new("delegatee_key");
        assert!(config.enabled_gateway().is_none());

        if let Some(gateway) = config.gateway.as_mut() {
            gateway.enabled = true;
        }

        assert!(config.enabled_gateway().is_some());
    }

    #[test]
    fn rendered_yaml_section_completes_a_config_without_one() {
        let section = match NativeGatewayConfig::disabled_default().to_yaml_section() {
            Ok(section) => section,
            Err(error) => panic!("section must render: {error:?}"),
        };
        assert!(section.starts_with("gateway:\n"));

        let config = config_from(&format!("{CONFIG_WITHOUT_GATEWAY_SECTION}{section}"));

        assert!(matches!(config.gateway, Some(ref gateway) if !gateway.enabled));
    }

    #[test]
    fn test_config_with_valid_webrtc_udp_range_builds_processor_config() {
        let (mut config, session_path) = config_with_session_file();
        config.webrtc_udp_port_min = Some(49160);
        config.webrtc_udp_port_max = Some(49200);

        let processor_config = ProcessorConfig::try_from(config);
        let _ = fs::remove_file(session_path);

        assert!(matches!(
            processor_config.and_then(|config| config.webrtc_udp_port_range()),
            Ok(Some(range)) if range.min() == 49160 && range.max() == 49200
        ));
    }

    #[test]
    fn test_config_with_partial_webrtc_udp_range_is_rejected() {
        let (mut config, session_path) = config_with_session_file();
        config.webrtc_udp_port_min = Some(49160);

        let processor_config = ProcessorConfig::try_from(config);
        let _ = fs::remove_file(session_path);

        assert!(matches!(
            processor_config,
            Err(Error::IncompleteWebrtcUdpPortRange {
                min: Some(49160),
                max: None
            })
        ));
    }

    /// Raw session dumps are not reinterpreted after file lookup fails.
    #[test]
    fn test_raw_session_dump_is_not_treated_as_a_path_fallback() {
        let result = ProcessorConfig::try_from(Config::new(dumped_delegatee_key()));

        assert!(matches!(result, Err(Error::OpenFileError(_))));
    }

    /// Removed signer and session-manager fields fail deserialization under total cutover.
    #[test]
    fn test_removed_legacy_config_fields_are_rejected() {
        for legacy_field in ["ecdsa_key", "session_manager"] {
            let document = generated_document().replace(
                "network_id:",
                &format!("{legacy_field}: legacy\nnetwork_id:"),
            );
            let result = serde_yaml::from_str::<Config>(&document);

            assert!(
                result.is_err(),
                "legacy field {legacy_field} must be rejected"
            );
        }
    }
}
