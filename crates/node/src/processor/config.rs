use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::RwLock;

use rings_core::dht::default_storage_virtual_positions_per_owner;
use rings_core::dht::VirtualNodeConfig;
use rings_core::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::ecc::keys::SigningSecretKey;
use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;
use rings_runtime::MaybeSendSync;
use rings_transport::core::transport::SharedTransport;
pub use rings_transport::network_config::TransportNetworkConfig;
pub use rings_transport::network_config::UdpPortRange;
pub use rings_transport::network_config::UdpPortRangeError;
use rings_turn::TurnCredentialSigner;

use super::*;
use crate::remote_endpoint::RemoteRpcProxy;

/// Infallible phase-2 commit closure returned by
/// [`NodeNetworkParameters::prepare_session_key_rotation`].
///
/// Executed synchronously after all phase-1 validations and durable key persistence succeed, so
/// committing the prepared session credentials in memory cannot fail or require rollback.
pub type SessionRotationCommit = Box<dyn FnOnce()>;

/// Factory closure converting a deserialized `params` JSON value from a [`NetworkConfigDescriptor`]
/// into a validated [`NodeNetworkParameters`] instance.
pub type NetworkParametersFactory =
    Arc<dyn Fn(&serde_json::Value) -> Result<Arc<dyn NodeNetworkParameters>> + Send + Sync>;

fn default_empty_network_params() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Serde-extensible tagged network configuration descriptor (#1291).
///
/// Allows native `config.yaml`, [`ProcessorConfigSerialized`], FFI, and browser WASM serialized
/// configurations to instantiate any registered [`NodeNetworkParameters`] backend (`"webrtc"`,
/// `"dummy"`, or embedder-registered custom media such as `"quic"`, `"ble"`, or `"packet-radio"`)
/// without modifying `rings-node`'s configuration structs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfigDescriptor {
    /// Registered transport backend kind identifier (for example `"webrtc"` or `"quic"`).
    pub kind: String,
    /// Backend-specific configuration object decoded by the registered factory.
    #[serde(default = "default_empty_network_params")]
    pub params: serde_json::Value,
}

/// Explicit TURN credential signing mode for WebRTC network parameters (#1291).
#[derive(Clone, Debug)]
pub enum TurnCredentialMode {
    /// Default session-scoped mode: signs `?pubkey=` TURN URLs with the active [`DelegateeKey`]
    /// and rotates TURN credentials in lockstep whenever `renew_delegatee_key` commits.
    Delegatee,
    /// Static account-scoped mode: signs `?pubkey=` TURN URLs directly with a non-expiring
    /// [`SigningSecretKey`] (`Did` / `Account`) at startup and leaves credentials unchanged across
    /// session-key rotations.
    ///
    /// Note: `Account`-mode [`WebrtcNetworkParameters`] intentionally return `None` from
    /// [`NodeNetworkParameters::to_network_descriptor`] so long-lived account secret keys are never
    /// serialized into [`ProcessorConfigSerialized`] documents.
    Account(Arc<SigningSecretKey>),
}

/// Validate that every `turn://`, `turns://`, RFC 7065 `turn:`, or `turns:` entry carrying a
/// `pubkey` query parameter encodes a valid secp256k1 server public key, rather than silently
/// leaving a malformed `?pubkey=` template unsigned.
fn validate_turn_pubkey_templates(spec: &str) -> Result<()> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    for entry in trimmed.split(';') {
        let entry = entry.trim();
        if rings_turn::has_turn_pubkey_query_param(entry)
            && rings_turn::extract_turn_server_pubkey(entry).is_none()
        {
            return Err(Error::InvalidConfig(format!(
                "invalid TURN server pubkey in '{entry}'"
            )));
        }
    }
    Ok(())
}

/// Derive session-scoped ICE/TURN credentials from `spec` using `delegatee_key`, failing closed
/// if any `?pubkey=` template is malformed or if credential signing fails.
fn derive_session_ice_servers(spec: &str, delegatee_key: &DelegateeKey) -> Result<String> {
    validate_turn_pubkey_templates(spec)?;
    delegatee_key
        .sign_ice_servers(spec)
        .map_err(Error::CoreError)
}

