#[cfg(feature = "dummy")]
use rings_core::dht::entry::EntryKind;
use rings_core::message::MessageSigner;

#[cfg(feature = "dummy")]
use super::controlled::ControlledNetwork;
use super::*;
#[cfg(feature = "dummy")]
use crate::consts::DATA_REDUNDANT;
use crate::tests::activity::probe_on_activity;
use crate::tests::activity::record_activity;
use crate::tests::exit_descriptor::ExitDescriptorFixture;
#[cfg(not(feature = "dummy"))]
use crate::tests::native::network_test_guard as native_network_lock;
#[cfg(feature = "dummy")]
use crate::tests::native::signal_edge;
use crate::tests::test_processor_builder;
use crate::tests::test_processor_config;

/// The policy of `allowed_targets` and `denied_targets`, bounded to 8 sessions and 4 KiB a
/// minute.
pub(super) fn bounded_policy(
    allowed_targets: &[&str],
    denied_targets: &[&str],
) -> Result<OnionExitPolicy> {
    let mut policy = onion_policy(allowed_targets, denied_targets)?;
    policy.max_sessions = 8;
    policy.max_bytes_per_minute = 4096;
    Ok(policy)
}

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

/// The application of a test processor: claims the processor's application inbox and records
/// every application message read from it, in arrival order.
///
/// Every item read, message or peer fact, records activity, so helpers probing what the
/// application received, or what an admission or retirement changed, wake on it.
pub(super) struct ApplicationRecorder {
    /// The application data read so far, in arrival order.
    inbound: Arc<Mutex<Vec<ApplicationData>>>,
    /// The task reading the inbox; aborted with the recorder.
    reader: tokio::task::JoinHandle<()>,
}

impl ApplicationRecorder {
    /// Claim `processor`'s application inbox and start recording it. The inbox is claimable
    /// once, so a processor has at most one recorder.
    pub(super) fn attach(processor: &Processor) -> Arc<Self> {
        let inbox = processor
            .swarm
            .application_inbox()
            .expect("the test processor's inbox is unclaimed");
        let inbound = Arc::new(Mutex::new(Vec::new()));
        let recorded = inbound.clone();
        let reader = tokio::spawn(async move {
            while let Some(delivery) = inbox.next().await {
                if let ApplicationDelivery::Message(message) = delivery {
                    recorded.lock().unwrap().push(message.data);
                }
                record_activity();
            }
        });
        Arc::new(Self { inbound, reader })
    }
}

impl Drop for ApplicationRecorder {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Exclusive use of the test network for one test.
///
/// Default build: serializes the real-WebRTC tests. `dummy` build: runs the test on its own
/// controlled in-memory network (see [`ControlledNetwork`]), a FIFO schedule on which no
/// message waits on a clock; it shares nothing with another test's network, so it takes no lock.
pub(super) struct NetworkTestGuard {
    /// The controlled network of this test.
    #[cfg(feature = "dummy")]
    pub(super) network: ControlledNetwork,
    /// Serializes tests that share process-global network resources.
    #[cfg(not(feature = "dummy"))]
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

/// Take exclusive use of the test network for the calling test; see [`NetworkTestGuard`].
///
/// Default build: the serial lock is the crate's one native network lock, shared with the onion
/// loop tests.
pub(super) async fn network_test_guard() -> NetworkTestGuard {
    NetworkTestGuard {
        #[cfg(feature = "dummy")]
        network: ControlledNetwork::start(),
        #[cfg(not(feature = "dummy"))]
        _serial: native_network_lock().await,
    }
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
    test_processor_builder(&test_processor_config(&identity_key, network_id))
        .dht_virtual_nodes(dht_virtual_nodes)
        .build()
        .unwrap()
}

#[cfg(feature = "dummy")]
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

#[cfg(feature = "dummy")]
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
    prepare_processor_with_identity_key_network_and_virtual_nodes(
        SecretKey::random(),
        network_id,
        dht_virtual_nodes,
    )
    .await
}

#[cfg(feature = "dummy")]
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
    test_processor_builder(&test_processor_config(&SecretKey::random(), 0))
        .online_node_type(node_type)
        .build()
        .unwrap()
}

/// `processor`'s live registration of `service`, allowing `127.0.0.1:8080` and
/// `example.com:443` under [`bounded_policy`].
pub(super) fn onion_exit_descriptor_for_processor(
    processor: &Processor,
    service: &str,
    now_ms: u128,
) -> Result<OnionExitDescriptor> {
    ExitDescriptorFixture::live(
        processor,
        OnionServiceName::parse(service)?,
        bounded_policy(&["127.0.0.1:8080", "example.com:443"], &[])?,
        now_ms,
    )
    .sign()
}

