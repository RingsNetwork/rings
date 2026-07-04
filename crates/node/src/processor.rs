#![warn(missing_docs)]

//! Processor of rings-node rpc server.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use rings_core::chunk::ReassemblyLimits;
use rings_core::dht::Did;
use rings_core::dht::EntryStorage;
use rings_core::dht::DEFAULT_FINGER_TABLE_SIZE;
use rings_core::ecc::PublicKey;
use rings_core::ecc::SecretKey;
use rings_core::measure::MeasureImpl;
use rings_core::measure::PeerMeasurement;
use rings_core::message::e2e;
use rings_core::message::e2e::E2eHandshakeRequest;
use rings_core::message::e2e::E2eHandshakeResponse;
use rings_core::message::e2e::E2eStreamDecryptor;
use rings_core::message::e2e::E2eStreamFrame;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::Message;
use rings_core::prelude::uuid;
use rings_core::storage::MemStorage;
use rings_core::swarm::Swarm;
use rings_core::swarm::SwarmBuilder;
use rings_core::utils::get_epoch_ms;
use rings_rpc::protos::rings_node::*;
use rings_transport::webrtc_config::WebrtcUdpPortRange;
use serde::Deserialize;
use serde::Serialize;

use crate::consts::DATA_REDUNDANT;
use crate::error::Error;
use crate::error::Result;
use crate::measure::PeriodicMeasure;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;
use crate::prelude::entry;
use crate::prelude::wasm_export;
use crate::prelude::ChordStorageInterface;
use crate::prelude::ChordStorageInterfaceCacheChecker;
use crate::prelude::SessionSk;
use crate::registration::default_advertise_presence;
use crate::registration::default_online_node_heartbeat_interval_secs;
use crate::registration::default_online_node_ttl_secs;
use crate::registration::default_online_node_type;
use crate::registration::sleep_registration_interval;
use crate::registration::validate_online_node_registration_timing;
use crate::registration::OnlineNodeRegistration;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;

/// ProcessorConfig is usually serialized as json or yaml.
/// There is a `from_config` method in [ProcessorBuilder] used to initialize the Builder with a serialized ProcessorConfig.
#[derive(Clone, Debug)]
#[wasm_export]
pub struct ProcessorConfig {
    /// The network_id is used to distinguish different networks.
    /// Use 1 for main network.
    network_id: u32,
    /// ICE servers for webrtc
    ice_servers: String,
    /// External address for webrtc
    external_address: Option<String>,
    /// Inclusive lower native WebRTC UDP port bound.
    webrtc_udp_port_min: Option<u16>,
    /// Inclusive upper native WebRTC UDP port bound.
    webrtc_udp_port_max: Option<u16>,
    /// [SessionSk].
    session_sk: SessionSk,
    /// Stabilization interval.
    stabilize_interval: Duration,
    /// Online-node registry heartbeat interval.
    online_node_heartbeat_interval: Duration,
    /// Online-node registry descriptor TTL.
    online_node_ttl: Duration,
    /// Runtime family advertised in the online-node registry.
    online_node_type: OnlineNodeType,
    /// Whether listen() advertises this node's presence.
    advertise_presence: bool,
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
        }
    }

    /// Return associated [SessionSk].
    pub fn session_sk(&self) -> SessionSk {
        self.session_sk.clone()
    }
}

