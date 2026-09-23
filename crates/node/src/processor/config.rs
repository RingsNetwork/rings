use rings_core::dht::default_storage_virtual_positions_per_owner;
use rings_core::dht::VirtualNodeConfig;
use rings_core::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;

use super::*;

/// ProcessorConfig is usually serialized as json or yaml.
/// There is a `from_config` method in [ProcessorBuilder] used to initialize the Builder with a serialized ProcessorConfig.
#[derive(Clone, Debug)]
#[wasm_export]
pub struct ProcessorConfig {
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    pub(in crate::processor) network_id: u32,
    /// ICE servers for webrtc
    pub(in crate::processor) ice_servers: String,
    /// External address for webrtc
    pub(in crate::processor) external_address: Option<String>,
    /// Inclusive lower native WebRTC UDP port bound.
    pub(in crate::processor) webrtc_udp_port_min: Option<u16>,
    /// Inclusive upper native WebRTC UDP port bound.
    pub(in crate::processor) webrtc_udp_port_max: Option<u16>,
    /// [SessionSk].
    pub(in crate::processor) session_sk: SessionSk,
    /// Stabilization interval.
    pub(in crate::processor) stabilize_interval: Duration,
    /// Online-node registry heartbeat interval.
    pub(in crate::processor) online_node_heartbeat_interval: Duration,
    /// Online-node registry descriptor TTL.
    pub(in crate::processor) online_node_ttl: Duration,
    /// Runtime family advertised in the online-node registry.
    pub(in crate::processor) online_node_type: OnlineNodeType,
    /// Whether listen() advertises this node's presence.
    pub(in crate::processor) advertise_presence: bool,
    /// Storage-only virtual positions derived per physical peer.
    pub(in crate::processor) dht_virtual_nodes: u16,
    /// Runtime-local final-destination quotas keyed by verified origin and logical lane.
    pub(in crate::processor) origin_quota: OriginQuotaConfig,
    /// Whether this node advertises onion relay capability in the online-node registry.
    pub(in crate::processor) advertise_onion_relay: bool,
    /// Whether this node publishes an onion-exit descriptor.
    pub(in crate::processor) advertise_onion_exit: bool,
    /// Onion-exit registry heartbeat interval.
    pub(in crate::processor) onion_exit_heartbeat_interval: Duration,
    /// Onion-exit registry descriptor TTL.
    pub(in crate::processor) onion_exit_ttl: Duration,
    /// Services this node publishes when onion exit advertisement is enabled.
    pub(in crate::processor) onion_exit_services: Vec<OnionServiceName>,
    /// Exit policy this node publishes when onion exit advertisement is enabled.
    pub(in crate::processor) onion_exit_policy: OnionExitPolicy,
}

#[wasm_export]
impl ProcessorConfig {
    /// Creates a new `ProcessorConfig` instance without an external address.
    pub fn new(
        network_id: u32,
        ice_servers: String,
        session_sk: SessionSk,
        stabilize_interval: u64,
    ) -> Self {
        Self {
            network_id,
            ice_servers,
            external_address: None,
            webrtc_udp_port_min: None,
            webrtc_udp_port_max: None,
            session_sk,
            stabilize_interval: Duration::from_secs(stabilize_interval),
            online_node_heartbeat_interval: Duration::from_secs(
                default_online_node_heartbeat_interval_secs(),
            ),
            online_node_ttl: Duration::from_secs(default_online_node_ttl_secs()),
            online_node_type: default_online_node_type(),
            advertise_presence: default_advertise_presence(),
            dht_virtual_nodes: DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER,
            origin_quota: OriginQuotaConfig::default(),
            advertise_onion_relay: default_advertise_onion_relay(),
            advertise_onion_exit: default_advertise_onion_exit(),
            onion_exit_heartbeat_interval: Duration::from_secs(
                default_onion_exit_heartbeat_interval_secs(),
            ),
            onion_exit_ttl: Duration::from_secs(default_onion_exit_ttl_secs()),
            onion_exit_services: default_onion_exit_services(),
            onion_exit_policy: default_onion_exit_policy(),
        }
    }

