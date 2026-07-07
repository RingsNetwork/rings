use super::config::validate_onion_role_config;
use super::*;

/// ProcessorBuilder is used to initialize a [Processor] instance.
pub struct ProcessorBuilder {
    pub(in crate::processor) network_id: u32,
    pub(in crate::processor) ice_servers: String,
    pub(in crate::processor) external_address: Option<String>,
    pub(in crate::processor) webrtc_udp_port_range: Option<WebrtcUdpPortRange>,
    pub(in crate::processor) session_sk: SessionSk,
    pub(in crate::processor) storage: Option<EntryStorage>,
    pub(in crate::processor) measure: Option<MeasureImpl>,
    pub(in crate::processor) stabilize_interval: Duration,
    pub(in crate::processor) online_node_heartbeat_interval: Duration,
    pub(in crate::processor) online_node_ttl: Duration,
    pub(in crate::processor) online_node_type: OnlineNodeType,
    pub(in crate::processor) advertise_presence: bool,
    pub(in crate::processor) dht_virtual_nodes: u16,
    pub(in crate::processor) advertise_onion_relay: bool,
    pub(in crate::processor) advertise_onion_exit: bool,
    pub(in crate::processor) onion_exit_heartbeat_interval: Duration,
    pub(in crate::processor) onion_exit_ttl: Duration,
    pub(in crate::processor) onion_exit_services: Vec<OnionExitService>,
    pub(in crate::processor) onion_exit_policy: OnionExitPolicy,
    pub(in crate::processor) registration_tasks: Vec<Arc<dyn RegistrationTask>>,
    pub(in crate::processor) dht_finger_table_size: usize,
    pub(in crate::processor) reassembly_limits: ReassemblyLimits,
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
        validate_onion_exit_registration_timing(
            config.advertise_onion_exit,
            config.onion_exit_heartbeat_interval,
            config.onion_exit_ttl,
        )?;
        validate_onion_role_config(
            config.advertise_presence,
            config.advertise_onion_relay,
            config.advertise_onion_exit,
            &config.onion_exit_services,
            &config.onion_exit_policy,
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
            dht_virtual_nodes: config.dht_virtual_nodes,
            advertise_onion_relay: config.advertise_onion_relay,
            advertise_onion_exit: config.advertise_onion_exit,
            onion_exit_heartbeat_interval: config.onion_exit_heartbeat_interval,
            onion_exit_ttl: config.onion_exit_ttl,
            onion_exit_services: config.onion_exit_services.clone(),
            onion_exit_policy: config.onion_exit_policy.clone(),
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

    /// Set whether listen() advertises this node as an onion relay.
    pub fn advertise_onion_relay(mut self, advertise: bool) -> Self {
        self.advertise_onion_relay = advertise;
        self
    }

    /// Set whether listen() publishes this node as an onion exit.
    pub fn advertise_onion_exit(mut self, advertise: bool) -> Self {
        self.advertise_onion_exit = advertise;
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
        let mut online_node_capabilities = Vec::new();
        if self.advertise_onion_relay {
            online_node_capabilities.push(ONION_RELAY_CAPABILITY.to_string());
        }
        let session_sk = self.session_sk.clone();
        let online_node_registration = OnlineNodeRegistration::new(
            self.online_node_heartbeat_interval,
            self.online_node_ttl,
            self.online_node_type.clone(),
            endpoint_hint,
            online_node_capabilities,
        );
        let mut registration_tasks = self.registration_tasks;
        if self.advertise_presence {
            online_node_registration.validate_enabled_schedule()?;
            registration_tasks.push(Arc::new(online_node_registration.clone()));
        }
        if self.advertise_onion_exit {
            let onion_exit_registration = OnionExitRegistration::new(
                self.onion_exit_heartbeat_interval,
                self.onion_exit_ttl,
                self.online_node_type,
                self.onion_exit_services,
                self.onion_exit_policy,
            );
            onion_exit_registration.validate_enabled_schedule()?;
            registration_tasks.push(Arc::new(onion_exit_registration));
        }

        let mut swarm_builder =
            SwarmBuilder::new(self.network_id, &self.ice_servers, storage, self.session_sk);
        swarm_builder = swarm_builder.dht_storage_redundancy(DATA_REDUNDANT);
        swarm_builder = swarm_builder.dht_finger_table_size(self.dht_finger_table_size);
        swarm_builder = swarm_builder.dht_virtual_nodes(self.dht_virtual_nodes);
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
            #[cfg(feature = "browser")]
            advertise_onion_relay: self.advertise_onion_relay,
            registration_tasks,
        })
    }
}