impl ProcessorConfig {
    /// Returns the validated native WebRTC UDP port range, when configured.
    pub fn webrtc_udp_port_range(&self) -> Result<Option<WebrtcUdpPortRange>> {
        parse_webrtc_udp_port_range(self.webrtc_udp_port_min, self.webrtc_udp_port_max)
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
        validate_online_node_registration_timing(
            ins.advertise_presence,
            online_node_heartbeat_interval,
            online_node_ttl,
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

/// ProcessorBuilder is used to initialize a [Processor] instance.
pub struct ProcessorBuilder {
    network_id: u32,
    ice_servers: String,
    external_address: Option<String>,
    webrtc_udp_port_range: Option<WebrtcUdpPortRange>,
    session_sk: SessionSk,
    storage: Option<EntryStorage>,
    measure: Option<MeasureImpl>,
    stabilize_interval: Duration,
    online_node_heartbeat_interval: Duration,
    online_node_ttl: Duration,
    online_node_type: OnlineNodeType,
    advertise_presence: bool,
    registration_tasks: Vec<Arc<dyn RegistrationTask>>,
    dht_finger_table_size: usize,
    reassembly_limits: ReassemblyLimits,
}

/// Processor for rings-node rpc server.
///
/// Cloning shares the same node handle; publishes from any clone are serialized
/// against each other.
#[derive(Clone)]
pub struct Processor {
    /// a swarm instance
    pub swarm: Arc<Swarm>,
    /// Same session key held by the swarm transport; kept here for node-layer descriptor signing.
    session_sk: SessionSk,
    stabilize_interval: Duration,
    online_node_registration: OnlineNodeRegistration,
    registration_tasks: Vec<Arc<dyn RegistrationTask>>,
}

impl ProcessorBuilder {
    /// initialize a [ProcessorBuilder] with a serialized [ProcessorConfig].
    pub fn from_serialized(config: &str) -> Result<Self> {
        let config =
            serde_yaml::from_str::<ProcessorConfig>(config).map_err(Error::SerdeYamlError)?;
        Self::from_config(&config)
    }

    /// initialize a [ProcessorBuilder] with a [ProcessorConfig].
    pub fn from_config(config: &ProcessorConfig) -> Result<Self> {
        validate_online_node_registration_timing(
            config.advertise_presence,
            config.online_node_heartbeat_interval,
            config.online_node_ttl,
        )?;
        Ok(Self {
            network_id: config.network_id,
            ice_servers: config.ice_servers.clone(),
            external_address: config.external_address.clone(),
            webrtc_udp_port_range: config.webrtc_udp_port_range()?,
            session_sk: config.session_sk.clone(),
            storage: None,
            measure: None,
            stabilize_interval: config.stabilize_interval,
            online_node_heartbeat_interval: config.online_node_heartbeat_interval,
            online_node_ttl: config.online_node_ttl,
            online_node_type: config.online_node_type.clone(),
            advertise_presence: config.advertise_presence,
            registration_tasks: Vec::new(),
            dht_finger_table_size: DEFAULT_FINGER_TABLE_SIZE,
            reassembly_limits: ReassemblyLimits::production(),
        })
    }

    /// Set the storage for the processor.
    pub fn storage(mut self, storage: EntryStorage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Set the measure for the processor.
    pub fn measure(mut self, implement: PeriodicMeasure) -> Self {
        self.measure = Some(Arc::new(implement));
        self
    }

    /// Set the number of DHT finger-table slots for the processor's swarm.
    pub fn dht_finger_table_size(mut self, size: usize) -> Self {
        self.dht_finger_table_size = size;
        self
    }

    /// Set inbound chunk reassembly limits for the processor's swarm.
    pub fn reassembly_limits(mut self, limits: ReassemblyLimits) -> Self {
        self.reassembly_limits = limits;
        self
    }

    /// Set the runtime family advertised in the online-node registry.
    pub fn online_node_type(mut self, node_type: OnlineNodeType) -> Self {
        self.online_node_type = node_type;
        self
    }

    /// Set whether listen() advertises this node's presence.
    pub fn advertise_presence(mut self, advertise: bool) -> Self {
        self.advertise_presence = advertise;
        self
    }

    /// Add a custom periodic registration task.
    pub fn registration_task<T>(mut self, task: T) -> Self
    where T: RegistrationTask + 'static {
        self.registration_tasks.push(Arc::new(task));
        self
    }

    /// Add an already shared custom periodic registration task.
    pub fn shared_registration_task(mut self, task: Arc<dyn RegistrationTask>) -> Self {
        self.registration_tasks.push(task);
        self
    }

    /// Build the [Processor].
    pub fn build(self) -> Result<Processor> {
        self.session_sk
            .session()
            .verify_self()
            .map_err(|e| Error::VerifyError(e.to_string()))?;

        let storage = self.storage.unwrap_or_else(|| Box::new(MemStorage::new()));
        let endpoint_hint = self.external_address.clone();

        let session_sk = self.session_sk.clone();
        let online_node_registration = OnlineNodeRegistration::new(
            self.online_node_heartbeat_interval,
            self.online_node_ttl,
            self.online_node_type,
            endpoint_hint,
        );
        let mut registration_tasks = self.registration_tasks;
        if self.advertise_presence {
            online_node_registration.validate_enabled_schedule()?;
            registration_tasks.push(Arc::new(online_node_registration.clone()));
        }

        let mut swarm_builder =
            SwarmBuilder::new(self.network_id, &self.ice_servers, storage, self.session_sk);
        swarm_builder = swarm_builder.dht_storage_redundancy(DATA_REDUNDANT);
        swarm_builder = swarm_builder.dht_finger_table_size(self.dht_finger_table_size);
        swarm_builder = swarm_builder.reassembly_limits(self.reassembly_limits);

        if let Some(external_address) = self.external_address {
            swarm_builder = swarm_builder.external_address(external_address);
        }
        if let Some(range) = self.webrtc_udp_port_range {
            swarm_builder = swarm_builder.webrtc_udp_port_range(range);
        }

        if let Some(measure) = self.measure {
            swarm_builder = swarm_builder.measure(measure);
        }
        let swarm = Arc::new(swarm_builder.build());

        Ok(Processor {
            swarm,
            session_sk,
            stabilize_interval: self.stabilize_interval,
            online_node_registration,
            registration_tasks,
        })
    }
}

impl Processor {
    /// Get current did
    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    pub(crate) fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    fn registration_context(&self) -> RegistrationContext<'_> {
        RegistrationContext::new(self)
    }

    #[cfg(all(test, feature = "node"))]
    fn online_node_descriptor_at(&self, now_ms: u128) -> Result<OnlineNodeDescriptor> {
        self.online_node_registration
            .descriptor_at(&self.registration_context(), now_ms)
    }

    fn online_node_descriptors_from_entry(entry: &entry::Entry) -> Vec<OnlineNodeDescriptor> {
        OnlineNodeRegistration::descriptors_from_entry(entry)
    }

    #[cfg(all(test, feature = "node"))]
    fn online_node_registry_entry(descriptors: Vec<OnlineNodeDescriptor>) -> Result<entry::Entry> {
        let data = descriptors
            .into_iter()
            .map(|descriptor| descriptor.encode().map_err(Error::CoreError))
            .collect::<Result<Vec<_>>>()?;

        Ok(entry::Entry::new(
            entry::Entry::gen_did(ONLINE_NODES_TOPIC)?,
            data,
            entry::EntryKind::Data,
        ))
    }

    /// Publish this node's signed online descriptor to the online-node registry.
    pub async fn publish_online_node_descriptor(&self) -> Result<OnlineNodeDescriptor> {
        self.online_node_registration
            .publish_descriptor(&self.registration_context())
            .await
    }

    /// List signed online-node descriptors from the registry.
    pub async fn lookup_online_nodes(
        &self,
        include_expired: bool,
    ) -> Result<Vec<OnlineNodeDescriptor>> {
        let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;

        self.storage_fetch(entry_key).await?;
        let Some(entry) = self.storage_check_cache(entry_key).await else {
            return Ok(vec![]);
        };

        let descriptors = Self::online_node_descriptors_from_entry(&entry)
            .into_iter()
            .filter(|descriptor| descriptor.matches_network(self.swarm.network_id()));

        Ok(OnlineNodeDescriptor::latest_valid_by_did(
            descriptors,
            get_epoch_ms(),
            include_expired,
        ))
    }

    async fn registration_task_daemon(&self, task: &dyn RegistrationTask) {
        loop {
            if let Err(error) = task.register_once(&self.registration_context()).await {
                tracing::warn!("Failed to run {} registration task: {error:?}", task.name());
            }
            if let Err(error) = sleep_registration_interval(task.interval()).await {
                tracing::warn!(
                    "Stopping {} registration task after timer error: {error:?}",
                    task.name()
                );
                return;
            }
        }
    }

    async fn registration_daemons(&self) {
        join_all(
            self.registration_tasks
                .iter()
                .map(|task| self.registration_task_daemon(task.as_ref())),
        )
        .await;
    }

    /// Run stabilization and node registration tasks until this future is dropped or aborted.
    ///
    /// This is a long-running task; do not await completion as a readiness signal.
    pub async fn listen(&self) {
        let stabilizer = self.swarm.stabilizer();
        let stabilizer = Arc::new(stabilizer);
        if self.registration_tasks.is_empty() {
            stabilizer.wait(self.stabilize_interval).await;
        } else {
            let _ = futures::future::join(
                stabilizer.wait(self.stabilize_interval),
                self.registration_daemons(),
            )
            .await;
        }
    }

    /// Connect peer with web3 did.
    /// There are 3 peers: PeerA, PeerB, PeerC.
    /// 1. PeerA has a connection with PeerB.
    /// 2. PeerC has a connection with PeerB.
    /// 3. PeerC can connect PeerA with PeerA's web3 address.
    pub async fn connect_with_did(&self, did: Did) -> Result<()> {
        self.swarm.connect(did).await.map_err(Error::ConnectError)?;
        Ok(())
    }

    /// Disconnect a peer with web3 did.
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        self.swarm
            .disconnect(did)
            .await
            .map_err(Error::CloseConnectionError)
    }

    /// Send custom message to a did.
    pub async fn send_message(&self, destination: Did, msg: &[u8]) -> Result<uuid::Uuid> {
        tracing::info!("send_message, message size: {:?}", msg.len());

        let msg = Message::custom(msg).map_err(Error::SendMessage)?;

        self.swarm
            .send_message(msg, destination)
            .await
            .map_err(Error::SendMessage)
    }

    /// Send an E2E handshake request to a DID.
    ///
    /// The negotiated key is the peer's account/identity secp256k1 key, not
    /// the ephemeral session key.
    pub async fn send_e2e_handshake(&self, destination: Did) -> Result<uuid::Uuid> {
        let public_key = self.swarm.account_pubkey().map_err(Error::SendMessage)?;
        self.swarm
            .send_message(
                Message::E2eHandshakeRequest(E2eHandshakeRequest::new(public_key)),
                destination,
            )
            .await
            .map_err(Error::SendMessage)
    }

    /// Send an ElGamal-encrypted E2E message to a DID with a verified recipient key.
    ///
    /// Returns the stream id shared by all emitted E2E stream frames.
    pub async fn send_e2e_message(
        &self,
        destination: Did,
        recipient_public_key: PublicKey<33>,
        msg: &[u8],
    ) -> Result<uuid::Uuid> {
        self.send_e2e_message_with_frame_len(
            destination,
            recipient_public_key,
            msg,
            e2e::DEFAULT_E2E_PLAINTEXT_FRAME_LEN,
        )
        .await
    }

    /// Send an ElGamal-encrypted E2E stream with an explicit plaintext frame size.
    ///
    /// Returns the stream id shared by all emitted E2E stream frames.
    pub async fn send_e2e_message_with_frame_len(
        &self,
        destination: Did,
        recipient_public_key: PublicKey<33>,
        msg: &[u8],
        max_plaintext_frame_len: usize,
    ) -> Result<uuid::Uuid> {
        e2e::ensure_public_key_matches_did(recipient_public_key, destination)
            .map_err(Error::SendMessage)?;
        let sender_public_key = self.swarm.account_pubkey().map_err(Error::SendMessage)?;
        let stream_id = uuid::Uuid::new_v4();
        let frames = e2e::encrypt_stream_frames(
            msg,
            stream_id,
            sender_public_key,
            recipient_public_key,
            max_plaintext_frame_len,
        )
        .map_err(Error::SendMessage)?
        .collect::<rings_core::error::Result<Vec<_>>>()
        .map_err(Error::SendMessage)?;

        for frame in frames {
            self.swarm
                .send_message(Message::E2eStreamFrame(frame), destination)
                .await
                .map_err(Error::SendMessage)?;
        }

        Ok(stream_id)
    }

    /// Verify an E2E handshake request and return the requester's identity public key.
    pub fn verify_e2e_handshake_request(
        &self,
        requester: Did,
        request: &E2eHandshakeRequest,
    ) -> Result<PublicKey<33>> {
        request
            .verify_requester(requester)
            .map_err(Error::CoreError)?;
        Ok(request.requester_public_key)
    }

    /// Verify an E2E handshake response and return the responder's identity public key.
    pub fn verify_e2e_handshake_response(
        &self,
        responder: Did,
        response: &E2eHandshakeResponse,
    ) -> Result<PublicKey<33>> {
        response
            .verify_responder(responder)
            .map_err(Error::CoreError)?;
        Ok(response.responder_public_key)
    }

    /// Create an E2E stream decryptor with this node's identity/signing secret key.
    ///
    /// The ciphertext is encrypted to the DID/account key negotiated by the
    /// handshake. A session private key cannot decrypt it unless the session key
    /// is also the account key, so callers must supply the local identity key
    /// explicitly.
    pub fn e2e_stream_decryptor(
        &self,
        expected_sender: Did,
        stream_id: e2e::E2eStreamId,
        recipient_identity_key: SecretKey,
    ) -> Result<E2eStreamDecryptor> {
        e2e::ensure_public_key_matches_did(recipient_identity_key.pubkey(), self.did())
            .map_err(Error::CoreError)?;
        Ok(E2eStreamDecryptor::new(
            stream_id,
            expected_sender,
            recipient_identity_key,
        ))
    }

    /// Decrypt one E2E stream frame with an already-created stream decryptor.
    pub fn decrypt_e2e_stream_frame(
        &self,
        decryptor: &mut E2eStreamDecryptor,
        frame: &E2eStreamFrame,
    ) -> Result<Vec<u8>> {
        decryptor.decrypt_next(frame).map_err(Error::CoreError)
    }

    /// Send a namespaced [`Envelope`](crate::extension::ext::Envelope) to a did over the
    /// P2P transport (the wire codec
    /// of the extension layer). `send_envelope : (Did, Envelope) → IO TxId`.
    pub async fn send_envelope(
        &self,
        destination: Did,
        envelope: &crate::extension::ext::Envelope,
    ) -> Result<uuid::Uuid> {
        let msg_bytes = envelope.encode()?;
        self.send_message(destination, &msg_bytes).await
    }

    /// check local cache of dht
    pub async fn storage_check_cache(&self, entry_key: Did) -> Option<entry::Entry> {
        self.swarm.storage_check_cache(entry_key).await
    }

    /// Fetch an entry from DHT storage
    pub async fn storage_fetch(&self, entry_key: Did) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_fetch(&self.swarm, entry_key)
            .await
            .map_err(Error::EntryError)
    }