/// Derive static account-scoped ICE/TURN credentials from `spec` using `account_key`, failing
/// closed if any `?pubkey=` template is malformed or if credential signing fails.
fn derive_account_ice_servers(spec: &str, account_key: &SigningSecretKey) -> Result<String> {
    validate_turn_pubkey_templates(spec)?;
    account_key.sign_ice_servers(spec).map_err(Error::CoreError)
}

fn instantiate_builtin_transport(
    config: &TransportNetworkConfig,
    signed_credentials: &str,
) -> Result<SharedTransport> {
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    {
        config
            .try_build_dummy_transport(Some(signed_credentials))
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }
    #[cfg(not(all(feature = "dummy", not(target_family = "wasm"))))]
    {
        config
            .try_build_shared_transport(Some(signed_credentials))
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }
}

/// Trait defining the network parameters that customize and instantiate a node's transport (#1291).
///
/// From first principles, every interaction between a node (`Processor`, `ProcessorBuilder`,
/// `ProcessorConfig`) and its network parameters reduces to three orthogonal operations plus
/// optional config-descriptor projection:
/// 1. **Transport instantiation** ([`build_transport`](Self::build_transport)): validates the
///    medium's parameters (failing closed on malformed configuration) and constructs a
///    [`SharedTransport`] bound to the node's initial [`DelegateeKey`].
/// 2. **Advertised locator hint** ([`endpoint_hint`](Self::endpoint_hint)): returns an optional
///    external locator published in the node's online presence descriptor (`None` by default for
///    proximity ad-hoc media such as packet radio, Bluetooth ad-hoc, ultrasonic ad-hoc, or
///    unaddressed client nodes).
/// 3. **Two-phase session-key rotation**
///    ([`prepare_session_key_rotation`](Self::prepare_session_key_rotation)): phase 1 validates and
///    prepares any session-bound transport credentials **without mutating** `transport`, returning
///    an infallible [`SessionRotationCommit`] closure that phase 2 executes only after all pure
///    validations and durable disk persistence succeed.
pub trait NodeNetworkParameters: Any + fmt::Debug + MaybeSendSync {
    /// Validate these network parameters and instantiate a [`SharedTransport`] bound to
    /// `delegatee_key`.
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport>;

    /// Return the external endpoint or locator hint advertised in the node's online presence
    /// descriptor, if any.
    ///
    /// Defaults to `None` (appropriate for proximity ad-hoc media or nodes without a public locator).
    fn endpoint_hint(&self) -> Option<&str> {
        None
    }

    /// Phase 1 of two-phase session-key rotation: derive and validate transport state for
    /// `next_key` **without mutating** `transport`, returning an infallible phase-2 commit closure.
    ///
    /// # Concurrency & Lock-Ordering Contract
    ///
    /// Both `prepare_session_key_rotation` and the returned [`SessionRotationCommit`] closure run
    /// synchronously while `Processor`'s internal `SessionKeyRing` write lock is held:
    /// - Implementations **must** remain synchronous and non-blocking (never awaiting network I/O).
    /// - Implementations **must not** re-enter `Processor` methods that read or write the session
    ///   key ring (such as `Processor::signing_horizon` or `Processor::renew_delegatee_key`).
    /// - Phase 1 (`prepare_session_key_rotation`) **must not** mutate `transport` before returning,
    ///   and Phase 2 (`SessionRotationCommit`) **must** be infallible.
    ///
    /// Defaults to an empty no-op commit closure for media whose link layer does not carry
    /// session-signed credentials.
    fn prepare_session_key_rotation(
        &self,
        _transport: &SharedTransport,
        _next_key: &DelegateeKey,
    ) -> Result<SessionRotationCommit> {
        Ok(Box::new(|| {}))
    }

    /// Project these network parameters into a tagged [`NetworkConfigDescriptor`] for lossless
    /// serialization into [`ProcessorConfigSerialized`], returning `None` if this parameter type is
    /// purely programmatic.
    fn to_network_descriptor(&self) -> Option<NetworkConfigDescriptor> {
        None
    }
}

