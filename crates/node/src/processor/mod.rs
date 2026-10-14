//! Processor of rings-node rpc server.

use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use rings_core::chunk::ReassemblyLimits;
use rings_core::dht::Did;
use rings_core::dht::EntryStorage;
use rings_core::dht::DEFAULT_FINGER_TABLE_SIZE;
use rings_core::inspect::DHTInspect;
use rings_core::lifecycle::StopSource;
use rings_core::lifecycle::StopToken;
use rings_core::measure::EvidenceCounters;
use rings_core::measure::EvidenceDigest;
use rings_core::measure::EvidenceError;
use rings_core::measure::EvidencePage;
use rings_core::measure::MeasureImpl;
use rings_core::measure::PeerMeasurement;
use rings_core::measure::PeerMeasurementPage;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::Message;
use rings_core::message::OriginQuotaConfig;
use rings_core::message::OriginQuotaCounters;
use rings_core::message::ReplayStorage;
use rings_core::storage::MemStorage;
use rings_core::swarm::Swarm;
use rings_core::swarm::SwarmBuilder;
use rings_core::utils::get_epoch_ms;
use rings_rpc::protos::rings_node::*;
use rings_runtime::sleep;
use rings_transport::webrtc_config::WebrtcUdpPortRange;
use serde::Deserialize;
use serde::Serialize;
use uuid;

use crate::error::Error;
use crate::error::Result;
use crate::measure::PeriodicMeasure;
use crate::observability::HealthSnapshot;
use crate::observability::MailboxSnapshot;
use crate::observability::Observability;
use crate::observability::OperatorSnapshot;
use crate::observability::PeerRatingSnapshot;
use crate::observability::SessionKeySnapshot;
use crate::observability::OPERATOR_SCHEMA_VERSION;
use crate::observability::PEER_RATING_CAPACITY;
use crate::onion::circuit::OnionCircuitCapabilities;
use crate::onion::default_advertise_onion_exit;
use crate::onion::default_advertise_onion_relay;
use crate::onion::default_onion_exit_heartbeat_interval_secs;
use crate::onion::default_onion_exit_policy;
use crate::onion::default_onion_exit_services;
use crate::onion::default_onion_exit_ttl_secs;
use crate::onion::directory;
use crate::onion::directory::OnionDirectoryReader;
use crate::onion::proxy::OnionProxyConfig;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::validate_onion_exit_registration_timing;
use crate::onion::OnionEntryGuardStorage;
use crate::onion::OnionEntryGuards;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitRegistration;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRole;
use crate::onion::OnionServiceName;
use crate::onion::ONION_EXITS_TOPIC;
use crate::online::OnlineNodeCapabilities;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;
use crate::prelude::entry;
use crate::prelude::wasm_export;
use crate::prelude::ChordStorageInterface;
use crate::prelude::ChordStorageInterfaceCacheChecker;
use crate::prelude::DelegateeKey;
use crate::registration::default_advertise_presence;
use crate::registration::default_online_node_heartbeat_interval_secs;
use crate::registration::default_online_node_ttl_secs;
use crate::registration::default_online_node_type;
use crate::registration::validate_online_node_registration_timing;
use crate::registration::OnlineNodeRegistration;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;

const MEASUREMENT_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

mod builder;
mod config;
/// End-to-end encrypted streams to a DID.
mod e2e_stream;
/// Handshakes: by DID over the overlay, and over a peer's HTTP endpoint.
pub(crate) mod handshake;

pub use builder::ProcessorBuilder;
#[cfg(feature = "node")]
pub(crate) use config::parse_webrtc_udp_port_range;
pub use config::ProcessorConfig;
pub use config::ProcessorConfigSerialized;
pub(crate) use handshake::HandshakeFailure;
pub(crate) use handshake::HandshakePeer;

const DHT_LOOKUP_CACHE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DHT_LOOKUP_CACHE_POLL_ATTEMPTS: usize = 40;