    /// Store an entry on DHT storage
    pub async fn storage_store(&self, entry: entry::Entry) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_store(&self.swarm, entry)
            .await
            .map_err(Error::EntryError)
    }

    /// Append data to an entry on DHT storage
    pub async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_append_data(
            &self.swarm,
            topic,
            data,
        )
        .await
        .map_err(Error::EntryError)
    }

    /// Touch data in an entry on DHT storage, moving existing equal payloads to the end.
    pub async fn storage_touch_data(&self, topic: &str, data: Encoded) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_touch_data(
            &self.swarm,
            topic,
            data,
        )
        .await
        .map_err(Error::EntryError)
    }

    /// Tombstone observed data in an entry on DHT storage.
    pub async fn storage_tombstone_data(&self, topic: &str, data: Encoded) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_tombstone_data(
            &self.swarm,
            topic,
            data,
        )
        .await
        .map_err(Error::EntryError)
    }

    /// Return local measurement counters for a peer, if observed.
    pub async fn peer_measurement(&self, did: Did) -> Option<PeerMeasurement> {
        self.swarm.peer_measurement(did).await
    }

    /// Return observed local measurement counters for all connected peers.
    pub async fn peer_measurements(&self) -> Vec<PeerMeasurement> {
        let mut measurements = join_all(
            self.swarm
                .peer_dids()
                .into_iter()
                .map(|did| self.peer_measurement(did)),
        )
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        measurements.sort_by_key(|measurement| measurement.did);
        measurements
    }

    /// register service
    pub async fn register_service(&self, name: &str) -> Result<()> {
        let encoded_did = self
            .did()
            .to_string()
            .encode()
            .map_err(Error::ServiceRegisterError)?;
        self.storage_touch_data(name, encoded_did)
            .await
            .map_err(|error| match error {
                Error::EntryError(error) => Error::ServiceRegisterError(error),
                error => error,
            })
    }

    /// get node info
    pub async fn get_node_info(&self) -> Result<NodeInfoResponse> {
        Ok(NodeInfoResponse {
            version: crate::util::build_version(),
            swarm: Some(self.swarm.inspect().await.into()),
        })
    }
}