/// Implementation of [`NodeNetworkParameters`] for the built-in [`TransportNetworkConfig`].
impl NodeNetworkParameters for TransportNetworkConfig {
    /// Strictly derive session-signed traversal credentials from `delegatee_key` and construct the
    /// platform's [`SharedTransport`], failing closed if any ICE/TURN URL or server public key is
    /// malformed.
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
        let signed = derive_session_ice_servers(self.traversal_servers(), delegatee_key)?;
        instantiate_builtin_transport(self, &signed)
    }

    /// Return the configured external endpoint hint, if any.
    fn endpoint_hint(&self) -> Option<&str> {
        TransportNetworkConfig::endpoint_hint(self)
    }

    /// Re-sign the configured traversal server template with `next_key`, strictly parse the
    /// resulting ICE server list in phase 1 without mutating `transport`, and return an infallible
    /// phase-2 closure that atomically commits `(raw_credentials, parsed_ice_servers)`.
    fn prepare_session_key_rotation(
        &self,
        transport: &SharedTransport,
        next_key: &DelegateeKey,
    ) -> Result<SessionRotationCommit> {
        let signed = derive_session_ice_servers(self.traversal_servers(), next_key)?;
        TransportNetworkConfig::prepare_ice_server_rotation(transport, signed)
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }
}

/// Built-in WebRTC network parameters pairing [`TransportNetworkConfig`] with an explicit
/// [`TurnCredentialMode`] (`Delegatee` by default, or `Account` for non-expiring account-signed
/// TURN credentials).
#[derive(Clone, Debug)]
pub struct WebrtcNetworkParameters {
    config: TransportNetworkConfig,
    credential_mode: TurnCredentialMode,
}

impl WebrtcNetworkParameters {
    /// Create [`WebrtcNetworkParameters`] using default session-scoped [`TurnCredentialMode::Delegatee`].
    pub const fn new(config: TransportNetworkConfig) -> Self {
        Self {
            config,
            credential_mode: TurnCredentialMode::Delegatee,
        }
    }

    /// Override the [`TurnCredentialMode`] used to sign `?pubkey=` TURN server URLs.
    pub fn with_credential_mode(mut self, credential_mode: TurnCredentialMode) -> Self {
        self.credential_mode = credential_mode;
        self
    }

    /// Borrow the underlying [`TransportNetworkConfig`].
    pub const fn transport_config(&self) -> &TransportNetworkConfig {
        &self.config
    }

    /// Return the configured [`TurnCredentialMode`].
    pub const fn credential_mode(&self) -> &TurnCredentialMode {
        &self.credential_mode
    }
}

impl NodeNetworkParameters for WebrtcNetworkParameters {
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
        let signed = match &self.credential_mode {
            TurnCredentialMode::Delegatee => {
                derive_session_ice_servers(self.config.traversal_servers(), delegatee_key)?
            }
            TurnCredentialMode::Account(account_key) => {
                derive_account_ice_servers(self.config.traversal_servers(), account_key)?
            }
        };
        self.config
            .try_build_shared_transport(Some(&signed))
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }

    fn endpoint_hint(&self) -> Option<&str> {
        self.config.endpoint_hint()
    }

    fn prepare_session_key_rotation(
        &self,
        transport: &SharedTransport,
        next_key: &DelegateeKey,
    ) -> Result<SessionRotationCommit> {
        match &self.credential_mode {
            TurnCredentialMode::Delegatee => self
                .config
                .prepare_session_key_rotation(transport, next_key),
            TurnCredentialMode::Account(_) => Ok(Box::new(|| {})),
        }
    }

    fn to_network_descriptor(&self) -> Option<NetworkConfigDescriptor> {
        match &self.credential_mode {
            TurnCredentialMode::Delegatee => Some(webrtc_descriptor_from_config(&self.config)),
            TurnCredentialMode::Account(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebrtcDescriptorParams {
    #[serde(default)]
    ice_servers: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    external_address: Option<String>,
    #[serde(
        default,
        alias = "webrtc_udp_port_min",
        skip_serializing_if = "Option::is_none"
    )]
    udp_port_min: Option<u16>,
    #[serde(
        default,
        alias = "webrtc_udp_port_max",
        skip_serializing_if = "Option::is_none"
    )]
    udp_port_max: Option<u16>,
}

