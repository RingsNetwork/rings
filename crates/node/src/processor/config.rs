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
    /// Whether this node advertises onion relay capability in the online-node registry.
    pub(in crate::processor) advertise_onion_relay: bool,
    /// Whether this node publishes an onion-exit descriptor.
    pub(in crate::processor) advertise_onion_exit: bool,
    /// Onion-exit registry heartbeat interval.
    pub(in crate::processor) onion_exit_heartbeat_interval: Duration,
    /// Onion-exit registry descriptor TTL.
    pub(in crate::processor) onion_exit_ttl: Duration,
    /// Services this node publishes when onion exit advertisement is enabled.
    pub(in crate::processor) onion_exit_services: Vec<OnionExitService>,
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

    /// Enables HTTPS-only onion exit advertisement.
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

    /// Return the browser HTTPS onion-exit policy when this config advertises that service.
    #[cfg(feature = "browser")]
    pub fn onion_https_exit_policy(&self) -> Option<OnionExitPolicy> {
        (self.advertise_onion_exit
            && self.onion_exit_services.iter().any(|service| {
                service.matches(
                    ONION_PROXY_HTTPS_SERVICE,
                    crate::onion::OnionExitTransport::Https,
                )
            }))
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
    network_id: u32,
    /// A string representing ICE servers for WebRTC
    ice_servers: String,
    /// An optional string representing the external address for WebRTC
    external_address: Option<String>,
    /// Inclusive lower native WebRTC UDP port bound.
    webrtc_udp_port_min: Option<u16>,
    /// Inclusive upper native WebRTC UDP port bound.
    webrtc_udp_port_max: Option<u16>,
    /// A string representing the dumped `SessionSk`.
    session_sk: String,
    /// An unsigned integer representing the stabilization interval in seconds.
    stabilize_interval: u64,
    /// Online-node registry heartbeat interval in seconds.
    #[serde(default = "default_online_node_heartbeat_interval_secs")]
    online_node_heartbeat_interval_secs: u64,
    /// Online-node registry descriptor TTL in seconds.
    #[serde(default = "default_online_node_ttl_secs")]
    online_node_ttl_secs: u64,
    /// Runtime family advertised in the online-node registry.
    #[serde(default = "default_online_node_type")]
    online_node_type: OnlineNodeType,
    /// Whether listen() advertises this node's presence.
    #[serde(default = "default_advertise_presence")]
    advertise_presence: bool,
    /// Storage-only virtual positions derived per physical peer.
    #[serde(default = "default_storage_virtual_positions_per_owner")]
    dht_virtual_nodes: u16,
    /// Whether listen() advertises onion relay capability.
    #[serde(default = "default_advertise_onion_relay")]
    advertise_onion_relay: bool,
    /// Whether listen() publishes an onion-exit descriptor.
    #[serde(default = "default_advertise_onion_exit")]
    advertise_onion_exit: bool,
    /// Onion-exit registry heartbeat interval in seconds.
    #[serde(default = "default_onion_exit_heartbeat_interval_secs")]
    onion_exit_heartbeat_interval_secs: u64,
    /// Onion-exit registry descriptor TTL in seconds.
    #[serde(default = "default_onion_exit_ttl_secs")]
    onion_exit_ttl_secs: u64,
    /// Exit services advertised by this node.
    #[serde(default = "default_onion_exit_services")]
    onion_exit_services: Vec<OnionExitService>,
    /// Exit policy advertised by this node.
    #[serde(default = "default_onion_exit_policy")]
    onion_exit_policy: OnionExitPolicy,
}

impl ProcessorConfigSerialized {
    /// Creates a new `ProcessorConfigSerialized` instance without an external address.
    pub fn new(
        network_id: u32,
        ice_servers: String,
        session_sk: String,
        stabilize_interval: u64,
    ) -> Self {
        Self {
            network_id,
            ice_servers,
            external_address: None,
            webrtc_udp_port_min: None,
            webrtc_udp_port_max: None,
            session_sk,
            stabilize_interval,
            online_node_heartbeat_interval_secs: default_online_node_heartbeat_interval_secs(),
            online_node_ttl_secs: default_online_node_ttl_secs(),
            online_node_type: default_online_node_type(),
            advertise_presence: default_advertise_presence(),
            dht_virtual_nodes: DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER,
            advertise_onion_relay: default_advertise_onion_relay(),
            advertise_onion_exit: default_advertise_onion_exit(),
            onion_exit_heartbeat_interval_secs: default_onion_exit_heartbeat_interval_secs(),
            onion_exit_ttl_secs: default_onion_exit_ttl_secs(),
            onion_exit_services: default_onion_exit_services(),
            onion_exit_policy: default_onion_exit_policy(),
        }
    }