#[cfg(test)]
#[cfg(feature = "node")]
mod test {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::time::Duration;
    use std::time::Instant;

    use rings_core::dht::Chord;
    use rings_core::dht::PeerRingAction;
    use rings_core::dht::PeerRingRemoteAction;
    use rings_core::storage::MemStorage;
    use rings_core::swarm::callback::SwarmCallback;
    use rings_core::swarm::callback::SwarmEvent;
    use rings_rpc::method::Method;
    use rings_transport::core::transport::WebrtcConnectionState;
    use tokio::sync::Mutex as AsyncTestMutex;
    use tokio::sync::Notify;

    use super::*;
    use crate::online::OnlineNodeDescriptorBody;
    use crate::prelude::*;
    use crate::provider::Provider;
    use crate::tests::native::prepare_processor;

    // Native WebRTC tests share process-global ICE/UDP resources and timing-sensitive
    // connection callbacks; run them serially so one test's candidates or callbacks
    // cannot add pressure to another test's handshake.
    static NETWORK_TEST_LOCK: OnceLock<AsyncTestMutex<()>> = OnceLock::new();

    #[test]
    fn webrtc_udp_port_range_absent_by_default() {
        let range = parse_webrtc_udp_port_range(None, None);

        assert!(matches!(range, core::result::Result::Ok(None)));
    }

    #[test]
    fn webrtc_udp_port_range_accepts_valid_bounds() {
        let range = parse_webrtc_udp_port_range(Some(49160), Some(49200));

        assert!(matches!(
            range,
            Ok(Some(range)) if range.min() == 49160 && range.max() == 49200
        ));
    }

    #[test]
    fn webrtc_udp_port_range_rejects_partial_bounds() {
        let range = parse_webrtc_udp_port_range(Some(49160), None);

        assert!(matches!(
            range,
            Err(Error::IncompleteWebrtcUdpPortRange {
                min: Some(49160),
                max: None
            })
        ));
    }

    #[test]
    fn webrtc_udp_port_range_rejects_zero_bound() {
        let range = parse_webrtc_udp_port_range(Some(0), Some(49200));

        assert!(matches!(
            range,
            Err(Error::InvalidWebrtcUdpPortRange(
                rings_transport::webrtc_config::WebrtcUdpPortRangeError::ZeroBound {
                    min: 0,
                    max: 49200
                }
            ))
        ));
    }

    #[test]
    fn webrtc_udp_port_range_rejects_inverted_bounds() {
        let range = parse_webrtc_udp_port_range(Some(49200), Some(49160));

        assert!(matches!(
            range,
            Err(Error::InvalidWebrtcUdpPortRange(
                rings_transport::webrtc_config::WebrtcUdpPortRangeError::Inverted {
                    min: 49200,
                    max: 49160
                }
            ))
        ));
    }

    #[test]
    fn online_node_timing_requires_heartbeat_interval_less_than_ttl_when_enabled() {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let serialized = ProcessorConfigSerialized::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            session_sk.dump().unwrap(),
            3,
        )
        .online_node_heartbeat_interval_secs(90)
        .online_node_ttl_secs(30);