pub(super) fn online_relay_descriptor_for_processor(
    processor: &Processor,
    now_ms: u128,
) -> Result<OnlineNodeDescriptor> {
    let capabilities = OnlineNodeCapabilities::onion_relay(processor.onion_process_epoch.get());
    OnlineNodeDescriptor::new_signed(
        OnlineNodeDescriptorBody {
            did: processor.did(),
            public_key: processor
                .swarm
                .delegator_verification_pubkey()
                .map_err(Error::CoreError)?,
            delegatee_public_key: processor.delegatee_key.delegatee_public_key(),
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
        MessageSigner::new(&processor.delegatee_key, processor.swarm.network_id()),
    )
    .map_err(Error::CoreError)
}

/// Store in `processor`'s online-node registry the relay registrations of `exits` and of
/// `relays` fresh relay processors, which are returned. An onion loop over one symbol needs
/// `H − 1 = 4` distinct relay registrants, the exit included (#834 D5).
pub(super) async fn store_onion_relays(
    processor: &Processor,
    exits: &[&Processor],
    relays: usize,
) -> Result<Vec<Processor>> {
    let mut relay_processors = Vec::with_capacity(relays);
    for _ in 0..relays {
        relay_processors.push(prepare_processor().await);
    }
    let now_ms = get_epoch_ms();
    let descriptors = exits
        .iter()
        .copied()
        .chain(relay_processors.iter())
        .map(|relay| online_relay_descriptor_for_processor(relay, now_ms))
        .collect::<Result<Vec<_>>>()?;
    processor
        .storage_store(Processor::registry_entry(ONLINE_NODES_TOPIC, descriptors)?)
        .await?;
    Ok(relay_processors)
}

pub(super) fn mismatched_storage_redundancy(value: u16) -> u16 {
    if value == u16::MAX {
        value.saturating_sub(1)
    } else {
        value.saturating_add(1)
    }
}

pub(super) async fn prepare_measured_processor() -> Processor {
    let measure = PeriodicMeasure::new(Box::new(MemStorage::new()))
        .await
        .unwrap();

    test_processor_builder(&test_processor_config(&SecretKey::random(), 0))
        .measure(measure)
        .build()
        .unwrap()
}

/// Signal an edge from `p1` to `p2` ([`signal_edge`]) and wait until both admit it.
#[cfg(feature = "dummy")]
pub(super) async fn connect_processors(p1: &Processor, p2: &Processor) {
    signal_edge(p1, p2).await.expect("the edge is signalled");
    wait_processors_connected(p1, p2).await;
}

/// Hang guard of every awaited processor-test state: a failure bound only. In `dummy` builds the
/// network is in memory; in the default build the two real-WebRTC smoke tests run webrtc-rs
/// handshakes, whose latency under suite load has exceeded 5 s (#850).
#[cfg(feature = "dummy")]
pub(super) const PROCESSOR_TEST_HANG_GUARD: Duration = Duration::from_secs(5);
#[cfg(not(feature = "dummy"))]
pub(super) const PROCESSOR_TEST_HANG_GUARD: Duration = Duration::from_secs(60);

/// Await, on activity, both processors admitting each other. Admission is announced as a
/// `PeerAdmitted` event, which an [`ApplicationRecorder`] records as activity.
pub(super) async fn wait_processors_connected(p1: &Processor, p2: &Processor) {
    probe_on_activity(
        "processors admitted each other",
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let admitted = processor_has_admitted_peer(p1, p2.did())
                && processor_has_admitted_peer(p2, p1.did());
            async move { Ok(admitted.then_some(())) }
        },
    )
    .await
    .unwrap();
}

pub(super) fn processor_has_admitted_peer(processor: &Processor, peer: Did) -> bool {
    processor.swarm.peer_dids().contains(&peer)
}

/// Run one stabilize round on both nodes, then await, on activity, the mutual successor and
/// predecessor view it produces.
///
/// A round is issued once: `begin_stabilization` supersedes an unanswered round,
/// so re-issuing while the head's report is in flight would make every report
/// stale and the head notify (the only predecessor-propagation path) never fire.
#[cfg(feature = "dummy")]
pub(super) async fn wait_for_mutual_dht_topology(
    processor: &Processor,
    other: &Processor,
) -> Result<()> {
    let stabilizer = processor.swarm.stabilizer();
    let other_stabilizer = other.swarm.stabilizer();
    futures::try_join!(stabilizer.stabilize(), other_stabilizer.stabilize(),)
        .map_err(Error::CoreError)?;
    let did = processor.did().to_string();
    let other_did = other.did().to_string();
    let (did, other_did) = (&did, &other_did);
    probe_on_activity(
        "mutual DHT topology",
        PROCESSOR_TEST_HANG_GUARD,
        || async move {
            let inspect = processor.swarm.inspect().await;
            let other_inspect = other.swarm.inspect().await;
            let processor_sees_other = inspect
                .dht
                .successors
                .iter()
                .any(|successor| successor == other_did)
                && inspect.dht.predecessor.as_ref() == Some(other_did);
            let other_sees_processor = other_inspect
                .dht
                .successors
                .iter()
                .any(|successor| successor == did)
                && other_inspect.dht.predecessor.as_ref() == Some(did);
            Ok((processor_sees_other && other_sees_processor).then_some(()))
        },
    )
    .await
}

