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
    /// [DelegateeKey].
    pub(in crate::processor) delegatee_key: DelegateeKey,
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
    /// Onion symbols this process registers (#834 D2).
    pub(in crate::processor) onion_role: OnionRole<OnionExitOffer>,
    /// Onion-exit registry heartbeat interval.
    pub(in crate::processor) onion_exit_heartbeat_interval: Duration,
    /// Onion-exit registry descriptor TTL.
    pub(in crate::processor) onion_exit_ttl: Duration,
}

#[wasm_export]
impl ProcessorConfig {
    /// Creates a new `ProcessorConfig` instance without an external address.
    pub fn new(
        network_id: u32,
        ice_servers: String,
        delegatee_key: DelegateeKey,
        stabilize_interval: u64,
    ) -> Self {
        Self {
            network_id,
            ice_servers,
            external_address: None,
            webrtc_udp_port_min: None,
            webrtc_udp_port_max: None,
            delegatee_key,
            stabilize_interval: Duration::from_secs(stabilize_interval),
            online_node_heartbeat_interval: Duration::from_secs(
                default_online_node_heartbeat_interval_secs(),
            ),
            online_node_ttl: Duration::from_secs(default_online_node_ttl_secs()),
            online_node_type: default_online_node_type(),
            advertise_presence: default_advertise_presence(),
            dht_virtual_nodes: DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER,
            origin_quota: OriginQuotaConfig::default(),
            onion_role: OnionRole::Client,
            onion_exit_heartbeat_interval: Duration::from_secs(
                default_onion_exit_heartbeat_interval_secs(),
            ),
            onion_exit_ttl: Duration::from_secs(default_onion_exit_ttl_secs()),
        }
    }

    /// Return associated [DelegateeKey].
    pub fn delegatee_key(&self) -> DelegateeKey {
        self.delegatee_key.clone()
    }

    /// Return the overlay this node joins.
    pub fn network_id(&self) -> u32 {
        self.network_id
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
}

impl ProcessorConfig {
    /// Returns the validated native WebRTC UDP port range, when configured.
    pub fn webrtc_udp_port_range(&self) -> Result<Option<WebrtcUdpPortRange>> {
        parse_webrtc_udp_port_range(self.webrtc_udp_port_min, self.webrtc_udp_port_max)
    }

    /// Sets the onion symbols this process registers (#834 D2).
    pub fn onion_role(mut self, role: OnionRole<OnionExitOffer>) -> Self {
        self.onion_role = role;
        self
    }

    /// Sets runtime-local final-destination quotas keyed by verified origin and logical lane.
    pub fn origin_quota(mut self, config: OriginQuotaConfig) -> Self {
        self.origin_quota = config;
        self
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
/// Instead of storing the `DelegateeKey` instance, it stores the dumped string representation of the session secret key.
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
    /// A string representing the dumped `DelegateeKey`.
    pub(crate) delegatee_key: String,
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

/// Validate that the onion role can be published: `relay ∈ Σ_n ⇒ advertise_presence`, because the
/// relay registration lives in the online-node descriptor. `Σ_n ≠ ∅ ⇒ relay ∈ Σ_n` needs no check:
/// [`OnionRole`] has no rung that registers a symbol without `relay`.
pub(in crate::processor) fn validate_onion_role_config(
    advertise_presence: bool,
    onion_role: &OnionRole<OnionExitOffer>,
) -> Result<()> {
    if onion_role.registers_relay() && !advertise_presence {
        return Err(Error::InvalidConfig(
            "advertise_onion_relay requires advertise_presence because relay capability is published in online-node descriptors"
                .to_string(),
        ));
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
            delegatee_key: ins.delegatee_key.dump()?,
            stabilize_interval: ins.stabilize_interval.as_secs(),
            online_node_heartbeat_interval_secs: ins.online_node_heartbeat_interval.as_secs(),
            online_node_ttl_secs: ins.online_node_ttl.as_secs(),
            online_node_type: ins.online_node_type,
            advertise_presence: ins.advertise_presence,
            dht_virtual_nodes: ins.dht_virtual_nodes,
            origin_quota: ins.origin_quota,
            advertise_onion_relay: ins.onion_role.registers_relay(),
            advertise_onion_exit: ins.onion_role.exit().is_some(),
            onion_exit_heartbeat_interval_secs: ins.onion_exit_heartbeat_interval.as_secs(),
            onion_exit_ttl_secs: ins.onion_exit_ttl.as_secs(),
            onion_exit_services: ins
                .onion_role
                .exit()
                .map_or_else(default_onion_exit_services, |offer| {
                    offer.services().iter().cloned().collect()
                }),
            onion_exit_policy: ins
                .onion_role
                .exit()
                .map_or_else(default_onion_exit_policy, |offer| offer.policy().clone()),
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
            delegatee_key: DelegateeKey::from_str(&ins.delegatee_key)?,
            stabilize_interval: Duration::from_secs(ins.stabilize_interval),
            online_node_heartbeat_interval,
            online_node_ttl,
            online_node_type: ins.online_node_type,
            advertise_presence: ins.advertise_presence,
            dht_virtual_nodes: ins.dht_virtual_nodes,
            origin_quota: ins.origin_quota,
            onion_role: OnionRole::from_flags(
                ins.advertise_onion_relay,
                ins.advertise_onion_exit,
                ins.onion_exit_services,
                ins.onion_exit_policy,
            )?,
            onion_exit_heartbeat_interval,
            onion_exit_ttl,
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