    /// Sets up the external address for WebRTC.
    /// This will be used to configure the transport to listen for WebRTC connections in "HOST" mode.
    pub fn external_address(mut self, external_address: String) -> Self {
        self.external_address = Some(external_address);
        self
    }

    /// Sets the native WebRTC UDP port range bounds.
    pub fn webrtc_udp_port_range(mut self, range: WebrtcUdpPortRange) -> Self {
        self.webrtc_udp_port_min = Some(range.min());
        self.webrtc_udp_port_max = Some(range.max());
        self
    }

    /// Sets the online-node registry heartbeat interval in seconds.
    pub fn online_node_heartbeat_interval_secs(mut self, interval_secs: u64) -> Self {
        self.online_node_heartbeat_interval_secs = interval_secs;
        self
    }

    /// Sets the online-node registry descriptor TTL in seconds.
    pub fn online_node_ttl_secs(mut self, ttl_secs: u64) -> Self {
        self.online_node_ttl_secs = ttl_secs;
        self
    }

    /// Sets the runtime family advertised in the online-node registry.
    pub fn online_node_type(mut self, node_type: OnlineNodeType) -> Self {
        self.online_node_type = node_type;
        self
    }

    /// Sets whether listen() advertises this node's presence.
    pub fn advertise_presence(mut self, advertise: bool) -> Self {
        self.advertise_presence = advertise;
        self
    }

    /// Sets whether listen() advertises onion relay capability.
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

    /// Sets whether listen() publishes an onion-exit descriptor.
    pub fn advertise_onion_exit(mut self, advertise: bool) -> Self {
        self.advertise_onion_exit = advertise;
        self
    }

    /// Sets the onion-exit registry heartbeat interval in seconds.
    pub fn onion_exit_heartbeat_interval_secs(mut self, interval_secs: u64) -> Self {
        self.onion_exit_heartbeat_interval_secs = interval_secs;
        self
    }

    /// Sets the onion-exit registry descriptor TTL in seconds.
    pub fn onion_exit_ttl_secs(mut self, ttl_secs: u64) -> Self {
        self.onion_exit_ttl_secs = ttl_secs;
        self
    }

    /// Sets the onion-exit services advertised by this node.
    pub fn onion_exit_services(mut self, services: Vec<OnionExitService>) -> Self {
        self.onion_exit_services = services;
        self
    }

    /// Sets the onion-exit policy advertised by this node.
    pub fn onion_exit_policy(mut self, policy: OnionExitPolicy) -> Self {
        self.onion_exit_policy = policy;
        self
    }

    /// Enables HTTPS-only onion exit advertisement.
    pub fn enable_https_onion_exit(mut self) -> Self {
        self.advertise_onion_exit = true;
        self.onion_exit_services = https_onion_exit_services();
        self
    }

    /// Enables the default native onion exit services.
    pub fn enable_default_onion_exit(mut self) -> Self {
        self.advertise_onion_exit = true;
        self.onion_exit_services = default_onion_exit_services();
        self
    }
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

fn validate_dht_virtual_nodes(positions_per_peer: u16) -> Result<()> {
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
    onion_exit_services: &[OnionExitService],
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
        for service in onion_exit_services {
            if let Some(expected) = OnionExitService::reserved_transport(service.name.as_str()) {
                if service.transport == expected {
                    continue;
                }
                return Err(Error::InvalidConfig(format!(
                    "onion exit service {:?} must use {:?} transport, got {:?}",
                    service.name, expected, service.transport
                )));
            }
        }
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
        validate_dht_virtual_nodes(ins.dht_virtual_nodes)?;
        let online_node_heartbeat_interval =
            Duration::from_secs(ins.online_node_heartbeat_interval_secs);
        let online_node_ttl = Duration::from_secs(ins.online_node_ttl_secs);
        let onion_exit_heartbeat_interval =
            Duration::from_secs(ins.onion_exit_heartbeat_interval_secs);
        let onion_exit_ttl = Duration::from_secs(ins.onion_exit_ttl_secs);
        validate_online_node_registration_timing(
            ins.advertise_presence,
            online_node_heartbeat_interval,
            online_node_ttl,
        )?;
        validate_onion_exit_registration_timing(
            ins.advertise_onion_exit,
            onion_exit_heartbeat_interval,
            onion_exit_ttl,
        )?;
        validate_onion_role_config(
            ins.advertise_presence,
            ins.advertise_onion_relay,
            ins.advertise_onion_exit,
            &ins.onion_exit_services,
            &ins.onion_exit_policy,
        )?;
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