    /// Return associated [SessionSk].
    pub fn session_sk(&self) -> SessionSk {
        self.session_sk.clone()
    }

    /// Return the overlay this node joins.
    pub fn network_id(&self) -> u32 {
        self.network_id
    }

    /// Enables only the standard HTTPS-over-TCP onion exit service.
    pub fn enable_https_onion_exit(mut self) -> Self {
        self.advertise_onion_exit = true;
        self.onion_exit_services = https_onion_exit_services();
        self
    }

    /// Enables default native onion exit advertisement.
    pub fn enable_default_onion_exit(mut self) -> Self {
        self.advertise_onion_exit = true;
        self.onion_exit_services = default_onion_exit_services();
        self
    }

    /// Sets whether listen() advertises this node as an onion relay.
    pub fn advertise_onion_relay(mut self, advertise: bool) -> Self {
        self.advertise_onion_relay = advertise;
        self
    }

    /// Sets storage-only virtual positions derived per physical peer.
    ///
    /// Serialized configs reject values above
    /// [`MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER`]. This setter is infallible
    /// for direct programmatic use; the core swarm builder normalizes the value
    /// once before storage ownership and protocol advertisement are created.
    pub fn dht_virtual_nodes(mut self, positions_per_peer: u16) -> Self {
        self.dht_virtual_nodes = positions_per_peer;
        self
    }

    /// Sets whether listen() publishes this node as an onion exit.
    pub fn advertise_onion_exit(mut self, advertise: bool) -> Self {
        self.advertise_onion_exit = advertise;
        self
    }
}

impl ProcessorConfig {
    /// Returns the validated native WebRTC UDP port range, when configured.
    pub fn webrtc_udp_port_range(&self) -> Result<Option<WebrtcUdpPortRange>> {
        parse_webrtc_udp_port_range(self.webrtc_udp_port_min, self.webrtc_udp_port_max)
    }

    /// Sets the onion-exit policy.
    pub fn onion_exit_policy(mut self, policy: OnionExitPolicy) -> Self {
        self.onion_exit_policy = policy;
        self
    }

    /// Sets runtime-local final-destination quotas keyed by verified origin and logical lane.
    pub fn origin_quota(mut self, config: OriginQuotaConfig) -> Self {
        self.origin_quota = config;
        self
    }

    /// Return the HTTPS onion-exit policy when this config advertises that service.
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    pub fn onion_https_exit_policy(&self) -> Option<OnionExitPolicy> {
        (self.advertise_onion_exit
            && self
                .onion_exit_services
                .iter()
                .any(|service| service.matches(ONION_PROXY_HTTPS_SERVICE)))
        .then(|| self.onion_exit_policy.clone())
    }
}

impl FromStr for ProcessorConfig {
    type Err = Error;
    /// Reveal config from serialized string.
    fn from_str(ser: &str) -> Result<Self> {
        serde_yaml::from_str::<ProcessorConfig>(ser).map_err(Error::SerdeYamlError)
    }
}