async fn sleep_registration_interval_with_stop(
    interval: Duration,
    stop: &StopToken,
    sibling_stop: &StopToken,
) -> Result<bool> {
    use futures::future::select;
    use futures::pin_mut;

    let stopped = async {
        let own_stop = stop.stopped();
        let maintenance_stop = sibling_stop.stopped();
        pin_mut!(own_stop, maintenance_stop);
        let _ = select(own_stop, maintenance_stop).await;
    };
    let interval_elapsed = sleep(interval);
    pin_mut!(stopped, interval_elapsed);
    match select(interval_elapsed, stopped).await {
        futures::future::Either::Left((result, _)) => {
            result?;
            Ok(true)
        }
        futures::future::Either::Right(((), _)) => Ok(false),
    }
}

/// Processor for rings-node rpc server.
///
/// Cloning shares the same node handle; publishes from any clone are serialized
/// against each other.
#[derive(Clone)]
pub struct Processor {
    /// a swarm instance
    pub swarm: Arc<Swarm>,
    /// Same delegatee key held by the swarm transport; kept here for node-layer descriptor signing.
    delegatee_key: DelegateeKey,
    /// Fresh process epoch `e_n` carried by every onion symbol this process registers (#834 D2).
    onion_process_epoch: OnionProcessEpoch,
    onion_entry_guards: Arc<OnionEntryGuards>,
    stabilize_interval: Duration,
    online_node_registration: OnlineNodeRegistration,
    measure: Option<Arc<PeriodicMeasure>>,
    /// Serializes listener generations across every clone and wrapper of this processor.
    /// Ownership includes graceful maintenance shutdown and measurement flushing.
    listener_lifecycle_lock: Arc<futures::lock::Mutex<()>>,
    /// Onion symbols this process registers (#834 D2).
    onion_role: OnionRole<OnionExitOffer>,
    registration_tasks: Vec<Arc<dyn RegistrationTask>>,
    /// Process-local bounded recorder backing the authenticated operator surface.
    observability: Arc<Observability>,
}

/// The node error of a DHT operation run under a registration stop: a rerouting wait ended by
/// the stop is the registration's cooperative stop, anything else an entry error.
pub(crate) fn stoppable_storage_error(error: rings_core::error::Error) -> Error {
    match error {
        rings_core::error::Error::ReroutingStopped => Error::RegistrationStopped,
        error => Error::EntryError(error),
    }
}

impl Processor {
    /// Get current did
    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    pub(crate) fn delegatee_key(&self) -> &DelegateeKey {
        &self.delegatee_key
    }

    #[cfg(all(feature = "browser", target_family = "wasm"))]
    pub(crate) fn onion_entry_guards(&self) -> &OnionEntryGuards {
        self.onion_entry_guards.as_ref()
    }

    /// Return the onion symbols this process registers (#834 D2).
    pub const fn onion_role(&self) -> &OnionRole<OnionExitOffer> {
        &self.onion_role
    }

    /// Return this process's circuit capabilities: its onion role at its process epoch, the image
    /// of [`Self::onion_role`] under `OnionRole::map(|_| e_n)`.
    pub(crate) fn onion_circuit_capabilities(&self) -> OnionCircuitCapabilities {
        self.onion_role.as_ref().map(|_| self.onion_process_epoch)
    }

