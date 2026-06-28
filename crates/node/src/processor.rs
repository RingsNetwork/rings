#![warn(missing_docs)]

//! Processor of rings-node rpc server.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rings_core::chunk::ReassemblyLimits;
use rings_core::dht::Did;
use rings_core::dht::EntryStorage;
use rings_core::dht::DEFAULT_FINGER_TABLE_SIZE;
use rings_core::ecc::PublicKey;
use rings_core::ecc::SecretKey;
use rings_core::measure::MeasureImpl;
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
use rings_rpc::protos::rings_node::*;
use rings_transport::webrtc_config::WebrtcUdpPortRange;
use serde::Deserialize;
use serde::Serialize;

use crate::consts::DATA_REDUNDANT;
use crate::error::Error;
use crate::error::Result;
use crate::measure::PeriodicMeasure;
use crate::prelude::entry;
use crate::prelude::wasm_export;
use crate::prelude::ChordStorageInterface;
use crate::prelude::ChordStorageInterfaceCacheChecker;
use crate::prelude::SessionSk;

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
        })
    }
}

impl TryFrom<ProcessorConfigSerialized> for ProcessorConfig {
    type Error = Error;
    fn try_from(ins: ProcessorConfigSerialized) -> Result<Self> {
        let webrtc_udp_port_range =
            parse_webrtc_udp_port_range(ins.webrtc_udp_port_min, ins.webrtc_udp_port_max)?;
        Ok(Self {
            network_id: ins.network_id,
            ice_servers: ins.ice_servers.clone(),
            external_address: ins.external_address.clone(),
            webrtc_udp_port_min: webrtc_udp_port_range.map(WebrtcUdpPortRange::min),
            webrtc_udp_port_max: webrtc_udp_port_range.map(WebrtcUdpPortRange::max),
            session_sk: SessionSk::from_str(&ins.session_sk)?,
            stabilize_interval: Duration::from_secs(ins.stabilize_interval),
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
    dht_finger_table_size: usize,
    reassembly_limits: ReassemblyLimits,
}

/// Processor for rings-node rpc server
#[derive(Clone)]
pub struct Processor {
    /// a swarm instance
    pub swarm: Arc<Swarm>,
    stabilize_interval: Duration,
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
        Ok(Self {
            network_id: config.network_id,
            ice_servers: config.ice_servers.clone(),
            external_address: config.external_address.clone(),
            webrtc_udp_port_range: config.webrtc_udp_port_range()?,
            session_sk: config.session_sk.clone(),
            storage: None,
            measure: None,
            stabilize_interval: config.stabilize_interval,
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
        self.measure = Some(Box::new(implement));
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

    /// Build the [Processor].
    pub fn build(self) -> Result<Processor> {
        self.session_sk
            .session()
            .verify_self()
            .map_err(|e| Error::VerifyError(e.to_string()))?;

        let storage = self.storage.unwrap_or_else(|| Box::new(MemStorage::new()));

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
            stabilize_interval: self.stabilize_interval,
        })
    }
}

impl Processor {
    /// Get current did
    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    /// Run stabilization daemon
    pub async fn listen(&self) {
        let stabilizer = self.swarm.stabilizer();
        Arc::new(stabilizer).wait(self.stabilize_interval).await
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

    /// register service
    pub async fn register_service(&self, name: &str) -> Result<()> {
        let encoded_did = self
            .did()
            .to_string()
            .encode()
            .map_err(Error::ServiceRegisterError)?;
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_touch_data(
            &self.swarm,
            name,
            encoded_did,
        )
        .await
        .map_err(Error::ServiceRegisterError)
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
    use std::sync::Mutex;
    use std::time::Duration;
    use std::time::Instant;

    use rings_core::storage::MemStorage;
    use rings_core::swarm::callback::SwarmCallback;
    use rings_core::swarm::callback::SwarmEvent;
    use rings_transport::core::transport::WebrtcConnectionState;
    use tokio::sync::Notify;

    use super::*;
    use crate::prelude::*;
    use crate::tests::native::prepare_processor;

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

    async fn prepare_processor_with_identity_key(identity_key: SecretKey) -> Processor {
        let session_sk = SessionSk::new_with_seckey(&identity_key).unwrap();
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
    async fn test_processor_e2e_handshake_exchanges_verified_public_keys() {
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