        assert!(matches!(
            ProcessorConfig::try_from(serialized),
            Err(Error::InvalidConfig(message))
                if message.contains("online_node_heartbeat_interval")
                    && message.contains("online_node_ttl")
        ));
    }

    #[test]
    fn presence_advertisement_can_be_disabled() {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let serialized = ProcessorConfigSerialized::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            session_sk.dump().unwrap(),
            3,
        )
        .online_node_heartbeat_interval_secs(90)
        .online_node_ttl_secs(30)
        .advertise_presence(false);

        let config = ProcessorConfig::try_from(serialized).unwrap();
        let builder = ProcessorBuilder::from_config(&config).unwrap();

        assert!(!builder.advertise_presence);
        assert!(builder.registration_tasks.is_empty());
    }

    #[test]
    fn presence_advertisement_is_enabled_by_default() {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let serialized = ProcessorConfigSerialized::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            session_sk.dump().unwrap(),
            3,
        );

        let config = ProcessorConfig::try_from(serialized).unwrap();
        let builder = ProcessorBuilder::from_config(&config).unwrap();

        assert!(builder.advertise_presence);
    }

    #[tokio::test]
    async fn custom_registration_task_publishes_through_shared_dht_sink() -> Result<()> {
        let topic = "custom_registration_task";
        let value = "custom-value"
            .to_string()
            .encode()
            .map_err(Error::CoreError)?;
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let config = ProcessorConfig::try_from(
            ProcessorConfigSerialized::new(
                0,
                "stun://stun.l.google.com:19302".to_string(),
                session_sk.dump().unwrap(),
                3,
            )
            .advertise_presence(false),
        )
        .unwrap();
        let processor = ProcessorBuilder::from_config(&config)
            .unwrap()
            .storage(Box::new(MemStorage::new()))
            .dht_finger_table_size(8)
            .registration_task(StaticRegistration::new(topic, value.clone()))
            .build()
            .unwrap();

        assert_eq!(processor.registration_tasks.len(), 1);
        for task in &processor.registration_tasks {
            task.register_once(&processor.registration_context())
                .await?;
        }

        let entry_key = entry::Entry::gen_did(topic)?;
        processor.storage_fetch(entry_key).await?;
        let entry = processor
            .storage_check_cache(entry_key)
            .await
            .expect("custom registration entry should be cached after publish");

        assert!(entry.data.contains(&value));
        Ok(())
    }

    #[tokio::test]
    async fn online_node_descriptor_publishes_and_lists_signed_self() -> Result<()> {
        let processor = prepare_processor().await;
        let published = processor.publish_online_node_descriptor().await?;
        let nodes = processor.lookup_online_nodes(false).await?;

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].did, processor.did());
        assert_eq!(nodes[0].did, published.did);
        assert_eq!(nodes[0].network_id, processor.swarm.network_id());
        assert!(nodes[0].verify_signature());
        assert!(!nodes[0].is_expired_at(get_epoch_ms()));
        Ok(())
    }

    #[tokio::test]
    async fn online_node_descriptor_refresh_replaces_previous_self_record() -> Result<()> {
        let processor = prepare_processor().await;
        let first = processor.publish_online_node_descriptor().await?;
        futures_timer::Delay::new(std::time::Duration::from_millis(1)).await;
        let second = processor.publish_online_node_descriptor().await?;
        let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
        processor.storage_fetch(entry_key).await?;
        let entry = processor
            .storage_check_cache(entry_key)
            .await
            .expect("online node registry entry should be cached after publish");
        let stored = Processor::online_node_descriptors_from_entry(&entry);
        let nodes = processor.lookup_online_nodes(false).await?;

        assert_eq!(stored.len(), 1);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].did, processor.did());
        assert!(second.heartbeat_at_ms >= first.heartbeat_at_ms);
        assert_eq!(nodes[0].heartbeat_at_ms, second.heartbeat_at_ms);
        Ok(())
    }

    #[tokio::test]
    async fn online_node_concurrent_publish_keeps_one_self_record() -> Result<()> {
        let processor = prepare_processor().await;
        let processor_clone = processor.clone();

        let (first, second) = futures::try_join!(
            processor.publish_online_node_descriptor(),
            processor_clone.publish_online_node_descriptor(),
        )?;
        let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
        processor.storage_fetch(entry_key).await?;
        let entry = processor
            .storage_check_cache(entry_key)
            .await
            .expect("online node registry entry should be cached after publish");
        let stored = Processor::online_node_descriptors_from_entry(&entry);
        let nodes = processor.lookup_online_nodes(false).await?;

        assert_eq!(stored.len(), 1);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].did, processor.did());
        assert!(
            nodes[0].heartbeat_at_ms == first.heartbeat_at_ms
                || nodes[0].heartbeat_at_ms == second.heartbeat_at_ms
        );
        Ok(())
    }

    #[tokio::test]
    async fn online_node_lookup_filters_expired_descriptors_by_default() -> Result<()> {
        let processor = prepare_processor().await;
        let expired_processor = prepare_processor().await;
        let now_ms = get_epoch_ms();
        let live = processor.online_node_descriptor_at(now_ms)?;
        let expired = OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did: expired_processor.did(),
                public_key: expired_processor
                    .swarm
                    .account_verification_pubkey()
                    .map_err(Error::CoreError)?,
                node_type: default_online_node_type(),
                network_id: expired_processor.swarm.network_id(),
                capabilities: OnlineNodeRegistration::capabilities(),
                endpoint_hint: None,
                started_at_ms: now_ms.saturating_sub(120_000),
                heartbeat_at_ms: now_ms.saturating_sub(90_000),
                expires_at_ms: now_ms.saturating_sub(30_000),
                version: crate::util::build_version(),
            },
            &expired_processor.session_sk,
        )
        .map_err(Error::CoreError)?;

        processor
            .storage_store(Processor::online_node_registry_entry(vec![
                live.clone(),
                expired.clone(),
            ])?)
            .await?;

        let live_nodes = processor.lookup_online_nodes(false).await?;
        assert_eq!(live_nodes, vec![live]);

        let all_nodes = processor.lookup_online_nodes(true).await?;
        assert_eq!(all_nodes.len(), 2);
        assert!(all_nodes
            .iter()
            .any(|descriptor| descriptor.did == processor.did()));
        assert!(all_nodes
            .iter()
            .any(|descriptor| descriptor.did == expired_processor.did()));
        assert!(all_nodes.iter().any(|descriptor| descriptor == &expired));
        Ok(())
    }

    #[tokio::test]
    async fn online_node_lookup_filters_other_network_descriptors() -> Result<()> {
        let processor = prepare_processor_with_network(0).await;
        let foreign = prepare_processor_with_network(1).await;
        let now_ms = get_epoch_ms();
        let local_descriptor = processor.online_node_descriptor_at(now_ms)?;
        let foreign_descriptor = foreign.online_node_descriptor_at(now_ms)?;

        processor
            .storage_store(Processor::online_node_registry_entry(vec![
                local_descriptor.clone(),
                foreign_descriptor,
            ])?)
            .await?;

        let nodes = processor.lookup_online_nodes(true).await?;
        assert_eq!(nodes, vec![local_descriptor]);
        Ok(())
    }

    #[tokio::test]
    async fn online_node_registry_lists_multiple_nodes() -> Result<()> {
        let processor = prepare_processor().await;
        let other = prepare_processor().await;
        let other_descriptor = other.online_node_descriptor_at(get_epoch_ms())?;

        processor
            .storage_touch_data(
                ONLINE_NODES_TOPIC,
                other_descriptor.encode().map_err(Error::CoreError)?,
            )
            .await?;
        let published = processor.publish_online_node_descriptor().await?;
        let mut nodes = processor.lookup_online_nodes(false).await?;
        nodes.sort_by_key(|descriptor| descriptor.did);

        assert_eq!(nodes.len(), 2);
        assert!(nodes
            .iter()
            .any(|descriptor| descriptor.did == published.did));
        assert!(nodes.iter().any(|descriptor| descriptor.did == other.did()));
        assert!(nodes.iter().all(OnlineNodeDescriptor::verify_signature));
        Ok(())
    }

    #[tokio::test]
    async fn online_node_registry_lists_two_publishers_over_network() -> Result<()> {
        let _network_guard = network_test_guard().await;
        let (publisher, owner) = prepare_online_node_registry_pair(42).await?;
        let callback = test_callback();
        let other_callback = test_callback();
        publisher.swarm.set_callback(callback.clone()).unwrap();
        owner.swarm.set_callback(other_callback.clone()).unwrap();
        connect_processors(&publisher, &owner, &callback, &other_callback).await;
        wait_for_mutual_dht_topology(&publisher, &owner).await?;
        let registry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
        let placement_keys = registry_key.rotate_affine(DATA_REDUNDANT)?;
        for placement_key in placement_keys.as_slice() {
            assert!(!owns_entry_placement(&publisher, *placement_key)?);
            assert!(owns_entry_placement(&owner, *placement_key)?);
        }

        let published = publisher.publish_online_node_descriptor().await?;
        let mut expected = BTreeSet::from([published.did]);
        wait_for_online_node_dids_in_storage(
            &owner,
            placement_keys.as_slice(),
            &expected,
            "owner stores publisher publish",
        )
        .await?;

        let owner_published = owner.publish_online_node_descriptor().await?;
        expected.insert(owner_published.did);
        wait_for_online_node_dids_in_storage(
            &owner,
            placement_keys.as_slice(),
            &expected,
            "owner stores both publishers at every placement",
        )
        .await?;
        let other_nodes =
            wait_for_online_node_dids(&owner, &expected, "owner sees both publishers").await?;
        let nodes =
            wait_for_online_node_dids(&publisher, &expected, "publisher sees both publishers")
                .await?;

        assert!(nodes.iter().all(OnlineNodeDescriptor::verify_signature));
        assert!(other_nodes
            .iter()
            .all(OnlineNodeDescriptor::verify_signature));
        Ok(())
    }

    #[tokio::test]
    async fn online_node_type_is_configurable() {
        let processor = prepare_processor_with_online_node_type(OnlineNodeType::Browser).await;
        let descriptor = processor.online_node_descriptor_at(get_epoch_ms()).unwrap();

        assert_eq!(descriptor.node_type, OnlineNodeType::Browser);
    }

    #[tokio::test]
    async fn test_processor_create_offer() {
        let peer_did = SecretKey::random().address().into();
        let processor = prepare_processor().await;
        processor.swarm.create_offer(peer_did).await.unwrap();
        let conn_dids = processor.swarm.peers();
        assert_eq!(conn_dids.len(), 1);
        assert_eq!(conn_dids.first().unwrap().did, peer_did.to_string());
    }

    struct SwarmCallbackInstance {
        inbound: Mutex<Vec<Message>>,
        inbound_notify: Notify,
        connected_notify: Notify,
    }

    struct StaticRegistration {
        publisher: crate::registration::DhtRegistrationPublisher,
        value: Encoded,
    }

    impl StaticRegistration {
        fn new(topic: &str, value: Encoded) -> Self {
            Self {
                publisher: crate::registration::DhtRegistrationPublisher::new(topic),
                value,
            }
        }
    }

    #[async_trait]
    impl RegistrationTask for StaticRegistration {
        fn name(&self) -> &'static str {
            "static-test"
        }

        fn interval(&self) -> Duration {
            Duration::from_secs(60)
        }

        async fn register_once(&self, context: &RegistrationContext<'_>) -> Result<()> {
            self.publisher.publish(context, self.value.clone()).await
        }
    }

    #[async_trait]
    impl SwarmCallback for SwarmCallbackInstance {
        async fn on_inbound(
            &self,
            payload: &MessagePayload,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            let msg: Message = payload.transaction.data().map_err(Box::new)?;
            {
                let mut inbound = self.inbound.lock().unwrap();
                inbound.push(msg);
            }
            self.inbound_notify.notify_one();

            Ok(())
        }

        async fn on_event(
            &self,
            event: &SwarmEvent,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            if let SwarmEvent::ConnectionStateChange {
                state: WebrtcConnectionState::Connected,
                ..
            } = event
            {
                self.connected_notify.notify_one();
            }

            Ok(())
        }
    }

    fn test_callback() -> Arc<SwarmCallbackInstance> {
        Arc::new(SwarmCallbackInstance {
            inbound: Mutex::new(Vec::new()),
            inbound_notify: Notify::new(),
            connected_notify: Notify::new(),
        })
    }

    async fn network_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        NETWORK_TEST_LOCK
            .get_or_init(|| AsyncTestMutex::new(()))
            .lock()
            .await
    }

    async fn prepare_processor_with_identity_key(identity_key: SecretKey) -> Processor {
        prepare_processor_with_identity_key_and_network(identity_key, 0).await
    }

    async fn prepare_processor_with_identity_key_and_network(
        identity_key: SecretKey,
        network_id: u32,
    ) -> Processor {
        let session_sk = SessionSk::new_with_seckey(&identity_key).unwrap();
        let config = ProcessorConfig::new(
            network_id,
            "stun://stun.l.google.com:19302".to_string(),
            session_sk,
            3,
        );
        let storage = Box::new(MemStorage::new());

        ProcessorBuilder::from_config(&config)
            .unwrap()
            .storage(storage)
            .dht_finger_table_size(8)
            .build()
            .unwrap()
    }

    async fn prepare_online_node_registry_pair(network_id: u32) -> Result<(Processor, Processor)> {
        let registry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;
        let placement_keys = registry_key.rotate_affine(DATA_REDUNDANT)?;
        // Keep the fetch path deterministic: storage_fetch returns the first
        // placement hit, so the publisher must not own a stale replica on any
        // registry placement before it asks the owner for the merged entry.
        for _ in 0..512 {
            let first_key = SecretKey::random();
            let second_key = SecretKey::random();
            let first_did = first_key.address().into();
            let second_did = second_key.address().into();
            let first_owns_all =
                owns_all_placements(first_did, second_did, placement_keys.as_slice());
            let second_owns_all =
                owns_all_placements(second_did, first_did, placement_keys.as_slice());
            let Some((publisher_key, owner_key)) = (match (first_owns_all, second_owns_all) {
                (true, false) => Some((second_key, first_key)),
                (false, true) => Some((first_key, second_key)),
                _ => None,
            }) else {
                continue;
            };
            let publisher =
                prepare_processor_with_identity_key_and_network(publisher_key, network_id).await;
            let owner =
                prepare_processor_with_identity_key_and_network(owner_key, network_id).await;
            return Ok((publisher, owner));
        }
        Err(Error::InvalidConfig(
            "could not generate an online-node registry owner covering every placement".to_string(),
        ))
    }

    fn owns_all_placements(local: Did, successor: Did, placements: &[Did]) -> bool {
        placements
            .iter()
            .all(|placement| *placement - local <= successor - local)
    }

    async fn prepare_processor_with_network(network_id: u32) -> Processor {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let config = ProcessorConfig::new(
            network_id,
            "stun://stun.l.google.com:19302".to_string(),
            session_sk,
            3,
        );
        let storage = Box::new(MemStorage::new());

        ProcessorBuilder::from_config(&config)
            .unwrap()
            .storage(storage)
            .dht_finger_table_size(8)
            .build()
            .unwrap()
    }

    fn owns_entry_placement(processor: &Processor, placement_key: Did) -> Result<bool> {
        match processor.swarm.dht().find_successor(placement_key)? {
            PeerRingAction::Some(_) => Ok(true),
            PeerRingAction::RemoteAction(_, PeerRingRemoteAction::FindSuccessor(_)) => Ok(false),
            action => Err(Error::InvalidConfig(format!(
                "unexpected registry owner lookup action: {action:?}"
            ))),
        }
    }

    async fn prepare_processor_with_online_node_type(node_type: OnlineNodeType) -> Processor {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let config = ProcessorConfig::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            session_sk,
            3,
        );
        let storage = Box::new(MemStorage::new());

        ProcessorBuilder::from_config(&config)
            .unwrap()
            .storage(storage)
            .online_node_type(node_type)
            .dht_finger_table_size(8)
            .build()
            .unwrap()
    }

    async fn prepare_measured_processor() -> Processor {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let config = ProcessorConfig::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            session_sk,
            3,
        );
        let storage = Box::new(MemStorage::new());
        let measure = PeriodicMeasure::new(Box::new(MemStorage::new()));

        ProcessorBuilder::from_config(&config)
            .unwrap()
            .storage(storage)
            .measure(measure)
            .dht_finger_table_size(8)
            .build()
            .unwrap()
    }

    async fn connect_processors(
        p1: &Processor,
        p2: &Processor,
        callback1: &SwarmCallbackInstance,
        callback2: &SwarmCallbackInstance,
    ) {
        let offer = p1.swarm.create_offer(p2.did()).await.unwrap();
        let answer = p2.swarm.answer_offer(offer).await.unwrap();
        p1.swarm.accept_answer(answer).await.unwrap();
        wait_processors_connected(p1, p2, callback1, callback2).await;
    }

    async fn wait_processors_connected(
        p1: &Processor,
        p2: &Processor,
        callback1: &SwarmCallbackInstance,
        callback2: &SwarmCallbackInstance,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if processor_has_connected_peer(p1, p2.did())
                && processor_has_connected_peer(p2, p1.did())
            {
                return;
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("processors did not connect");
            tokio::time::timeout(remaining, async {
                tokio::select! {
                    _ = callback1.connected_notify.notified() => {}
                    _ = callback2.connected_notify.notified() => {}
                }
            })
            .await
            .expect("processors did not connect");
        }
    }

    fn processor_has_connected_peer(processor: &Processor, peer: Did) -> bool {
        let peer = peer.to_string();
        processor
            .swarm
            .peers()
            .into_iter()
            .any(|conn| conn.did == peer && conn.state == "Connected")
    }

    async fn wait_for_mutual_dht_topology(processor: &Processor, other: &Processor) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let inspect = processor.swarm.inspect().await;
            let other_inspect = other.swarm.inspect().await;
            let did = processor.did().to_string();
            let other_did = other.did().to_string();
            let processor_sees_other = inspect
                .dht
                .successors
                .iter()
                .any(|successor| successor == &other_did)
                && inspect.dht.predecessor.as_ref() == Some(&other_did);
            let other_sees_processor = other_inspect
                .dht
                .successors
                .iter()
                .any(|successor| successor == &did)
                && other_inspect.dht.predecessor.as_ref() == Some(&did);
            if processor_sees_other && other_sees_processor {
                return Ok(());
            }

            let stabilizer = processor.swarm.stabilizer();
            let other_stabilizer = other.swarm.stabilizer();
            futures::try_join!(stabilizer.stabilize(), other_stabilizer.stabilize(),)
                .map_err(Error::CoreError)?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!(
                        "mutual DHT topology did not converge: processor={:?}, other={:?}",
                        inspect.dht, other_inspect.dht
                    )
                });
            tokio::time::timeout(remaining, tokio::time::sleep(Duration::from_millis(20)))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "mutual DHT topology did not converge: processor={:?}, other={:?}",
                        inspect.dht, other_inspect.dht
                    )
                });
        }
    }

    async fn wait_for_online_node_dids(
        processor: &Processor,
        expected: &BTreeSet<Did>,
        context: &str,
    ) -> Result<Vec<OnlineNodeDescriptor>> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let nodes = processor.lookup_online_nodes(false).await?;
            let observed = nodes
                .iter()
                .map(|descriptor| descriptor.did)
                .collect::<BTreeSet<_>>();
            if expected.is_subset(&observed) {
                return Ok(nodes);
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!(
                        "online node registry did not converge during {context}: expected {expected:?}, observed {observed:?}",
                    )
                });
            tokio::time::timeout(remaining, tokio::time::sleep(Duration::from_millis(20)))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "online node registry did not converge during {context}: expected {expected:?}, observed {observed:?}",
                    )
                });
        }
    }

    async fn wait_for_online_node_dids_in_storage(
        processor: &Processor,
        placement_keys: &[Did],
        expected: &BTreeSet<Did>,
        context: &str,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let mut observed_by_placement = BTreeMap::new();
            for placement_key in placement_keys {
                let observed = match processor
                    .swarm
                    .dht()
                    .storage
                    .get(&placement_key.to_string())
                    .await
                    .map_err(Error::Storage)?
                {
                    Some(entry) => Processor::online_node_descriptors_from_entry(&entry)
                        .into_iter()
                        .map(|descriptor| descriptor.did)
                        .collect::<BTreeSet<_>>(),
                    None => BTreeSet::new(),
                };
                observed_by_placement.insert(*placement_key, observed);
            }

            if observed_by_placement
                .values()
                .all(|observed| expected.is_subset(observed))
            {
                return Ok(());
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!(
                        "online node registry storage did not converge during {context}: expected {expected:?}, observed {observed_by_placement:?}",
                    )
                });
            tokio::time::timeout(remaining, tokio::time::sleep(Duration::from_millis(20)))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "online node registry storage did not converge during {context}: expected {expected:?}, observed {observed_by_placement:?}",
                    )
                });
        }
    }

    async fn wait_for_peer_measurement(
        processor: &Processor,
        did: Did,
        predicate: impl Fn(&PeerMeasurement) -> bool,
    ) -> PeerMeasurement {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(measurement) = processor.peer_measurement(did).await {
                if predicate(&measurement) {
                    return measurement;
                }
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("measurement was not updated");
            tokio::time::timeout(remaining, tokio::time::sleep(Duration::from_millis(20)))
                .await
                .expect("measurement was not updated");
        }
    }

    async fn wait_for_inbound_message(
        callback: &SwarmCallbackInstance,
        predicate: impl Fn(&Message) -> bool,
    ) -> Message {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let inbound = callback.inbound.lock().unwrap();
                if let Some(msg) = inbound.iter().find(|msg| predicate(msg)).cloned() {
                    return msg;
                }
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("inbound message was not delivered");
            tokio::time::timeout(remaining, callback.inbound_notify.notified())
                .await
                .expect("inbound message was not delivered");
        }
    }

    async fn wait_for_e2e_stream_frames(
        callback: &SwarmCallbackInstance,
        stream_id: e2e::E2eStreamId,
    ) -> Vec<E2eStreamFrame> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let inbound = callback.inbound.lock().unwrap();
                let frames = inbound
                    .iter()
                    .filter_map(|msg| match msg {
                        Message::E2eStreamFrame(frame) if frame.stream_id == stream_id => {
                            Some(frame.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if frames.iter().any(|frame| frame.is_final) {
                    return frames;
                }
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("E2E stream final frame was not delivered");
            tokio::time::timeout(remaining, callback.inbound_notify.notified())
                .await
                .expect("E2E stream final frame was not delivered");
        }
    }

    #[tokio::test]
    async fn test_processor_handshake_msg() {
        let _network_guard = network_test_guard().await;
        let callback1 = test_callback();
        let callback2 = test_callback();

        let p1 = prepare_processor().await;
        let p2 = prepare_processor().await;

        p1.swarm.set_callback(callback1.clone()).unwrap();
        p2.swarm.set_callback(callback2.clone()).unwrap();

        let did1 = p1.did();
        let did2 = p2.did();

        let offer = p1.swarm.create_offer(p2.did()).await.unwrap();
        assert_eq!(
            p1.swarm
                .peers()
                .into_iter()
                .find(|peer| peer.did == p2.did().to_string())
                .unwrap()
                .state,
            "New"
        );

        let answer = p2.swarm.answer_offer(offer).await.unwrap();
        p1.swarm.accept_answer(answer).await.unwrap();
        wait_processors_connected(&p1, &p2, &callback1, &callback2).await;

        let test_text1 = "test1";
        let test_text2 = "test2";

        p1.send_message(did2, test_text1.as_bytes()).await.unwrap();
        p2.send_message(did1, test_text2.as_bytes()).await.unwrap();

        let got_msg2 = wait_for_inbound_message(&callback2, |msg| {
            matches!(msg, Message::CustomMessage(custom) if custom.0 == test_text1.as_bytes())
        })
        .await;
        assert!(matches!(got_msg2, Message::CustomMessage(_)));

        let got_msg1 = wait_for_inbound_message(&callback1, |msg| {
            matches!(msg, Message::CustomMessage(custom) if custom.0 == test_text2.as_bytes())
        })
        .await;
        assert!(matches!(got_msg1, Message::CustomMessage(_)));
    }

    #[tokio::test]
    async fn peer_measurement_is_absent_without_measure_or_observation() {
        let unmeasured = prepare_processor_with_identity_key(SecretKey::random()).await;
        let unseen_did = SecretKey::random().address().into();
        assert!(unmeasured.peer_measurement(unseen_did).await.is_none());

        let measured = prepare_measured_processor().await;
        assert!(measured.peer_measurement(unseen_did).await.is_none());
        assert!(measured.peer_measurements().await.is_empty());
    }

    #[tokio::test]
    async fn provider_exposes_sent_and_received_peer_measurements() {
        let _network_guard = network_test_guard().await;
        let callback1 = test_callback();
        let callback2 = test_callback();
        let p1 = prepare_measured_processor().await;
        let p2 = prepare_measured_processor().await;

        p1.swarm.set_callback(callback1.clone()).unwrap();
        p2.swarm.set_callback(callback2.clone()).unwrap();
        connect_processors(&p1, &p2, &callback1, &callback2).await;

        p1.send_message(p2.did(), b"measure-provider")
            .await
            .unwrap();
        let got_msg2 = wait_for_inbound_message(
            &callback2,
            |msg| matches!(msg, Message::CustomMessage(custom) if custom.0 == b"measure-provider"),
        )
        .await;
        assert!(matches!(got_msg2, Message::CustomMessage(_)));

        let sent =
            wait_for_peer_measurement(&p1, p2.did(), |measurement| measurement.evidence.sent >= 1)
                .await;
        let received = wait_for_peer_measurement(&p2, p1.did(), |measurement| {
            measurement.evidence.received >= 1
        })
        .await;
        assert_eq!(sent.did, p2.did());
        assert_eq!(received.did, p1.did());

        let node_info = p1.get_node_info().await.unwrap();
        assert_eq!(node_info.version, crate::util::build_version());
        assert!(node_info.swarm.is_some());

        let provider = Provider::from_processor(Arc::new(p1));
        let provider_measurement = provider.peer_measurement(p2.did()).await.unwrap();
        assert!(provider_measurement.evidence.sent >= 1);

        let rpc_value = provider
            .request(Method::PeerMeasurement, PeerMeasurementRequest {
                did: p2.did().to_string(),
            })
            .await
            .unwrap();
        let rpc_measurement: PeerMeasurementResponse = serde_json::from_value(rpc_value).unwrap();
        assert!(rpc_measurement
            .measurement
            .as_ref()
            .is_some_and(|measurement| measurement.counters.sent >= 1));

        let list_value = provider
            .request(Method::ListPeerMeasurements, ListPeerMeasurementsRequest {})
            .await
            .unwrap();
        let list_measurements: ListPeerMeasurementsResponse =
            serde_json::from_value(list_value).unwrap();
        let p2_did_json = serde_json::to_value(p2.did()).unwrap();
        assert!(list_measurements
            .measurements
            .iter()
            .any(|measurement| measurement.did == p2_did_json && measurement.counters.sent >= 1));
    }

    #[tokio::test]
    async fn test_processor_e2e_handshake_exchanges_verified_public_keys() {
        let _network_guard = network_test_guard().await;
        let callback1 = test_callback();
        let callback2 = test_callback();

        let p1 = prepare_processor().await;
        let p2 = prepare_processor().await;

        p1.swarm.set_callback(callback1.clone()).unwrap();
        p2.swarm.set_callback(callback2.clone()).unwrap();

        connect_processors(&p1, &p2, &callback1, &callback2).await;

        let did1 = p1.did();
        let did2 = p2.did();
        let requester_public_key = p1.swarm.account_pubkey().unwrap();
        let responder_public_key = p2.swarm.account_pubkey().unwrap();

        p1.send_e2e_handshake(did2).await.unwrap();

        let request = wait_for_inbound_message(&callback2, |msg| {
            matches!(msg, Message::E2eHandshakeRequest(_))
        })
        .await;
        match request {
            Message::E2eHandshakeRequest(request) => {
                assert_eq!(request.requester_public_key, requester_public_key);
                assert_eq!(
                    p2.verify_e2e_handshake_request(did1, &request).unwrap(),
                    requester_public_key
                );
            }
            msg => panic!("expected E2eHandshakeRequest, got {msg:?}"),
        }

        let response = wait_for_inbound_message(&callback1, |msg| {
            matches!(msg, Message::E2eHandshakeResponse(_))
        })
        .await;
        match response {
            Message::E2eHandshakeResponse(response) => {
                assert_eq!(response.responder_public_key, responder_public_key);
                assert_eq!(
                    p1.verify_e2e_handshake_response(did2, &response).unwrap(),
                    responder_public_key
                );
            }
            msg => panic!("expected E2eHandshakeResponse, got {msg:?}"),
        }
    }

    #[tokio::test]
    async fn test_processor_e2e_message_streams_and_decrypts_with_receiver_identity_key() {
        let _network_guard = network_test_guard().await;
        let callback1 = test_callback();
        let callback2 = test_callback();
        let identity1 = SecretKey::random();
        let identity2 = SecretKey::random();

        let p1 = prepare_processor_with_identity_key(identity1).await;
        let p2 = prepare_processor_with_identity_key(identity2).await;

        p1.swarm.set_callback(callback1.clone()).unwrap();
        p2.swarm.set_callback(callback2.clone()).unwrap();

        connect_processors(&p1, &p2, &callback1, &callback2).await;

        let did1 = p1.did();
        let did2 = p2.did();
        let responder_public_key = p2.swarm.account_pubkey().unwrap();
        let stream_id = p1
            .send_e2e_message_with_frame_len(
                did2,
                responder_public_key,
                b"homomorphic-ready streaming body",
                8,
            )
            .await
            .unwrap();

        let frames = wait_for_e2e_stream_frames(&callback2, stream_id).await;
        assert!(
            frames.len() > 1,
            "streaming send should emit more than one frame for this frame size"
        );
        assert_eq!(
            frames.iter().filter(|frame| frame.is_final).count(),
            1,
            "streaming send should emit exactly one final frame"
        );

        let mut sequences = frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        let frame_count = u64::try_from(frames.len()).unwrap();
        assert_eq!(sequences, (0..frame_count).collect::<Vec<_>>());

        let mut decryptor = p2.e2e_stream_decryptor(did1, stream_id, identity2).unwrap();
        let mut plaintext = Vec::new();
        let mut delivered_frames = frames.clone();
        delivered_frames.reverse();
        for frame in &delivered_frames {
            plaintext
                .extend_from_slice(&p2.decrypt_e2e_stream_frame(&mut decryptor, frame).unwrap());
        }
        decryptor.finish().unwrap();
        assert_eq!(plaintext, b"homomorphic-ready streaming body");

        assert!(matches!(
            p2.e2e_stream_decryptor(did1, stream_id, SecretKey::random()),
            Err(Error::CoreError(
                rings_core::error::Error::E2ePublicKeyDidMismatch { .. }
            ))
        ));
    }
}