    fn registration_context(&self) -> RegistrationContext<'_> {
        RegistrationContext::new(self)
    }

    fn registration_context_with_stop(&self, stop: StopToken) -> RegistrationContext<'_> {
        RegistrationContext::new_with_stop(self, stop)
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
    ///
    /// The registry carrier is the join of every reply this node has observed (#864): it ascends
    /// in the lattice order while cached and live, but a descriptor still leaves it on a
    /// tombstone, on an overwrite or compaction floor (#867), or at the element cap. Rerouted as
    /// [`Self::storage_fetch`] documents.
    pub async fn lookup_online_nodes(
        &self,
        include_expired: bool,
    ) -> Result<Vec<OnlineNodeDescriptor>> {
        let entry_key = entry::Entry::gen_did(ONLINE_NODES_TOPIC)?;

        let Some(entry) = self.fetch_storage_entry(entry_key).await? else {
            return Ok(vec![]);
        };

        let descriptors = Self::online_node_descriptors_from_entry(&entry)
            .into_iter()
            .filter(|descriptor| descriptor.matches_dht_protocol(self.swarm.dht_protocol_mode()));

        Ok(OnlineNodeDescriptor::latest_valid_by_did(
            descriptors,
            get_epoch_ms(),
            self.swarm.network_id(),
            include_expired,
        ))
    }

    /// List signed onion-exit descriptors from the application-layer exit registry.
    ///
    /// Like [`Self::lookup_online_nodes`], the carrier read is the join of the observed replies
    /// (#864). Rerouted as [`Self::storage_fetch`] documents.
    pub async fn lookup_onion_exits(
        &self,
        service: &str,
        include_expired: bool,
    ) -> Result<Vec<OnionExitDescriptor>> {
        let entry_key = entry::Entry::gen_did(ONION_EXITS_TOPIC)?;
        let service = service.trim();

        let Some(entry) = self.fetch_storage_entry(entry_key).await? else {
            return Ok(vec![]);
        };
        let exits = self.select_onion_exits_from_entry(&entry, service, include_expired);
        if include_expired
            || !exits.is_empty()
            || !self.entry_has_expired_onion_exit_service(&entry, service)
        {
            return Ok(exits);
        }

        let Some(refreshed_entry) = self
            .fetch_storage_entry_after_cache_refresh(entry_key, &entry)
            .await?
        else {
            return Ok(exits);
        };
        Ok(self.select_onion_exits_from_entry(&refreshed_entry, service, include_expired))
    }

    /// Select the newest live exit registration per `(DID, service)` of `entry` offering
    /// `service` (every service when empty).
    fn select_onion_exits_from_entry(
        &self,
        entry: &entry::Entry,
        service: &str,
        include_expired: bool,
    ) -> Vec<OnionExitDescriptor> {
        OnionExitDescriptor::latest_valid_by_service_did(
            Self::onion_exit_descriptors_from_entry(entry),
            get_epoch_ms(),
            self.swarm.network_id(),
            include_expired,
        )
        .into_iter()
        .filter(|descriptor| service.is_empty() || descriptor.offers_service(service))
        .collect()
    }

    pub(crate) async fn fetch_storage_entry(&self, entry_key: Did) -> Result<Option<entry::Entry>> {
        let stop = StopToken::never();
        self.fetch_storage_entry_with_stop(entry_key, &stop).await
    }

    /// Fetch `entry_key` and answer from the cache, the join of every reply observed (#864).
    ///
    /// A failed fetch is the empty read ([`Self::answer_failed_fetch`]). This serves the
    /// directory lookups and the registration publisher alike: a publisher whose fetch fails
    /// chooses what to tombstone from its cached observation instead of abandoning the attempt,
    /// and its own replaced values are still caught by its record of what it published.
    pub(crate) async fn fetch_storage_entry_with_stop(
        &self,
        entry_key: Did,
        stop: &StopToken,
    ) -> Result<Option<entry::Entry>> {
        if stop.should_stop() {
            return Err(Error::RegistrationStopped);
        }
        match self
            .swarm
            .scoped_storage(stop.clone())
            .storage_fetch(entry_key)
            .await
            .map_err(stoppable_storage_error)
        {
            Ok(()) => {}
            // A stop is the caller's cooperative stop, not a failed read.
            Err(Error::RegistrationStopped) => return Err(Error::RegistrationStopped),
            Err(error) => return self.answer_failed_fetch(entry_key, error).await,
        }
        for attempt in 0..DHT_LOOKUP_CACHE_POLL_ATTEMPTS {
            if stop.should_stop() {
                return Err(Error::RegistrationStopped);
            }
            if let Some(entry) = self.storage_check_cache(entry_key).await {
                return Ok(Some(entry));
            }
            if attempt + 1 == DHT_LOOKUP_CACHE_POLL_ATTEMPTS {
                break;
            }
            sleep(DHT_LOOKUP_CACHE_POLL_INTERVAL).await?;
        }
        Ok(None)
    }

    fn entry_has_expired_onion_exit_service(&self, entry: &entry::Entry, service: &str) -> bool {
        let now_ms = get_epoch_ms();
        Self::onion_exit_descriptors_from_entry(entry)
            .into_iter()
            .any(|descriptor| {
                (service.is_empty() || descriptor.offers_service(service))
                    && descriptor.verify_signature(self.swarm.network_id())
                    && descriptor.is_expired_at(now_ms)
            })
    }

    /// Answer a fetch of `entry_key` that failed with `error` as the empty read,
    /// `cache ⊔ ∅ = cache`: from the live cached carrier, and with `error` only when there is none.
    async fn answer_failed_fetch(
        &self,
        entry_key: Did,
        error: Error,
    ) -> Result<Option<entry::Entry>> {
        let cached = self.storage_check_cache(entry_key).await;
        if cached.is_some() {
            tracing::debug!(%entry_key, %error, "storage fetch failed; answering from the cache");
        }
        cached.map(Some).ok_or(error)
    }

    /// Fetch `entry_key` again and wait for the cached carrier to move past `previous_entry`.
    ///
    /// A failed fetch is the empty read ([`Self::answer_failed_fetch`]): nothing new arrived, so
    /// the cached carrier is the answer.
    async fn fetch_storage_entry_after_cache_refresh(
        &self,
        entry_key: Did,
        previous_entry: &entry::Entry,
    ) -> Result<Option<entry::Entry>> {
        if let Err(error) = self.storage_fetch(entry_key).await {
            return self.answer_failed_fetch(entry_key, error).await;
        }
        for _ in 0..DHT_LOOKUP_CACHE_POLL_ATTEMPTS {
            sleep(DHT_LOOKUP_CACHE_POLL_INTERVAL).await?;
            let Some(entry) = self.storage_check_cache(entry_key).await else {
                continue;
            };
            if &entry != previous_entry {
                return Ok(Some(entry));
            }
        }
        Ok(self.storage_check_cache(entry_key).await)
    }

    /// Build an onion proxy route for a client target through a target-agnostic proxy config.
    pub async fn build_onion_proxy_route(
        &self,
        proxy: OnionProxyConfig,
        target: OnionProxyTarget,
    ) -> Result<OnionProxyRoute> {
        directory::build_onion_proxy_route(self, proxy, target).await
    }

    async fn run_registration_once(
        &self,
        task: &dyn RegistrationTask,
        stop: StopToken,
    ) -> Result<()> {
        let context = self.registration_context_with_stop(stop);
        task.register_once(&context).await
    }

    async fn registration_task_daemon_with(
        &self,
        task: &dyn RegistrationTask,
        stop: StopToken,
        sibling_stop: StopToken,
    ) {
        loop {
            if stop.should_stop() || sibling_stop.should_stop() {
                return;
            }
            if let Err(error) = self.run_registration_once(task, stop.clone()).await {
                if matches!(error, Error::RegistrationStopped) {
                    tracing::debug!(
                        "Stopping {} registration task after cooperative stop",
                        task.name()
                    );
                    return;
                }
                tracing::warn!("Failed to run {} registration task: {error:?}", task.name());
            }
            if stop.should_stop() || sibling_stop.should_stop() {
                return;
            }
            match sleep_registration_interval_with_stop(task.interval(), &stop, &sibling_stop).await
            {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!(
                        "Stopping {} registration task after timer error: {error:?}",
                        task.name()
                    );
                    return;
                }
            }
        }
    }

    async fn registration_daemons_with(&self, stop: StopToken, sibling_stop: StopToken) {
        join_all(self.registration_tasks.iter().map(|task| {
            self.registration_task_daemon_with(task.as_ref(), stop.clone(), sibling_stop.clone())
        }))
        .await;
    }

    /// Run stabilization and node registration tasks until this future is dropped or aborted.
    ///
    /// This is a long-running task; do not await completion as a readiness signal. Concurrent
    /// calls on this processor queue, and each call retains ownership through cleanup.
    pub async fn listen(&self) {
        self.listen_with(StopToken::never()).await;
    }

    /// Run stabilization and node registration tasks until `stop` asks them to exit.
    ///
    /// Concurrent calls on this processor queue, including calls through any clone or
    /// provider wrapper. A stop requested while queued is observed after that call acquires
    /// ownership, so its cleanup cannot overlap the current generation.
    ///
    /// The shutdown is cooperative: it waits for the current stabilization or
    /// registration operation to finish before returning. This avoids dropping
    /// browser IndexedDB request futures while their JavaScript callbacks are
    /// still pending.
    pub async fn listen_with(&self, stop: StopToken) {
        self.listen_with_started(stop, || {}).await;
    }

    /// Run one serialized listener generation, notifying `on_started` only after it owns
    /// the processor lifecycle lock. Ownership remains held through cleanup and measurement
    /// flushing. Browser promises use this signal to preserve queued-start semantics while
    /// sharing ownership with native callers.
    pub(crate) async fn listen_with_started<F>(&self, stop: StopToken, on_started: F)
    where F: FnOnce() {
        let _listener_lifecycle_guard = self.listener_lifecycle_lock.lock().await;
        on_started();
        self.run_listener_generation(stop).await;
    }

    /// Run maintenance and registration loops for the generation holding the lifecycle lock.
    async fn run_listener_generation(&self, stop: StopToken) {
        let stabilizer = Arc::new(self.swarm.stabilizer());
        if self.registration_tasks.is_empty() {
            stabilizer.wait_with(self.stabilize_interval, stop).await;
        } else {
            let registration_stop_source = StopSource::new();
            let registration_stop = registration_stop_source.token();
            let stabilizer_stop = stop.clone();
            let stabilization = async {
                stabilizer
                    .wait_with(self.stabilize_interval, stabilizer_stop)
                    .await;
                registration_stop_source.request_stop();
            };
            let _ = futures::future::join(
                stabilization,
                self.registration_daemons_with(stop, registration_stop),
            )
            .await;
        }
        if let Err(error) = self.flush_measurements().await {
            tracing::error!(%error, "failed to flush measurements during graceful shutdown");
        }
    }

    /// Return the processor's listener lifecycle lock for lifecycle tests.
    #[cfg(test)]
    pub(crate) fn listener_lifecycle_lock_for_test(&self) -> Arc<futures::lock::Mutex<()>> {
        self.listener_lifecycle_lock.clone()
    }

    /// Flush all applied measurement updates with the graceful-shutdown deadline.
    pub async fn flush_measurements(&self) -> Result<()> {
        if let Some(measure) = &self.measure {
            measure
                .flush_with_timeout(MEASUREMENT_FLUSH_TIMEOUT)
                .await?;
        }
        Ok(())
    }

    #[cfg(all(test, feature = "ffi"))]
    pub(crate) async fn record_authenticated_measurement_for_test(
        &self,
        peer: Did,
    ) -> std::result::Result<(), rings_core::measure::MeasureError> {
        if let Some(measure) = &self.measure {
            rings_core::measure::Measure::record(
                measure.as_ref(),
                peer,
                rings_core::measure::Authentication::Authenticated,
                rings_core::measure::MeasurementEvent::Connected,
            )
            .await?;
        }
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
        tracing::trace!("send_message, message size: {:?}", msg.len());

        let msg = Message::custom(msg).map_err(Error::SendMessage)?;

        self.swarm
            .send_message(msg, destination)
            .await
            .map_err(Error::SendMessage)
    }

    /// Send a custom message to an already connected peer without Chord routing.
    ///
    /// Protocols with their own authenticated hop selection, such as onion circuits, use this
    /// to keep the core transport from replacing their selected next hop.
    pub async fn send_direct_message(&self, destination: Did, msg: &[u8]) -> Result<uuid::Uuid> {
        tracing::trace!("send_direct_message, message size: {:?}", msg.len());

        let msg = Message::custom(msg).map_err(Error::SendMessage)?;

        self.swarm
            .send_direct_message(msg, destination)
            .await
            .map_err(Error::SendMessage)
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

    /// Send a namespaced envelope directly to an already connected peer.
    ///
    /// This bypasses Chord routing while retaining the normal custom-message envelope codec.
    pub async fn send_direct_envelope(
        &self,
        destination: Did,
        envelope: &crate::extension::ext::Envelope,
    ) -> Result<uuid::Uuid> {
        let msg_bytes = envelope.encode()?;
        self.send_direct_message(destination, &msg_bytes).await
    }

    /// check local cache of dht
    pub async fn storage_check_cache(&self, entry_key: Did) -> Option<entry::Entry> {
        self.swarm.storage_check_cache(entry_key).await
    }

    /// Fetch an entry from DHT storage
    ///
    /// # Rerouting
    ///
    /// Every DHT operation of this processor (fetch, store, append, tombstone, compact, and the
    /// registry lookups and writes built on them) is rerouted as core's [`ChordStorageInterface`]
    /// documents (#859): refusals before acceptance are retried on events within a budget, and
    /// no placement is applied twice. A caller that must bound the wait drops the operation.
    pub async fn storage_fetch(&self, entry_key: Did) -> Result<()> {
        self.swarm
            .storage_fetch(entry_key)
            .await
            .map_err(Error::EntryError)
    }

    /// Store an entry on DHT storage
    ///
    /// Rerouted as [`Self::storage_fetch`] documents.
    pub async fn storage_store(&self, entry: entry::Entry) -> Result<()> {
        self.swarm
            .storage_store(entry)
            .await
            .map_err(Error::EntryError)
    }

    /// Append data to an entry on DHT storage
    ///
    /// Rerouted as [`Self::storage_fetch`] documents.
    pub async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.swarm
            .storage_append_data(topic, data)
            .await
            .map_err(Error::EntryError)
    }

    /// Tombstone observed data in an entry on DHT storage.
    ///
    /// Rerouted as [`Self::storage_fetch`] documents.
    pub async fn storage_tombstone_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.swarm
            .storage_tombstone_data(topic, data)
            .await
            .map_err(Error::EntryError)
    }

    /// Compact observed data in an entry on DHT storage.
    ///
    /// Rerouted as [`Self::storage_fetch`] documents.
    pub async fn storage_compact_data(&self, topic: &str, removals: Vec<Encoded>) -> Result<()> {
        self.swarm
            .storage_compact_data(topic, removals)
            .await
            .map_err(Error::EntryError)
    }

    /// Return local measurement counters for a peer, if observed.
    pub async fn peer_measurement(&self, did: Did) -> Option<PeerMeasurement> {
        self.swarm.peer_measurement(did).await
    }

    /// Return every retained local peer measurement.
    pub async fn peer_measurements(&self) -> Vec<PeerMeasurement> {
        let mut measurements = self.swarm.peer_measurements().await;
        measurements.sort_by_key(|measurement| measurement.did);
        measurements
    }

    /// Return one bounded page of retained local peer measurements.
    pub async fn peer_measurements_page(
        &self,
        after: Option<Did>,
        limit: NonZeroUsize,
    ) -> PeerMeasurementPage {
        self.swarm.peer_measurements_page(after, limit).await
    }

    /// Return one bounded page of locally collected provisional receipts.
    pub async fn provisional_evidence_page(
        &self,
        after: Option<EvidenceDigest>,
        limit: NonZeroUsize,
    ) -> std::result::Result<EvidencePage<Did>, EvidenceError> {
        let Some(measure) = &self.measure else {
            return Err(EvidenceError::StorageUnavailable);
        };
        rings_core::measure::Measure::provisional_evidence_page(measure.as_ref(), after, limit)
            .await
    }

    /// Return aggregate receipt admission and eviction counters without account labels.
    pub async fn provisional_evidence_counters(&self) -> EvidenceCounters {
        match &self.measure {
            Some(measure) => {
                rings_core::measure::Measure::provisional_evidence_counters(measure.as_ref()).await
            }
            None => EvidenceCounters::default(),
        }
    }

    /// Return aggregate final-destination origin-quota drops by lane and reason.
    pub fn origin_quota_counters(&self) -> OriginQuotaCounters {
        self.swarm.origin_quota_counters()
    }

    /// register service
    pub async fn register_service(&self, name: &str) -> Result<()> {
        let encoded_did = self
            .did()
            .to_string()
            .encode()
            .map_err(Error::ServiceRegisterError)?;
        self.storage_append_data(name, encoded_did)
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

    /// Assemble the documented v1 operator snapshot without serializing stored entry payloads.
    pub async fn operator_snapshot(&self) -> Result<OperatorSnapshot> {
        let generated_at_ms = get_epoch_ms();
        let runtime = self.observability.runtime_snapshot(generated_at_ms);
        let delegation = self.delegatee_key.delegation();
        let expires_at_ms = delegation.expires_at_ms();
        let mailbox = self
            .swarm
            .mailbox_storage_inspect()
            .await
            .map_err(Error::InternalError)?;
        let admitted_peer_count = u64::try_from(self.swarm.peers().len()).unwrap_or(u64::MAX);
        let has_admitted_peer = admitted_peer_count > 0;
        let dht = DHTInspect::inspect(&self.swarm.dht());
        let has_successor = !dht.successors.is_empty();
        let has_predecessor = dht.predecessor.is_some();
        let mut peer_ratings = self.peer_measurements().await;
        peer_ratings.sort_unstable_by_key(|measurement| measurement.did);
        peer_ratings.truncate(PEER_RATING_CAPACITY);
        let peer_ratings = peer_ratings
            .into_iter()
            .map(|measurement| PeerRatingSnapshot {
                peer: measurement.did.to_string(),
                reliability: peer_quality_name(measurement.quality),
                credit_score: measurement.credit_score.as_f64(),
                sent: measurement.evidence.sent,
                failed_to_send: measurement.evidence.failed_to_send,
                received: measurement.evidence.received,
                failed_to_receive: measurement.evidence.failed_to_receive,
            })
            .collect();

        Ok(OperatorSnapshot {
            schema_version: OPERATOR_SCHEMA_VERSION,
            generated_at_ms,
            process_started_at_ms: self.observability.process_started_at_ms(),
            counter_scope: "node_local_process_lifetime",
            messages: runtime.messages,
            recent_messages: runtime.recent_messages,
            session_key: SessionKeySnapshot {
                valid: !delegation.is_expired_at(generated_at_ms),
                created_at_ms: delegation.created_at_ms(),
                expires_at_ms,
                remaining_ms: expires_at_ms.saturating_sub(generated_at_ms),
                rotation_succeeded_total: 0,
                rotation_failed_total: 0,
                runtime_rotation_supported: false,
            },
            mailboxes: MailboxSnapshot {
                registered: mailbox.registered,
                held_messages: mailbox.held_messages,
                stored_total: runtime.messages.stored,
            },
            dht_lookups: runtime.dht_lookups,
            peer_ratings,
            health: HealthSnapshot {
                process_api_healthy: true,
                admitted_peer_count,
                has_admitted_peer,
                has_successor,
                has_predecessor,
                overlay_ready: has_admitted_peer && has_successor,
            },
        })
    }
}

