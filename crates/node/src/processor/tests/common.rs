use super::*;

// Native WebRTC tests share process-global ICE/UDP resources and timing-sensitive
// connection callbacks; run them serially so one test's candidates or callbacks
// cannot add pressure to another test's handshake.
static NETWORK_TEST_LOCK: OnceLock<AsyncTestMutex<()>> = OnceLock::new();

pub(super) fn onion_policy(
    allowed_targets: &[&str],
    denied_targets: &[&str],
) -> Result<OnionExitPolicy> {
    OnionExitPolicy::from_target_strings(
        allowed_targets
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
        denied_targets
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
    )
}
pub(super) struct SwarmCallbackInstance {
    inbound: Mutex<Vec<Message>>,
    inbound_notify: Notify,
    connected_notify: Notify,
}

pub(super) struct StaticRegistration {
    publisher: crate::registration::DhtRegistrationPublisher,
    value: Encoded,
}

impl StaticRegistration {
    pub(super) fn new(topic: &str, value: Encoded) -> Self {
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

pub(super) fn test_callback() -> Arc<SwarmCallbackInstance> {
    Arc::new(SwarmCallbackInstance {
        inbound: Mutex::new(Vec::new()),
        inbound_notify: Notify::new(),
        connected_notify: Notify::new(),
    })
}

pub(super) async fn network_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    NETWORK_TEST_LOCK
        .get_or_init(|| AsyncTestMutex::new(()))
        .lock()
        .await
}

pub(super) async fn prepare_processor_with_identity_key(identity_key: SecretKey) -> Processor {
    prepare_processor_with_identity_key_and_network(identity_key, 0).await
}

pub(super) async fn prepare_processor_with_identity_key_and_network(
    identity_key: SecretKey,
    network_id: u32,
) -> Processor {
    prepare_processor_with_identity_key_network_and_virtual_nodes(identity_key, network_id, {
        rings_core::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    })
    .await
}

pub(super) async fn prepare_processor_with_identity_key_network_and_virtual_nodes(
    identity_key: SecretKey,
    network_id: u32,
    dht_virtual_nodes: u16,
) -> Processor {
    let session_sk = SessionSk::new_with_seckey(&identity_key).unwrap();
    let config = ProcessorConfig::new(
        network_id,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    )
    .dht_virtual_nodes(dht_virtual_nodes);
    let storage = Box::new(MemStorage::new());

    ProcessorBuilder::from_config(&config)
        .unwrap()
        .storage(storage)
        .dht_finger_table_size(8)
        .build()
        .unwrap()
}

pub(super) async fn prepare_online_node_registry_pair(
    network_id: u32,
) -> Result<(Processor, Processor)> {
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
        let first_owns_all = owns_all_placements(first_did, second_did, placement_keys.as_slice());
        let second_owns_all = owns_all_placements(second_did, first_did, placement_keys.as_slice());
        let Some((publisher_key, owner_key)) = (match (first_owns_all, second_owns_all) {
            (true, false) => Some((second_key, first_key)),
            (false, true) => Some((first_key, second_key)),
            _ => None,
        }) else {
            continue;
        };
        let publisher = prepare_processor_with_identity_key_network_and_virtual_nodes(
            publisher_key,
            network_id,
            0,
        )
        .await;
        let owner =
            prepare_processor_with_identity_key_network_and_virtual_nodes(owner_key, network_id, 0)
                .await;
        return Ok((publisher, owner));
    }
    Err(Error::InvalidConfig(
        "could not generate an online-node registry owner covering every placement".to_string(),
    ))
}

pub(super) fn owns_all_placements(local: Did, successor: Did, placements: &[Did]) -> bool {
    placements
        .iter()
        .all(|placement| *placement - local <= successor - local)
}

pub(super) async fn prepare_processor_with_network(network_id: u32) -> Processor {
    prepare_processor_with_network_and_virtual_nodes(network_id, 0).await
}

pub(super) async fn prepare_processor_with_network_and_virtual_nodes(
    network_id: u32,
    dht_virtual_nodes: u16,
) -> Processor {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        network_id,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    )
    .dht_virtual_nodes(dht_virtual_nodes);
    let config = ProcessorConfig::try_from(serialized).unwrap();
    let storage = Box::new(MemStorage::new());

    ProcessorBuilder::from_config(&config)
        .unwrap()
        .storage(storage)
        .dht_finger_table_size(8)
        .build()
        .unwrap()
}

