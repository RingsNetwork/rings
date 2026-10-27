use rings_core::dht::RemovalStorage;

use super::config::validate_dht_virtual_nodes;
use super::config::validate_onion_role_config;
use super::*;
use crate::consts::DATA_REDUNDANT;
#[cfg(rings_native)]
use crate::dns_relay::ServiceOffer;
#[cfg(rings_native)]
use crate::dns_relay::ServiceRegistration;
use crate::mail::registration::MailOffer;
use crate::mail::registration::MailRegistration;
use crate::prelude::rings_core::dht::RelayReplication;

/// ProcessorBuilder is used to initialize a [Processor] instance.
pub struct ProcessorBuilder {
    pub(in crate::processor) network_id: u32,
    pub(in crate::processor) ice_servers: String,
    pub(in crate::processor) external_address: Option<String>,
    pub(in crate::processor) webrtc_udp_port_range: Option<WebrtcUdpPortRange>,
    pub(in crate::processor) delegatee_key: DelegateeKey,
    pub(in crate::processor) onion_process_epoch: OnionProcessEpoch,
    pub(in crate::processor) storage: Option<EntryStorage>,
    pub(in crate::processor) relay_storage: Option<EntryStorage>,
    pub(in crate::processor) removal_storage: Option<RemovalStorage>,
    pub(in crate::processor) relay_replication: Option<RelayReplication>,
    pub(in crate::processor) replay_storage: Option<ReplayStorage>,
    pub(in crate::processor) origin_quota: OriginQuotaConfig,
    pub(in crate::processor) onion_entry_guard_storage: Option<OnionEntryGuardStorage>,
    pub(in crate::processor) measure: Option<Arc<PeriodicMeasure>>,
    /// The release witnesses of the file stores the processor is built over (#1085).
    pub(in crate::processor) stores_released: StoresReleased,
    pub(in crate::processor) stabilize_interval: Duration,
    pub(in crate::processor) online_node_heartbeat_interval: Duration,
    pub(in crate::processor) online_node_ttl: Duration,
    pub(in crate::processor) online_node_type: OnlineNodeType,
    pub(in crate::processor) advertise_presence: bool,
    pub(in crate::processor) dht_virtual_nodes: u16,
    pub(in crate::processor) onion_role: OnionRole<OnionExitOffer>,
    pub(in crate::processor) onion_exit_heartbeat_interval: Duration,
    pub(in crate::processor) onion_exit_ttl: Duration,
    pub(in crate::processor) onion_idle_floor: OnionIdleFloor,
    pub(in crate::processor) dht_finger_table_size: usize,
    pub(in crate::processor) reassembly_limits: ReassemblyLimits,
    /// An observer chained after the processor's own `Observability`.
    pub(in crate::processor) observer: Option<rings_core::swarm::observer::SharedSwarmObserver>,
    /// The heartbeat announcing this node's services through DNS relays (#1049), if any.
    #[cfg(rings_native)]
    pub(in crate::processor) service_registration: Option<Arc<dyn RegistrationTask>>,
    /// The heartbeat of this node's mail descriptor (#1117), if it takes mail.
    pub(in crate::processor) mail_registration: Option<Arc<dyn RegistrationTask>>,
}

impl ProcessorBuilder {
    /// initialize a [ProcessorBuilder] with a serialized [ProcessorConfig].
    pub fn from_serialized(config: &str) -> Result<Self> {
        let config = serde_saphyr::from_str::<ProcessorConfig>(config)?;
        Self::from_config(&config)
    }

    /// initialize a [ProcessorBuilder] with a [ProcessorConfig].
    pub fn from_config(config: &ProcessorConfig) -> Result<Self> {
        Ok(Self {
            network_id: config.network_id,
            ice_servers: config.ice_servers.clone(),
            external_address: config.external_address.clone(),
            webrtc_udp_port_range: config.webrtc_udp_port_range()?,
            delegatee_key: config.delegatee_key.clone(),
            onion_process_epoch: OnionProcessEpoch::random(),
            storage: None,
            relay_storage: None,
            removal_storage: None,
            relay_replication: None,
            replay_storage: None,
            origin_quota: config.origin_quota,
            onion_entry_guard_storage: None,
            measure: None,
            stores_released: StoresReleased::default(),
            stabilize_interval: config.stabilize_interval,
            online_node_heartbeat_interval: config.online_node_heartbeat_interval,
            online_node_ttl: config.online_node_ttl,
            online_node_type: config.online_node_type.clone(),
            advertise_presence: config.advertise_presence,
            dht_virtual_nodes: config.dht_virtual_nodes,
            onion_role: config.onion_role.clone(),
            onion_exit_heartbeat_interval: config.onion_exit_heartbeat_interval,
            onion_exit_ttl: config.onion_exit_ttl,
            onion_idle_floor: OnionIdleFloor::DEFAULT,
            dht_finger_table_size: DEFAULT_FINGER_TABLE_SIZE,
            reassembly_limits: ReassemblyLimits::production(),
            observer: None,
            #[cfg(rings_native)]
            service_registration: None,
            mail_registration: None,
        })
    }