/// Await, on activity, every placement in `processor`'s storage covering `expected`.
#[cfg(feature = "dummy")]
pub(super) async fn wait_for_online_node_dids_in_storage(
    processor: &Processor,
    placement_keys: &[Did],
    expected: &BTreeSet<Did>,
    context: &str,
) -> Result<()> {
    probe_on_activity(
        &format!("online node registry storage covers {expected:?} during {context}"),
        PROCESSOR_TEST_HANG_GUARD,
        || async move {
            for placement_key in placement_keys {
                let observed = match processor
                    .swarm
                    .dht()
                    .stored_entry_at(EntryKind::Data, *placement_key, get_epoch_ms())
                    .await
                    .map_err(Error::Storage)?
                {
                    Some(entry) => OnlineNodeRegistration::descriptors_from_entry(&entry)
                        .into_iter()
                        .map(|descriptor| descriptor.did)
                        .collect::<BTreeSet<_>>(),
                    None => BTreeSet::new(),
                };
                if !expected.is_subset(&observed) {
                    return Ok(None);
                }
            }
            Ok(Some(()))
        },
    )
    .await
}

/// Await, on activity, a measurement of `did` that satisfies `predicate`.
#[cfg(feature = "dummy")]
pub(super) async fn wait_for_peer_measurement(
    processor: &Processor,
    did: Did,
    predicate: impl Fn(&PeerMeasurement) -> bool,
) -> PeerMeasurement {
    let predicate = &predicate;
    probe_on_activity(
        "peer measurement updated",
        PROCESSOR_TEST_HANG_GUARD,
        || async move {
            Ok(processor
                .peer_measurement(did)
                .await
                .filter(|measurement| predicate(measurement)))
        },
    )
    .await
    .unwrap()
}

/// Await, on activity, application data read by `recorder` that satisfies `predicate`.
pub(super) async fn wait_for_application_data(
    recorder: &ApplicationRecorder,
    predicate: impl Fn(&ApplicationData) -> bool,
) -> ApplicationData {
    probe_on_activity(
        "application data delivered",
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let found = recorder
                .inbound
                .lock()
                .unwrap()
                .iter()
                .find(|msg| predicate(msg))
                .cloned();
            async move { Ok(found) }
        },
    )
    .await
    .unwrap()
}

/// Whether `frames` carry a complete stream: some final frame arrived, and so did every
/// sequence up to it.
///
/// The overlay link makes no ordering guarantee, so arrival order is irrelevant. With
/// `S = { f.sequence | f ∈ frames }`:
///
/// ```text
/// complete(frames) ≡ ∃ f ∈ frames. f.is_final ∧ {0, …, f.sequence} ⊆ S
/// ```
///
/// The predicate is monotone: `S` and the set of final frames only grow as frames arrive, so
/// once complete, further arrivals (duplicates or stray frames included) keep it complete.
pub(super) fn e2e_stream_complete<'a>(
    frames: impl IntoIterator<Item = &'a E2eStreamFrame>,
) -> bool {
    let (sequences, finals): (BTreeSet<u64>, Vec<u64>) = frames.into_iter().fold(
        (BTreeSet::new(), Vec::new()),
        |(mut sequences, mut finals), frame| {
            sequences.insert(frame.sequence);
            if frame.is_final {
                finals.push(frame.sequence);
            }
            (sequences, finals)
        },
    );
    finals
        .into_iter()
        .any(|last| (0..=last).all(|sequence| sequences.contains(&sequence)))
}

/// Every frame of the E2E stream `stream_id` that `recorder` read, raw and in arrival order:
/// duplicates, stray frames and the actual order are all preserved.
#[cfg(feature = "dummy")]
pub(super) fn received_e2e_stream_frames(
    recorder: &ApplicationRecorder,
    stream_id: e2e::E2eStreamId,
) -> Vec<E2eStreamFrame> {
    recorder
        .inbound
        .lock()
        .unwrap()
        .iter()
        .filter_map(|data| match data {
            ApplicationData::E2eStreamFrame(frame) if frame.stream_id == stream_id => {
                Some(frame.clone())
            }
            _ => None,
        })
        .collect()
}