fn webrtc_descriptor_from_config(config: &TransportNetworkConfig) -> NetworkConfigDescriptor {
    let range = config.udp_port_range();
    let params = serde_json::to_value(WebrtcDescriptorParams {
        ice_servers: config.traversal_servers().to_owned(),
        external_address: config.endpoint_hint().map(str::to_owned),
        udp_port_min: range.map(UdpPortRange::min),
        udp_port_max: range.map(UdpPortRange::max),
    })
    .unwrap_or_else(|_| default_empty_network_params());
    NetworkConfigDescriptor {
        kind: "webrtc".to_string(),
        params,
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[derive(Clone, Debug)]
struct DummyNetworkParameters {
    config: TransportNetworkConfig,
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl NodeNetworkParameters for DummyNetworkParameters {
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
        let signed = derive_session_ice_servers(self.config.traversal_servers(), delegatee_key)?;
        self.config
            .try_build_dummy_transport(Some(&signed))
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }

    fn endpoint_hint(&self) -> Option<&str> {
        self.config.endpoint_hint()
    }

    fn prepare_session_key_rotation(
        &self,
        transport: &SharedTransport,
        next_key: &DelegateeKey,
    ) -> Result<SessionRotationCommit> {
        let signed = derive_session_ice_servers(self.config.traversal_servers(), next_key)?;
        TransportNetworkConfig::prepare_ice_server_rotation(transport, signed)
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }

    fn to_network_descriptor(&self) -> Option<NetworkConfigDescriptor> {
        let mut desc = webrtc_descriptor_from_config(&self.config);
        desc.kind = "dummy".to_string();
        Some(desc)
    }
}

fn network_transport_registry() -> &'static RwLock<BTreeMap<String, NetworkParametersFactory>> {
    static REGISTRY: OnceLock<RwLock<BTreeMap<String, NetworkParametersFactory>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut map: BTreeMap<String, NetworkParametersFactory> = BTreeMap::new();
        map.insert(
            "webrtc".to_string(),
            Arc::new(|params| {
                let decoded: WebrtcDescriptorParams = serde_json::from_value(params.clone())
                    .map_err(|err| {
                        Error::InvalidConfig(format!("invalid 'webrtc' network params: {err}"))
                    })?;
                let range = parse_udp_port_range(decoded.udp_port_min, decoded.udp_port_max)?;
                Ok(Arc::new(WebrtcNetworkParameters::new(
                    TransportNetworkConfig::new(
                        decoded.ice_servers,
                        decoded.external_address,
                        range,
                    ),
                )))
            }),
        );
        #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
        map.insert(
            "dummy".to_string(),
            Arc::new(|params| {
                let decoded: WebrtcDescriptorParams = serde_json::from_value(params.clone())
                    .map_err(|err| {
                        Error::InvalidConfig(format!("invalid 'dummy' network params: {err}"))
                    })?;
                let range = parse_udp_port_range(decoded.udp_port_min, decoded.udp_port_max)?;
                Ok(Arc::new(DummyNetworkParameters {
                    config: TransportNetworkConfig::new(
                        decoded.ice_servers,
                        decoded.external_address,
                        range,
                    ),
                }))
            }),
        );
        RwLock::new(map)
    })
}

/// Register a custom [`NodeNetworkParameters`] factory under `kind` so native `config.yaml`,
/// [`ProcessorConfigSerialized`], FFI, and browser WASM serialized configurations can instantiate
/// `network: { kind: "<kind>", params: { ... } }` (#1291).
///
/// Built-in transport kinds (`"webrtc"` and `"dummy"`) are reserved and cannot be overwritten,
/// and duplicate registrations for an already-registered custom `kind` fail with
/// [`Error::InvalidConfig`] rather than silently hijacking an existing factory.
pub fn register_network_transport_factory<F>(kind: impl Into<String>, factory: F) -> Result<()>
where F: Fn(&serde_json::Value) -> Result<Arc<dyn NodeNetworkParameters>> + Send + Sync + 'static {
    let kind = kind.into();
    let normalized = kind.trim();
    if normalized.is_empty() {
        return Err(Error::InvalidConfig(
            "network transport kind must not be empty".to_string(),
        ));
    }
    if matches!(normalized, "webrtc" | "dummy") {
        return Err(Error::InvalidConfig(format!(
            "cannot overwrite reserved built-in network transport kind '{normalized}'"
        )));
    }
    let mut guard = network_transport_registry()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    match guard.entry(normalized.to_owned()) {
        std::collections::btree_map::Entry::Occupied(_) => Err(Error::InvalidConfig(format!(
            "network transport kind '{normalized}' is already registered"
        ))),
        std::collections::btree_map::Entry::Vacant(vacant) => {
            vacant.insert(Arc::new(factory));
            Ok(())
        }
    }
}