/// `ProcessorConfigSerialized` is a serialized version of `ProcessorConfig`.
/// Instead of storing the `SessionSk` instance, it stores the dumped string representation of the session secret key.
#[derive(Serialize, Deserialize, Clone)]
#[wasm_export]
pub struct ProcessorConfigSerialized {
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    pub(crate) network_id: u32,
    /// A string representing ICE servers for WebRTC
    pub(crate) ice_servers: String,
    /// An optional string representing the external address for WebRTC
    pub(crate) external_address: Option<String>,
    /// Inclusive lower native WebRTC UDP port bound.
    pub(crate) webrtc_udp_port_min: Option<u16>,
    /// Inclusive upper native WebRTC UDP port bound.
    pub(crate) webrtc_udp_port_max: Option<u16>,
    /// A string representing the dumped `SessionSk`.
    pub(crate) session_sk: String,
    /// An unsigned integer representing the stabilization interval in seconds.
    pub(crate) stabilize_interval: u64,
    /// Online-node registry heartbeat interval in seconds.
    #[serde(default = "default_online_node_heartbeat_interval_secs")]
    pub(crate) online_node_heartbeat_interval_secs: u64,
    /// Online-node registry descriptor TTL in seconds.
    #[serde(default = "default_online_node_ttl_secs")]
    pub(crate) online_node_ttl_secs: u64,
    /// Runtime family advertised in the online-node registry.
    #[serde(default = "default_online_node_type")]
    pub(crate) online_node_type: OnlineNodeType,
    /// Whether listen() advertises this node's presence.
    #[serde(default = "default_advertise_presence")]
    pub(crate) advertise_presence: bool,
    /// Storage-only virtual positions derived per physical peer.
    #[serde(default = "default_storage_virtual_positions_per_owner")]
    pub(crate) dht_virtual_nodes: u16,
    /// Runtime-local final-destination quotas keyed by verified origin and logical lane.
    #[serde(default)]
    pub(crate) origin_quota: OriginQuotaConfig,
    /// Whether listen() advertises onion relay capability.
    #[serde(default = "default_advertise_onion_relay")]
    pub(crate) advertise_onion_relay: bool,
    /// Whether listen() publishes an onion-exit descriptor.
    #[serde(default = "default_advertise_onion_exit")]
    pub(crate) advertise_onion_exit: bool,
    /// Onion-exit registry heartbeat interval in seconds.
    #[serde(default = "default_onion_exit_heartbeat_interval_secs")]
    pub(crate) onion_exit_heartbeat_interval_secs: u64,
    /// Onion-exit registry descriptor TTL in seconds.
    #[serde(default = "default_onion_exit_ttl_secs")]
    pub(crate) onion_exit_ttl_secs: u64,
    /// Exit services advertised by this node.
    #[serde(default = "default_onion_exit_services")]
    pub(crate) onion_exit_services: Vec<OnionServiceName>,
    /// Exit policy advertised by this node.
    #[serde(default = "default_onion_exit_policy")]
    pub(crate) onion_exit_policy: OnionExitPolicy,
}

pub(crate) fn parse_webrtc_udp_port_range(
    min: Option<u16>,
    max: Option<u16>,
) -> Result<Option<WebrtcUdpPortRange>> {
    match (min, max) {
        (None, None) => Ok(None),
        (Some(min), Some(max)) => WebrtcUdpPortRange::new(min, max)
            .map(Some)
            .map_err(Error::from),
        (min, max) => Err(Error::IncompleteWebrtcUdpPortRange { min, max }),
    }
}

pub(in crate::processor) fn validate_dht_virtual_nodes(positions_per_peer: u16) -> Result<()> {
    if VirtualNodeConfig::positions_per_owner_within_limit(positions_per_peer) {
        return Ok(());
    }

    Err(Error::InvalidConfig(format!(
        "dht_virtual_nodes {positions_per_peer} exceeds maximum {MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER}"
    )))
}

pub(in crate::processor) fn validate_onion_role_config(
    advertise_presence: bool,
    advertise_onion_relay: bool,
    advertise_onion_exit: bool,
    onion_exit_services: &[OnionServiceName],
    onion_exit_policy: &OnionExitPolicy,
) -> Result<()> {
    if advertise_onion_relay && !advertise_presence {
        return Err(Error::InvalidConfig(
            "advertise_onion_relay requires advertise_presence because relay capability is published in online-node descriptors"
                .to_string(),
        ));
    }
    if advertise_onion_exit && onion_exit_services.is_empty() {
        return Err(Error::InvalidConfig(
            "advertise_onion_exit requires at least one onion_exit_services entry".to_string(),
        ));
    }
    if advertise_onion_exit {
        onion_exit_policy.validate_targets()?;
    }
    Ok(())
}

