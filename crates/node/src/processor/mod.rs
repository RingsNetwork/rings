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
use rings_core::measure::PeerQuality;
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
use crate::measure::peer_quality_thresholds;
use crate::measure::PeriodicMeasure;
use crate::onion::default_advertise_onion_exit;
use crate::onion::default_advertise_onion_relay;
use crate::onion::default_onion_exit_heartbeat_interval_secs;
use crate::onion::default_onion_exit_policy;
use crate::onion::default_onion_exit_services;
use crate::onion::default_onion_exit_ttl_secs;
use crate::onion::directory;
use crate::onion::directory::OnionDirectoryReader;
use crate::onion::https_onion_exit_services;
use crate::onion::proxy::OnionProxyConfig;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
#[cfg(feature = "browser")]
use crate::onion::proxy::ONION_PROXY_HTTPS_SERVICE;
use crate::onion::validate_onion_exit_registration_timing;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitRegistration;
use crate::onion::OnionExitService;
use crate::onion::OnionRoute;
use crate::onion::ONION_EXITS_TOPIC;
use crate::onion::ONION_RELAY_CAPABILITY;
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

mod builder;
mod config;

pub use builder::ProcessorBuilder;
#[cfg(feature = "node")]
pub(crate) use config::parse_webrtc_udp_port_range;
pub use config::ProcessorConfig;
pub use config::ProcessorConfigSerialized;

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
    #[cfg(feature = "browser")]
    advertise_onion_relay: bool,
    registration_tasks: Vec<Arc<dyn RegistrationTask>>,
}

impl Processor {
    /// Get current did
    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    pub(crate) fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    #[cfg(feature = "browser")]
    pub(crate) fn advertise_onion_relay(&self) -> bool {
        self.advertise_onion_relay
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

    fn onion_exit_descriptors_from_entry(entry: &entry::Entry) -> Vec<OnionExitDescriptor> {
        OnionExitRegistration::descriptors_from_entry(entry)
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

    #[cfg(all(test, feature = "node"))]
    fn onion_exit_registry_entry(descriptors: Vec<OnionExitDescriptor>) -> Result<entry::Entry> {
        let data = descriptors
            .into_iter()
            .map(|descriptor| descriptor.encode().map_err(Error::CoreError))
            .collect::<Result<Vec<_>>>()?;

        Ok(entry::Entry::new(
            entry::Entry::gen_did(ONION_EXITS_TOPIC)?,
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
            .filter(|descriptor| descriptor.matches_dht_protocol(self.swarm.dht_protocol_mode()));

        Ok(OnlineNodeDescriptor::latest_valid_by_did(
            descriptors,
            get_epoch_ms(),
            include_expired,
        ))
    }

    /// List signed onion-exit descriptors from the application-layer exit registry.
    pub async fn lookup_onion_exits(
        &self,
        service: &str,
        include_expired: bool,
    ) -> Result<Vec<OnionExitDescriptor>> {
        let entry_key = entry::Entry::gen_did(ONION_EXITS_TOPIC)?;

        self.storage_fetch(entry_key).await?;
        let Some(entry) = self.storage_check_cache(entry_key).await else {
            return Ok(vec![]);
        };

        let service = service.trim();
        let descriptors = OnionExitDescriptor::latest_valid_by_service_did(
            Self::onion_exit_descriptors_from_entry(&entry)
                .into_iter()
                .filter(|descriptor| descriptor.matches_network(self.swarm.network_id())),
            get_epoch_ms(),
            include_expired,
        )
        .into_iter()
        .filter(|descriptor| service.is_empty() || descriptor.offers_service(service));

        Ok(descriptors.collect())
    }

    /// Build an onion route from live presence descriptors and live exit descriptors.
    pub async fn build_onion_route(
        &self,
        service: String,
        hop_count: usize,
        allow_short_paths: bool,
    ) -> Result<OnionRoute> {
        directory::build_onion_route(self, service, hop_count, allow_short_paths).await
    }

    /// Build an onion proxy route for a client target through a target-agnostic proxy config.
    pub async fn build_onion_proxy_route(
        &self,
        proxy: OnionProxyConfig,
        target: OnionProxyTarget,
    ) -> Result<OnionProxyRoute> {
        directory::build_onion_proxy_route(self, proxy, target).await
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

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl OnionDirectoryReader for Processor {
    fn local_did(&self) -> Did {
        self.did()
    }

    async fn live_online_nodes(&self) -> Result<Vec<OnlineNodeDescriptor>> {
        self.lookup_online_nodes(false).await
    }

    async fn live_onion_exits(&self, service: &str) -> Result<Vec<OnionExitDescriptor>> {
        self.lookup_onion_exits(service, false).await
    }

    async fn peer_qualities(&self) -> Vec<(Did, PeerQuality)> {
        let thresholds = peer_quality_thresholds();
        self.peer_measurements()
            .await
            .into_iter()
            .map(|measurement| (measurement.did, measurement.evidence.classify(thresholds)))
            .collect()
    }
}

#[cfg(all(test, feature = "node"))]
mod tests;