/// Resolve a deserialized [`NetworkConfigDescriptor`] into a concrete [`NodeNetworkParameters`]
/// instance via the process-wide transport registry.
pub fn resolve_network_config_descriptor(
    descriptor: &NetworkConfigDescriptor,
) -> Result<Arc<dyn NodeNetworkParameters>> {
    let kind = descriptor.kind.trim();
    let factory = network_transport_registry()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(kind)
        .cloned()
        .ok_or_else(|| Error::InvalidConfig(format!("unknown network transport kind '{kind}'")))?;
    factory(&descriptor.params)
}

/// ProcessorConfig is usually serialized as json or yaml.
/// There is a `from_config` method in [ProcessorBuilder] used to initialize the Builder with a serialized ProcessorConfig.
#[derive(Clone, Debug)]
#[wasm_export]
pub struct ProcessorConfig {
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    pub(in crate::processor) network_id: u32,
    /// Abstract network parameters customizing the transport layer (#1291).
    pub(in crate::processor) network_parameters: Arc<dyn NodeNetworkParameters>,
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
    /// Optional HTTP(S) proxy for remote JSON-RPC endpoints on native targets.
    pub(crate) remote_rpc_proxy: Option<RemoteRpcProxy>,
    /// Epoch clock capability injected into this node's runtime subsystems.
    pub(in crate::processor) clock: SharedClock,
}

#[wasm_export]
impl ProcessorConfig {
    /// Creates a new `ProcessorConfig` instance without an external endpoint hint.
    pub fn new(
        network_id: u32,
        traversal_servers: String,
        delegatee_key: DelegateeKey,
        stabilize_interval: u64,
    ) -> Self {
        Self {
            network_id,
            network_parameters: Arc::new(TransportNetworkConfig::new(
                traversal_servers,
                None,
                None,
            )),
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
            remote_rpc_proxy: None,
            clock: Arc::new(SystemClock),
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

    /// Enable or disable onion relay advertisement on this node.
    ///
    /// When enabling relay while currently `Client`, upgrades the role to `Relay`; when disabling
    /// relay, resets the role to `Client`.
    pub fn advertise_onion_relay(mut self, enabled: bool) -> Self {
        self.onion_role = match (enabled, self.onion_role) {
            (true, OnionRole::Client) => OnionRole::Relay,
            (true, role) => role,
            (false, _) => OnionRole::Client,
        };
        self
    }
}

impl ProcessorConfig {
    /// Override the runtime family advertised in the online-node registry.
    pub fn online_node_type(mut self, node_type: OnlineNodeType) -> Self {
        self.online_node_type = node_type;
        self
    }

    /// Return the configured [`NodeNetworkParameters`].
    pub fn network_parameters(&self) -> &Arc<dyn NodeNetworkParameters> {
        &self.network_parameters
    }

    /// Inject custom [`NodeNetworkParameters`] to customize the transport layer (#1291).
    pub fn with_network_parameters<P>(mut self, params: P) -> Self
    where P: NodeNetworkParameters + 'static {
        self.network_parameters = Arc::new(params);
        self
    }

    /// Returns the validated local UDP port range when configured on a built-in UDP-capable
    /// transport (`TransportNetworkConfig` or `WebrtcNetworkParameters`), or `None` when no port
    /// range is set or when custom [`NodeNetworkParameters`] are injected (custom media manage
    /// their own local socket or radio channel allocation internally).
    pub fn udp_port_range(&self) -> Option<UdpPortRange> {
        let any = self.network_parameters.as_ref() as &dyn Any;
        if let Some(cfg) = any.downcast_ref::<TransportNetworkConfig>() {
            return cfg.udp_port_range();
        }
        if let Some(webrtc) = any.downcast_ref::<WebrtcNetworkParameters>() {
            return webrtc.transport_config().udp_port_range();
        }
        None
    }

    /// The longer heartbeat of the two registries this node publishes, online nodes and onion
    /// exits: the pause after which each of its registrations is published again.
    pub fn registry_heartbeat_interval(&self) -> Duration {
        self.online_node_heartbeat_interval
            .max(self.onion_exit_heartbeat_interval)
    }

    /// Sets the onion symbols this process registers (#834 D2).
    pub fn onion_role(mut self, role: OnionRole<OnionExitOffer>) -> Self {
        self.onion_role = role;
        self
    }

    /// Validate this config's onion role ([`validate_onion_role_config`]) before any runtime
    /// effect, as the builder does at build time.
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    pub(crate) fn validate_onion_role(&self) -> Result<()> {
        validate_onion_role_config(self.advertise_presence, &self.onion_role)
    }

    /// Sets runtime-local final-destination quotas keyed by verified origin and logical lane.
    pub fn origin_quota(mut self, config: OriginQuotaConfig) -> Self {
        self.origin_quota = config;
        self
    }

    /// Route native remote JSON-RPC requests (`nodeDid`, `answerOffer`, `acceptAnswer`) through
    /// `proxy` instead of connecting directly.
    pub fn remote_rpc_proxy(mut self, proxy: RemoteRpcProxy) -> Self {
        self.remote_rpc_proxy = Some(proxy);
        self
    }

    /// Return the [`SharedClock`] capability configured for this node.
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    /// Inject a custom [`SharedClock`] capability for deterministic node time.
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }
}