/// Stable text form of one advisory local peer-quality class.
const fn peer_quality_name(quality: PeerQuality) -> &'static str {
    match quality {
        PeerQuality::Healthy => "healthy",
        PeerQuality::Unknown => "unknown",
        PeerQuality::Degraded => "degraded",
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait::async_trait(?Send))]
#[cfg_attr(
    not(all(feature = "browser", target_family = "wasm")),
    async_trait::async_trait
)]
impl OnionDirectoryReader for Processor {
    fn local_did(&self) -> Did {
        self.did()
    }

    fn dht_protocol_mode(&self) -> DhtProtocolMode {
        self.swarm.dht_protocol_mode()
    }

    async fn live_online_nodes(&self) -> Result<Vec<OnlineNodeDescriptor>> {
        self.lookup_online_nodes(false).await
    }

    async fn live_onion_exits(&self, service: &str) -> Result<Vec<OnionExitDescriptor>> {
        self.lookup_onion_exits(service, false).await
    }

    async fn peer_qualities(&self) -> Vec<(Did, PeerQuality)> {
        self.peer_measurements()
            .await
            .into_iter()
            .map(|measurement| (measurement.did, measurement.quality))
            .collect()
    }

    fn onion_entry_guards(&self) -> &OnionEntryGuards {
        self.onion_entry_guards.as_ref()
    }
}

#[cfg(all(test, feature = "node"))]
mod tests;