impl TryFrom<ProcessorConfig> for ProcessorConfigSerialized {
    type Error = Error;
    fn try_from(ins: ProcessorConfig) -> Result<Self> {
        Ok(Self {
            network_id: ins.network_id,
            ice_servers: ins.ice_servers.clone(),
            external_address: ins.external_address.clone(),
            webrtc_udp_port_min: ins.webrtc_udp_port_min,
            webrtc_udp_port_max: ins.webrtc_udp_port_max,
            session_sk: ins.session_sk.dump()?,
            stabilize_interval: ins.stabilize_interval.as_secs(),
            online_node_heartbeat_interval_secs: ins.online_node_heartbeat_interval.as_secs(),
            online_node_ttl_secs: ins.online_node_ttl.as_secs(),
            online_node_type: ins.online_node_type,
            advertise_presence: ins.advertise_presence,
            dht_virtual_nodes: ins.dht_virtual_nodes,
            origin_quota: ins.origin_quota,
            advertise_onion_relay: ins.advertise_onion_relay,
            advertise_onion_exit: ins.advertise_onion_exit,
            onion_exit_heartbeat_interval_secs: ins.onion_exit_heartbeat_interval.as_secs(),
            onion_exit_ttl_secs: ins.onion_exit_ttl.as_secs(),
            onion_exit_services: ins.onion_exit_services,
            onion_exit_policy: ins.onion_exit_policy,
        })
    }
}

impl TryFrom<ProcessorConfigSerialized> for ProcessorConfig {
    type Error = Error;
    fn try_from(ins: ProcessorConfigSerialized) -> Result<Self> {
        let webrtc_udp_port_range =
            parse_webrtc_udp_port_range(ins.webrtc_udp_port_min, ins.webrtc_udp_port_max)?;
        let online_node_heartbeat_interval =
            Duration::from_secs(ins.online_node_heartbeat_interval_secs);
        let online_node_ttl = Duration::from_secs(ins.online_node_ttl_secs);
        let onion_exit_heartbeat_interval =
            Duration::from_secs(ins.onion_exit_heartbeat_interval_secs);
        let onion_exit_ttl = Duration::from_secs(ins.onion_exit_ttl_secs);
        Ok(Self {
            network_id: ins.network_id,
            ice_servers: ins.ice_servers.clone(),
            external_address: ins.external_address.clone(),
            webrtc_udp_port_min: webrtc_udp_port_range.map(WebrtcUdpPortRange::min),
            webrtc_udp_port_max: webrtc_udp_port_range.map(WebrtcUdpPortRange::max),
            session_sk: SessionSk::from_str(&ins.session_sk)?,
            stabilize_interval: Duration::from_secs(ins.stabilize_interval),
            online_node_heartbeat_interval,
            online_node_ttl,
            online_node_type: ins.online_node_type,
            advertise_presence: ins.advertise_presence,
            dht_virtual_nodes: ins.dht_virtual_nodes,
            origin_quota: ins.origin_quota,
            advertise_onion_relay: ins.advertise_onion_relay,
            advertise_onion_exit: ins.advertise_onion_exit,
            onion_exit_heartbeat_interval,
            onion_exit_ttl,
            onion_exit_services: ins.onion_exit_services,
            onion_exit_policy: ins.onion_exit_policy,
        })
    }
}

impl Serialize for ProcessorConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        let ins: ProcessorConfigSerialized = self
            .clone()
            .try_into()
            .map_err(|e: Error| serde::ser::Error::custom(e.to_string()))?;
        ProcessorConfigSerialized::serialize(&ins, serializer)
    }
}

impl<'de> serde::de::Deserialize<'de> for ProcessorConfig {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        match ProcessorConfigSerialized::deserialize(deserializer) {
            Ok(ins) => {
                let cfg: ProcessorConfig = ins
                    .try_into()
                    .map_err(|e: Error| serde::de::Error::custom(e.to_string()))?;
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }
}