impl FromStr for ProcessorConfig {
    type Err = Error;
    /// Reveal config from serialized string (with YAML error source snippets disabled so secret
    /// `delegatee_key` lines are never echoed in formatted error messages).
    fn from_str(ser: &str) -> Result<Self> {
        let mut options = serde_saphyr::Options::default();
        options.with_snippet = false;
        Ok(serde_saphyr::from_str_with_options::<ProcessorConfig>(
            ser, options,
        )?)
    }
}

/// `ProcessorConfigSerialized` is a serialized version of `ProcessorConfig`.
/// Instead of storing the `DelegateeKey` instance, it stores the dumped string representation of
/// the delegatee key.
#[derive(Serialize, Deserialize, Clone)]
#[wasm_export]
pub struct ProcessorConfigSerialized {
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    pub(crate) network_id: u32,
    /// Traversal/relay server specification string (serialized under the `ice_servers` key for
    /// config file compatibility; defaults to empty when `network` selects a non-ICE transport).
    #[serde(default)]
    pub(crate) ice_servers: String,
    /// Optional external endpoint locator advertised by this node.
    #[serde(default)]
    pub(crate) external_address: Option<String>,
    /// Inclusive lower local UDP port bound.
    #[serde(default, alias = "webrtc_udp_port_min")]
    pub(crate) udp_port_min: Option<u16>,
    /// Inclusive upper local UDP port bound.
    #[serde(default, alias = "webrtc_udp_port_max")]
    pub(crate) udp_port_max: Option<u16>,
    /// Optional serde-extensible tagged transport descriptor (`{ kind, params }`).
    /// When present, overrides the legacy `ice_servers` / `external_address` / `udp_port_*`
    /// WebRTC fields and resolves through [`resolve_network_config_descriptor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) network: Option<NetworkConfigDescriptor>,
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
    /// Optional intermediate `invoke` offer advertised by this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) onion_invoke_offer: Option<crate::onion::OnionInvokeOffer>,
    /// Optional HTTP(S) proxy URL for remote JSON-RPC endpoints on native targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote_rpc_proxy_url: Option<String>,
}