pub(super) fn owns_entry_placement(processor: &Processor, placement_key: Did) -> Result<bool> {
    match processor.swarm.dht().find_successor(placement_key)? {
        PeerRingAction::Some(_) => Ok(true),
        PeerRingAction::RemoteAction(_, PeerRingRemoteAction::FindSuccessor(_)) => Ok(false),
        action => Err(Error::InvalidConfig(format!(
            "unexpected registry owner lookup action: {action:?}"
        ))),
    }
}

pub(super) async fn prepare_processor_with_online_node_type(
    node_type: OnlineNodeType,
) -> Processor {
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

pub(super) fn onion_exit_descriptor_for_processor(
    processor: &Processor,
    service: &str,
    now_ms: u128,
) -> Result<OnionExitDescriptor> {
    onion_exit_descriptor_for_processor_with_policy(processor, service, now_ms, {
        let mut policy = onion_policy(&["127.0.0.1:8080", "example.com:443"], &[])?;
        policy.max_circuits = 8;
        policy.max_streams_per_circuit = 2;
        policy.max_bytes_per_minute = 4096;
        policy
    })
}

pub(super) fn onion_exit_descriptor_for_processor_with_policy(
    processor: &Processor,
    service: &str,
    now_ms: u128,
    policy: OnionExitPolicy,
) -> Result<OnionExitDescriptor> {
    onion_exit_descriptor_for_processor_with_service(
        processor,
        OnionExitService::new(
            service,
            OnionExitService::reserved_transport(service).unwrap_or(OnionExitTransport::Tcp),
        )?,
        now_ms,
        policy,
    )
}

pub(super) fn onion_exit_descriptor_for_processor_with_service(
    processor: &Processor,
    service: OnionExitService,
    now_ms: u128,
    policy: OnionExitPolicy,
) -> Result<OnionExitDescriptor> {
    OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did: processor.did(),
            public_key: processor
                .swarm
                .account_verification_pubkey()
                .map_err(Error::CoreError)?,
            session_public_key: processor.session_sk.session_public_key(),
            node_type: default_online_node_type(),
            network_id: processor.swarm.network_id(),
            service,
            policy,
            started_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            expires_at_ms: now_ms + 90_000,
            version: crate::util::build_version(),
        },
        &processor.session_sk,
    )
    .map_err(Error::CoreError)
}

pub(super) fn online_relay_descriptor_for_processor(
    processor: &Processor,
    now_ms: u128,
) -> Result<OnlineNodeDescriptor> {
    let mut capabilities = OnlineNodeRegistration::default_capabilities();
    capabilities.push(ONION_RELAY_CAPABILITY.to_string());
    OnlineNodeDescriptor::new_signed(
        OnlineNodeDescriptorBody {
            did: processor.did(),
            public_key: processor
                .swarm
                .account_verification_pubkey()
                .map_err(Error::CoreError)?,
            session_public_key: processor.session_sk.session_public_key(),
            node_type: default_online_node_type(),
            network_id: processor.swarm.network_id(),
            storage_redundancy: processor.swarm.storage_redundancy(),
            dht_virtual_nodes: processor.swarm.dht_virtual_nodes(),
            capabilities,
            endpoint_hint: None,
            started_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            expires_at_ms: now_ms + 90_000,
            version: crate::util::build_version(),
        },
        &processor.session_sk,
    )
    .map_err(Error::CoreError)
}

pub(super) fn mismatched_storage_redundancy(value: u16) -> u16 {
    if value == u16::MAX {
        value.saturating_sub(1)
    } else {
        value.saturating_add(1)
    }
}

pub(super) async fn prepare_measured_processor() -> Processor {
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

pub(super) async fn connect_processors(
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

pub(super) async fn wait_processors_connected(
    p1: &Processor,
    p2: &Processor,
    callback1: &SwarmCallbackInstance,
    callback2: &SwarmCallbackInstance,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if processor_has_connected_peer(p1, p2.did()) && processor_has_connected_peer(p2, p1.did())
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

pub(super) fn processor_has_connected_peer(processor: &Processor, peer: Did) -> bool {
    let peer = peer.to_string();
    processor
        .swarm
        .peers()
        .into_iter()
        .any(|conn| conn.did == peer && conn.state == "Connected")
}

pub(super) async fn wait_for_mutual_dht_topology(
    processor: &Processor,
    other: &Processor,
) -> Result<()> {
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

pub(super) async fn wait_for_online_node_dids(
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

pub(super) async fn wait_for_online_node_dids_in_storage(
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

pub(super) async fn wait_for_peer_measurement(
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

pub(super) async fn wait_for_inbound_message(
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

pub(super) async fn wait_for_e2e_stream_frames(
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