    /// Announce `offer` through DNS relays: the processor publishes this node's service
    /// descriptor on every heartbeat of the offer (#1049).
    #[cfg(rings_native)]
    pub fn announce_services(mut self, offer: ServiceOffer) -> Self {
        let did = self.delegatee_key.delegator_did();
        self.service_registration = Some(Arc::new(ServiceRegistration::new(did, offer)));
        self
    }

    /// Take mail at `did@domain` through mail gateways (#1117): the processor publishes this
    /// node's mail descriptor, which names its delegatee key, on every heartbeat of `offer`.
    pub fn receive_mail(mut self, offer: MailOffer) -> Self {
        let did = self.delegatee_key.delegator_did();
        self.mail_registration = Some(Arc::new(MailRegistration::new(did, offer)));
        self
    }

    /// Chain `observer` after the processor's own `Observability`, an open hook for embedders
    /// to observe the swarm's message and lookup activity. `SwarmBuilder::observer` offers the
    /// same for a bare swarm.
    ///
    /// Contract:
    /// - **Synchronous and bounded.** The observer is called synchronously on the send and
    ///   receive paths, so it must not block, wait or do slow work; see
    ///   [`SwarmObserver`](rings_core::swarm::observer::SwarmObserver).
    /// - **After the processor's recorder.** It runs after the processor's own
    ///   `Observability`, which alone feeds `/status?view=observability`, so installing an
    ///   observer leaves that status output unchanged.
    /// - **Bounded data.** Message observations carry no payload, key or identifier: only the
    ///   activity, the scheduling category, the compile-time message class and the outcome.
    ///   Lookup events carry a correlation key (a transaction id or a resource DID) so start and
    ///   finish can be paired; an exporter must aggregate or redact it, never use it as a label.
    /// - **One extra observer.** A later call replaces the observer set by an earlier one.
    pub fn observer(mut self, observer: rings_core::swarm::observer::SharedSwarmObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Set the storage for the processor.
    pub fn storage(mut self, storage: EntryStorage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Set the storage of the relay-inbox namespace (`SwarmBuilder::relay_storage`): a
    /// `RecordAuthority::Retained` store, so held mail survives a crash and is never evicted
    /// (#1021). Without one, the node takes custody of no relay carrier.
    pub fn relay_storage(mut self, storage: EntryStorage) -> Self {
        self.relay_storage = Some(storage);
        self
    }

    /// Set the storage of the data namespace's remove sides (`SwarmBuilder::removal_storage`):
    /// a `RecordAuthority::Retained` store, so a remove survives a crash and is never evicted
    /// before it is stable (#920). Without one, the remove sides are kept in memory.
    pub fn removal_storage(mut self, storage: RemovalStorage) -> Self {
        self.removal_storage = Some(storage);
        self
    }

    /// Set how relay custody is replicated (#777): the members `r` each carrier is replicated
    /// to, and how long the node is online before it holds relay mail. By default `r` is 8 and
    /// the age gate is 2 minutes, raised to four stabilization periods when those are longer
    /// (`Law (Age gate)` of durable-records); a value set here is taken as given.
    pub fn relay_replication(mut self, replication: RelayReplication) -> Self {
        self.relay_replication = Some(replication);
        self
    }

    /// Set durable destination-scoped transaction replay storage: an authoritative store.
    /// Without one, the processor refuses every replay transition, so it neither sends nor
    /// accepts a transaction (`SwarmBuilder::replay_storage`, #1022).
    pub fn replay_storage(mut self, storage: ReplayStorage) -> Self {
        self.replay_storage = Some(storage);
        self
    }

    /// Set final-destination per-origin message and byte-rate quotas.
    pub fn origin_quota(mut self, config: OriginQuotaConfig) -> Self {
        self.origin_quota = config;
        self
    }

    /// Set the local entry-guard storage for onion route selection.
    pub fn onion_entry_guard_storage(mut self, storage: OnionEntryGuardStorage) -> Self {
        self.onion_entry_guard_storage = Some(storage);
        self
    }

    /// Set the measure for the processor.
    pub fn measure(mut self, implement: PeriodicMeasure) -> Self {
        self.measure = Some(Arc::new(implement));
        self
    }

    /// Name the release witnesses of the file stores this processor is built over, which
    /// [`Provider::close`](crate::provider::Provider::close) awaits (#1085). A store given to the
    /// builder without its witness is not awaited by a close.
    pub fn stores_released(mut self, released: StoresReleased) -> Self {
        self.stores_released = released;
        self
    }

    /// Set the number of DHT finger-table slots for the processor's swarm.
    pub fn dht_finger_table_size(mut self, size: usize) -> Self {
        self.dht_finger_table_size = size;
        self
    }

    /// Set storage-only virtual positions derived per physical peer.
    ///
    /// Serialized configs reject values above
    /// [`rings_core::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER`]. This
    /// builder setter is infallible for direct programmatic use; the core swarm
    /// builder normalizes the value once before storage ownership and protocol
    /// advertisement are created.
    pub fn dht_virtual_nodes(mut self, positions_per_peer: u16) -> Self {
        self.dht_virtual_nodes = positions_per_peer;
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

    /// Set the cover floor of this node's idle onion links (#880); the default is one unit per
    /// second, and a constrained node (a browser) may lower it.
    pub fn onion_idle_floor(mut self, floor: OnionIdleFloor) -> Self {
        self.onion_idle_floor = floor;
        self
    }

    /// Set the onion symbols this process registers (#834 D2).
    pub fn onion_role(mut self, role: OnionRole<OnionExitOffer>) -> Self {
        self.onion_role = role;
        self
    }

    /// Build the [Processor].
    pub fn build(self) -> Result<Processor> {
        self.validate()?;
        self.delegatee_key
            .delegation()
            .verify_delegator_authorization()
            .map_err(|e| Error::VerifyError(e.to_string()))?;

        let storage = self.storage.unwrap_or_else(|| Box::new(MemStorage::new()));
        let onion_entry_guard_storage = self
            .onion_entry_guard_storage
            .unwrap_or_else(|| Box::new(MemStorage::new()));
        let endpoint_hint = self.external_address.clone();
        let onion_process_epoch = OnionProcessEpochCell::new(self.onion_process_epoch);
        let delegatee_key = self.delegatee_key.clone();
        let online_node_registration = OnlineNodeRegistration::new(
            self.online_node_heartbeat_interval,
            self.online_node_ttl,
            self.online_node_type.clone(),
            endpoint_hint,
            self.onion_role
                .registers_relay()
                .then(|| onion_process_epoch.clone()),
        );
        let mut registration_tasks: Vec<Arc<dyn RegistrationTask>> = Vec::new();
        if self.advertise_presence {
            registration_tasks.push(Arc::new(online_node_registration.clone()));
        }
        if let Some(offer) = self.onion_role.exit() {
            let onion_exit_registration = OnionExitRegistration::new(
                self.onion_exit_heartbeat_interval,
                self.onion_exit_ttl,
                self.online_node_type,
                offer.clone(),
                onion_process_epoch.clone(),
            );
            registration_tasks.push(Arc::new(onion_exit_registration));
        }
        #[cfg(rings_native)]
        registration_tasks.extend(self.service_registration);
        registration_tasks.extend(self.mail_registration);

        // One recorder is shared between core protocol hooks and node-level snapshot assembly.
        let observability = Arc::new(crate::observability::Observability::new());
        let mut swarm_builder = SwarmBuilder::new(
            self.network_id,
            &self.ice_servers,
            storage,
            self.delegatee_key,
        );
        swarm_builder = swarm_builder.dht_storage_redundancy(DATA_REDUNDANT);
        swarm_builder = swarm_builder.dht_finger_table_size(self.dht_finger_table_size);
        swarm_builder = swarm_builder.dht_virtual_nodes(self.dht_virtual_nodes);
        swarm_builder = swarm_builder.reassembly_limits(self.reassembly_limits);
        if let Some(replay_storage) = self.replay_storage {
            swarm_builder = swarm_builder.replay_storage(replay_storage);
        }
        if let Some(relay_storage) = self.relay_storage {
            swarm_builder = swarm_builder.relay_storage(relay_storage);
        }
        if let Some(removal_storage) = self.removal_storage {
            swarm_builder = swarm_builder.removal_storage(removal_storage);
        }
        let stabilization_periods_ms =
            u64::try_from(self.stabilize_interval.saturating_mul(4).as_millis())
                .unwrap_or(u64::MAX);
        let relay_replication = self.relay_replication.unwrap_or_else(|| {
            let default = RelayReplication::default();
            RelayReplication {
                age_gate_ms: default.age_gate_ms.max(stabilization_periods_ms),
                ..default
            }
        });
        swarm_builder = swarm_builder.relay_replication(relay_replication);
        swarm_builder = swarm_builder.origin_quota(self.origin_quota);
        let observer: rings_core::swarm::observer::SharedSwarmObserver = match self.observer {
            Some(extra) => Arc::new(ChainedObserver {
                first: observability.clone(),
                second: extra,
            }),
            None => observability.clone(),
        };
        swarm_builder = swarm_builder.observer(observer);

        if let Some(external_address) = self.external_address {
            swarm_builder = swarm_builder.external_address(external_address);
        }
        if let Some(range) = self.webrtc_udp_port_range {
            swarm_builder = swarm_builder.webrtc_udp_port_range(range);
        }

        let measure = self.measure;
        if let Some(measure) = &measure {
            let implementation: MeasureImpl = measure.clone();
            swarm_builder = swarm_builder.measure(implementation);
        }
        let swarm = Arc::new(swarm_builder.build());

        Ok(Processor {
            swarm,
            delegatee_key,
            onion_process_epoch,
            onion_entry_guards: Arc::new(OnionEntryGuards::new(onion_entry_guard_storage)),
            stabilize_interval: self.stabilize_interval,
            online_node_registration,
            measure,
            stores_released: self.stores_released,
            tasks: NodeTasks::new(),
            listener_lifecycle_lock: Arc::new(futures::lock::Mutex::new(())),
            onion_role: self.onion_role,
            onion_idle_floor: self.onion_idle_floor,
            registration_readiness: Arc::new(RegistrationReadiness::new(registration_tasks.len())),
            registration_tasks,
            observability,
        })
    }

    /// Validate the complete builder state at the single runtime construction boundary.
    ///
    /// Performing these checks after all builder mutations guarantees that every
    /// native, browser, and programmatic entrance rejects the same illegal role,
    /// schedule, and virtual-node combinations.
    fn validate(&self) -> Result<()> {
        validate_dht_virtual_nodes(self.dht_virtual_nodes)?;
        validate_registration_timing(
            "online_node",
            "advertise_presence",
            self.advertise_presence,
            self.online_node_heartbeat_interval,
            self.online_node_ttl,
        )?;
        validate_registration_timing(
            "onion_exit",
            "advertise_onion_exit",
            self.onion_role.exit().is_some(),
            self.onion_exit_heartbeat_interval,
            self.onion_exit_ttl,
        )?;
        validate_onion_role_config(self.advertise_presence, &self.onion_role)
    }
}

/// An observer that forwards every observation to two observers, in order.
struct ChainedObserver {
    first: rings_core::swarm::observer::SharedSwarmObserver,
    second: rings_core::swarm::observer::SharedSwarmObserver,
}

impl rings_core::swarm::observer::SwarmObserver for ChainedObserver {
    fn observe_message(&self, observation: rings_core::swarm::observer::MessageObservation) {
        self.first.observe_message(observation);
        self.second.observe_message(observation);
    }

    fn lookup_started(
        &self,
        kind: rings_core::swarm::observer::LookupKind,
        correlation: rings_core::swarm::observer::LookupCorrelation,
    ) {
        self.first.lookup_started(kind, correlation);
        self.second.lookup_started(kind, correlation);
    }

    fn lookup_finished(
        &self,
        kind: rings_core::swarm::observer::LookupKind,
        correlation: rings_core::swarm::observer::LookupCorrelation,
        outcome: rings_core::swarm::observer::LookupOutcome,
    ) {
        self.first.lookup_finished(kind, correlation, outcome);
        self.second.lookup_finished(kind, correlation, outcome);
    }
}