pub(crate) fn parse_udp_port_range(
    min: Option<u16>,
    max: Option<u16>,
) -> Result<Option<UdpPortRange>> {
    match (min, max) {
        (None, None) => Ok(None),
        (Some(min), Some(max)) => UdpPortRange::new(min, max).map(Some).map_err(Error::from),
        (min, max) => Err(Error::IncompleteUdpPortRange { min, max }),
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

/// Validate that the onion role can be published and served.
///
/// - `relay ∈ Σ_n ⇒ advertise_presence`, because the relay registration lives in the online-node
///   descriptor.
/// - Every offered exit service is one this node's runtime interprets
///   ([`OnionExitOffer::uninterpretable_service`]), so no descriptor names a service the node
///   cannot evaluate.
///
/// `Σ_n ≠ ∅ ⇒ relay ∈ Σ_n` needs no check: [`OnionRole`] has no rung that registers a symbol
/// without `relay`.
pub(in crate::processor) fn validate_onion_role_config(
    advertise_presence: bool,
    onion_role: &OnionRole<OnionExitOffer>,
) -> Result<()> {
    if let Some(service) = onion_role
        .exit()
        .and_then(OnionExitOffer::uninterpretable_service)
    {
        return Err(Error::UninterpretableOnionService {
            service: service.clone(),
        });
    }
    #[cfg(rings_browser)]
    if let Some(offer) = onion_role.exit() {
        offer.policy().validate_browser_targets()?;
    }
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
        let (ice_servers, external_address, udp_port_min, udp_port_max, network) = if let Some(
            transport_config,
        ) =
            (ins.network_parameters.as_ref() as &dyn Any).downcast_ref::<TransportNetworkConfig>()
        {
            let udp_port_range = transport_config.udp_port_range();
            (
                transport_config.traversal_servers().to_owned(),
                ins.network_parameters.endpoint_hint().map(str::to_owned),
                udp_port_range.map(UdpPortRange::min),
                udp_port_range.map(UdpPortRange::max),
                None,
            )
        } else if let Some(descriptor) = ins.network_parameters.to_network_descriptor() {
            (String::new(), None, None, None, Some(descriptor))
        } else {
            return Err(Error::InvalidConfig(
                "cannot losslessly serialize custom NodeNetworkParameters into ProcessorConfigSerialized \
                 (implement `to_network_descriptor` and register via `register_network_transport_factory`)"
                    .to_string(),
            ));
        };
        Ok(Self {
            network_id: ins.network_id,
            ice_servers,
            external_address,
            udp_port_min,
            udp_port_max,
            network,
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
            onion_invoke_offer: ins.onion_role.invoke().cloned(),
            remote_rpc_proxy_url: ins
                .remote_rpc_proxy
                .as_ref()
                .map(|proxy| proxy.as_str().to_owned()),
        })
    }
}

impl TryFrom<ProcessorConfigSerialized> for ProcessorConfig {
    type Error = Error;
    fn try_from(ins: ProcessorConfigSerialized) -> Result<Self> {
        let network_parameters: Arc<dyn NodeNetworkParameters> = match ins.network.as_ref() {
            Some(descriptor) => {
                if !ins.ice_servers.trim().is_empty()
                    || ins.external_address.is_some()
                    || ins.udp_port_min.is_some()
                    || ins.udp_port_max.is_some()
                {
                    return Err(Error::InvalidConfig(
                        "cannot specify both `network` and legacy flat WebRTC fields (`ice_servers`, `external_address`, `udp_port_min`, `udp_port_max`)"
                            .to_string(),
                    ));
                }
                resolve_network_config_descriptor(descriptor)?
            }
            None => {
                let udp_port_range = parse_udp_port_range(ins.udp_port_min, ins.udp_port_max)?;
                Arc::new(TransportNetworkConfig::new(
                    ins.ice_servers,
                    ins.external_address,
                    udp_port_range,
                ))
            }
        };
        let remote_rpc_proxy = ins
            .remote_rpc_proxy_url
            .as_deref()
            .map(RemoteRpcProxy::parse)
            .transpose()?;
        let online_node_heartbeat_interval =
            Duration::from_secs(ins.online_node_heartbeat_interval_secs);
        let online_node_ttl = Duration::from_secs(ins.online_node_ttl_secs);
        let onion_exit_heartbeat_interval =
            Duration::from_secs(ins.onion_exit_heartbeat_interval_secs);
        let onion_exit_ttl = Duration::from_secs(ins.onion_exit_ttl_secs);
        Ok(Self {
            network_id: ins.network_id,
            network_parameters,
            delegatee_key: DelegateeKey::from_str(&ins.delegatee_key)?,
            stabilize_interval: Duration::from_secs(ins.stabilize_interval),
            online_node_heartbeat_interval,
            online_node_ttl,
            online_node_type: ins.online_node_type,
            advertise_presence: ins.advertise_presence,
            dht_virtual_nodes: ins.dht_virtual_nodes,
            origin_quota: ins.origin_quota,
            onion_role: {
                let role = OnionRole::from_flags(
                    ins.advertise_onion_relay,
                    ins.advertise_onion_exit,
                    ins.onion_exit_services,
                    ins.onion_exit_policy,
                )?;
                match ins.onion_invoke_offer {
                    Some(invoke) => {
                        if !ins.advertise_onion_relay {
                            return Err(Error::InvalidConfig(
                                "onion_invoke_offer requires advertise_onion_relay".to_string(),
                            ));
                        }
                        role.with_invoke(invoke)
                    }
                    None => role,
                }
            },
            onion_exit_heartbeat_interval,
            onion_exit_ttl,
            remote_rpc_proxy,
            clock: Arc::new(SystemClock),
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
