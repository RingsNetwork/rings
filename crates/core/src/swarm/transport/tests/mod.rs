use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
#[cfg(feature = "dummy")]
use bytes::Bytes;
use rings_transport::core::callback::TransportCallback;
#[cfg(feature = "dummy")]
use serde::Serialize;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use tracing_test::traced_test;

use self::application_recorder::ApplicationRecorder;
#[cfg(feature = "dummy")]
use super::outbound::TransferDemand;
use super::pending::ConnectionLifecycleRegistry;
#[cfg(feature = "dummy")]
use super::pending::FingerUpdateDisposition;
use super::pending::LifecycleBounds;
#[cfg(feature = "dummy")]
use super::pending::RawConnectionOwner;
use super::pending::ReservationVerdict;
use super::*;
#[cfg(feature = "dummy")]
use crate::chunk::Chunk;
#[cfg(feature = "dummy")]
use crate::chunk::ChunkMeta;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::dht::delivery::NextHop;
use crate::dht::EntryStorage;
#[cfg(feature = "dummy")]
use crate::dht::FingerFixRequest;
use crate::dht::VirtualNodeConfig;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use crate::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use crate::ecc::SecretKey;
use crate::measure::ApplyOutcome;
use crate::measure::Authentication;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::measure::MeasureError;
use crate::measure::MeasurementBatch;
use crate::measure::PeerQuality;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::HopBudget;
use crate::message::Message;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::MessageCategory;
use crate::message::MessagePayload;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::MessageRelay;
#[cfg(feature = "dummy")]
use crate::message::MessageSigner;
use crate::message::PayloadSender;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::Transaction;
use crate::storage::MemStorage;
#[cfg(feature = "dummy")]
use crate::swarm::application::SwarmEvent;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::callback::max_on_message_recursion_depth_for_test;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::callback::reset_on_message_recursion_depth_for_test;
use crate::swarm::callback::SwarmTransportCallback;
use crate::swarm::SwarmBuilder;
#[cfg(feature = "dummy")]
use crate::tests::TEST_NETWORK_ID;
use crate::utils::GenerationWitness;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::utils::Witness;

mod application_recorder;
mod test_events;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_finger;
#[cfg(feature = "dummy")]
mod test_held_messages;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_inbound;
mod test_lifecycle;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_readiness;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_retention;
mod test_retirement;
mod test_retirement_events;

/// Latched test event: set by the system under test, awaited by event.
///
/// The `AtomicBool` + `Notify::notify_waiters` pairing it replaces could lose
/// the wake-up between the flag check and `notified()` registration; the
/// witness retains the value.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
type TestLatch = Witness<bool>;

#[derive(Default)]
struct RecordingMeasure {
    counters: Mutex<Vec<(Did, MeasurementEvent)>>,
    measurements: Mutex<Vec<(Did, MeasurementEvent)>>,
    qualities: Mutex<BTreeMap<Did, PeerQuality>>,
    /// Bumped after each counter or measurement lands, so a test awaits the
    /// recording rather than polling under a wall-clock bound.
    recorded: GenerationWitness,
}

impl RecordingMeasure {
    /// Resolve once `predicate` holds over the fully applied measure state.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    async fn await_recorded(&self, predicate: impl Fn(&Self) -> bool) {
        self.recorded
            .await_until(|_generation| predicate(self))
            .await;
    }

    fn snapshot_counters(&self) -> std::io::Result<Vec<(Did, MeasurementEvent)>> {
        self.counters
            .lock()
            .map(|counters| counters.clone())
            .map_err(|_| std::io::Error::other("counters poisoned"))
    }

    #[cfg(feature = "dummy")]
    fn snapshot_measurements(&self) -> std::io::Result<Vec<(Did, MeasurementEvent)>> {
        self.measurements
            .lock()
            .map(|measurements| measurements.clone())
            .map_err(|_| std::io::Error::other("measurements poisoned"))
    }

    fn set_quality(&self, did: Did, quality: PeerQuality) -> std::io::Result<()> {
        self.qualities
            .lock()
            .map(|mut qualities| {
                qualities.insert(did, quality);
            })
            .map_err(|_| std::io::Error::other("qualities poisoned"))
    }
}

#[async_trait]
impl Measure for RecordingMeasure {
    async fn record(
        &self,
        did: Did,
        authentication: Authentication,
        event: MeasurementEvent,
    ) -> std::result::Result<ApplyOutcome, MeasureError> {
        if !authentication.permits(event) {
            return Ok(ApplyOutcome::IgnoredUnattributable);
        }
        match self.measurements.lock() {
            Ok(mut measurements) => measurements.push((did, event)),
            Err(_) => tracing::error!("RecordingMeasure measurements mutex is poisoned"),
        }
        self.observe_event(did, event).await;
        Ok(ApplyOutcome::Applied)
    }

    async fn record_batch(
        &self,
        did: Did,
        authentication: Authentication,
        batch: MeasurementBatch,
    ) -> std::result::Result<ApplyOutcome, MeasureError> {
        if !authentication.permits(batch.event()) {
            return Ok(ApplyOutcome::IgnoredUnattributable);
        }
        match self.measurements.lock() {
            Ok(mut measurements) => measurements.push((did, batch.event())),
            Err(_) => tracing::error!("RecordingMeasure measurements mutex is poisoned"),
        }
        // Publish every occurrence together; the event retains aggregate useful bytes.
        if let Ok(mut observations) = self.counters.lock() {
            observations
                .extend((0..batch.occurrences().get()).map(|_occurrence| (did, batch.event())));
        }
        self.recorded.bump();
        Ok(ApplyOutcome::Applied)
    }
}
impl RecordingMeasure {
    /// Test-only event observation, independent of the runtime Measure API.
    async fn observe_event(&self, did: Did, counter: MeasurementEvent) {
        match self.counters.lock() {
            Ok(mut counters) => counters.push((did, counter)),
            Err(_) => tracing::error!("RecordingMeasure counters mutex is poisoned"),
        }
        self.recorded.bump();
    }
}

#[async_trait]
impl BehaviourJudgement for RecordingMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        match self.qualities.lock() {
            Ok(qualities) => qualities.get(&did).copied().unwrap_or(PeerQuality::Unknown),
            Err(_) => {
                tracing::error!("RecordingMeasure qualities mutex is poisoned");
                PeerQuality::Unknown
            }
        }
    }
}

/// The receive failures `measure` counted against `peer`.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn failed_receive_count(measure: &RecordingMeasure, peer: Did) -> Result<usize> {
    Ok(measure
        .snapshot_counters()?
        .into_iter()
        .filter(|(did, counter)| *did == peer && *counter == MeasurementEvent::FailedToReceive)
        .count())
}

/// The successful receives `measure` applied for `peer`.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn successful_receive_count(measure: &RecordingMeasure, peer: Did) -> Result<usize> {
    Ok(measure
        .snapshot_measurements()?
        .into_iter()
        .filter(|(did, event)| *did == peer && matches!(event, MeasurementEvent::Received { .. }))
        .count())
}

/// A [`RecordingMeasure`] that pauses at the first observation of one lifecycle event, so a
/// test can act while the transition reporting it is in flight: `started` is set as the event
/// arrives, and the observation proceeds once the test sets `release`.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
struct BlockingEventMeasure {
    inner: RecordingMeasure,
    /// The lifecycle event whose observation is paused.
    gated: MeasurementEvent,
    started: TestLatch,
    release: TestLatch,
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl BlockingEventMeasure {
    /// A measure that pauses each observation of `gated` until released.
    fn gating(gated: MeasurementEvent) -> Self {
        Self {
            inner: RecordingMeasure::default(),
            gated,
            started: TestLatch::default(),
            release: TestLatch::default(),
        }
    }

    /// Resolve once an observation of the gated event has arrived and is paused.
    async fn wait_for_started(&self) {
        self.started.wait().await;
    }

    /// Let every paused observation of the gated event proceed.
    fn release(&self) {
        self.release.set();
    }

    /// Pause the gated lifecycle event before publishing any observation.
    async fn wait_for_event(&self, event: MeasurementEvent) {
        if event == self.gated {
            self.started.set();
            self.release.wait().await;
        }
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[async_trait]
impl Measure for BlockingEventMeasure {
    /// Apply one explicitly attributed observation in this test double.
    async fn record(
        &self,
        did: Did,
        authentication: Authentication,
        event: MeasurementEvent,
    ) -> std::result::Result<ApplyOutcome, MeasureError> {
        if !authentication.permits(event) {
            return Ok(ApplyOutcome::IgnoredUnattributable);
        }
        self.wait_for_event(event).await;
        self.inner.record(did, authentication, event).await
    }

    /// Gate once, then delegate the complete batch without splitting its transition.
    async fn record_batch(
        &self,
        did: Did,
        authentication: Authentication,
        batch: MeasurementBatch,
    ) -> std::result::Result<ApplyOutcome, MeasureError> {
        if !authentication.permits(batch.event()) {
            return Ok(ApplyOutcome::IgnoredUnattributable);
        }
        self.wait_for_event(batch.event()).await;
        self.inner.record_batch(did, authentication, batch).await
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[async_trait]
impl BehaviourJudgement for BlockingEventMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        self.inner.quality(did).await
    }
}

/// What a test transport varies, each part defaulting to the common case: a random identity,
/// a fresh [`RecordingMeasure`], production reassembly limits, and in-memory DHT storage.
struct TransportSpec {
    /// The local identity.
    key: SecretKey,
    /// The measure the transport reports to.
    measure: MeasureImpl,
    /// The bounds of inbound chunk reassembly.
    reassembly_limits: ReassemblyLimits,
    /// The DHT's entry storage.
    storage: EntryStorage,
}

impl Default for TransportSpec {
    fn default() -> Self {
        Self {
            key: SecretKey::random(),
            measure: Arc::new(RecordingMeasure::default()),
            reassembly_limits: ReassemblyLimits::production(),
            storage: Box::new(MemStorage::new()),
        }
    }
}

impl TransportSpec {
    /// The transport this spec describes.
    fn build(self) -> Result<SwarmTransport> {
        let delegatee_key = DelegateeKey::new_with_seckey(&self.key)?;
        let dht = Arc::new(PeerRing::new_with_storage_and_finger_table_size(
            delegatee_key.delegator_did(),
            3,
            self.storage,
            DEFAULT_FINGER_TABLE_SIZE,
        ));
        Ok(SwarmTransport::new(SwarmTransportParts {
            network_id: 0,
            webrtc: SwarmWebrtcConfig::new("".to_string(), None, None),
            delegatee_key,
            dht,
            measure: Some(self.measure),
            transaction_replay: crate::message::TransactionReplay::new_shared(Box::new(
                MemStorage::new(),
            )),
            settings: SwarmTransportSettings::new(
                1,
                VirtualNodeConfig::disabled(),
                self.reassembly_limits,
            ),
            observer: Arc::new(crate::swarm::observer::NoopSwarmObserver),
        }))
    }
}

/// A transport reporting to `measure`, otherwise as [`TransportSpec::default`].
fn transport_with_measure(measure: MeasureImpl) -> Result<SwarmTransport> {
    TransportSpec {
        measure,
        ..TransportSpec::default()
    }
    .build()
}

/// A transport whose identity is derived from `key`, for tests that must
/// place the local peer at a known ring position.
pub(super) fn transport_with_key(key: &SecretKey) -> Result<SwarmTransport> {
    TransportSpec {
        key: key.clone(),
        ..TransportSpec::default()
    }
    .build()
}

/// A shared [`TransportSpec::default`] transport, for a test that reads none of its parts.
fn default_transport() -> Result<Arc<SwarmTransport>> {
    TransportSpec::default().build().map(Arc::new)
}

/// Prepare a live finger-fix request for `slot`, the exact token production finger lookup
/// would place in the DHT.
///
/// The finger state enters the same `AwaitingReport` state production uses, and the helper
/// fails explicitly if `slot` cannot begin a request, so no test manufactures an uncorrelated
/// token.
#[cfg(feature = "dummy")]
fn finger_request(transport: &SwarmTransport, slot: usize) -> Result<FingerFixRequest> {
    transport
        .dht
        .lock_finger()?
        .prepare_request_for_test(slot)
        .ok_or_else(|| Error::InvalidMessage("failed to prepare test finger request".to_owned()))
}

#[cfg(feature = "dummy")]
pub(super) async fn open_dummy_data_channel_before_ice_connected(
    transport: &SwarmTransport,
    peer: Did,
) -> Result<()> {
    let connection = transport
        .get_raw_connection(peer)
        .ok_or(Error::SwarmMissTransport(peer))?;

    connection
        .connection
        .webrtc_answer_offer("remote-dummy-connection".to_string())
        .await
        .map_err(Error::Transport)?;
    // The browser callback order under test: the channel opens while the peer connection still
    // reports `Connecting`. The dummy is causal on its own, so the order is set explicitly.
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    assert_eq!(
        connection.webrtc_connection_state(),
        WebrtcConnectionState::Connecting
    );
    assert!(connection.connection.data_channel_is_open()?);
    Ok(())
}

#[test]
fn test_swarm_builder_uses_chord_virtual_node_default() -> Result<()> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key)?;
    let swarm = SwarmBuilder::new(7, "", Box::new(MemStorage::new()), delegatee_key).build();

    assert_eq!(
        swarm.dht_virtual_nodes(),
        DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
    assert_eq!(
        swarm.dht().storage_virtual_positions(swarm.did())?.len(),
        usize::from(DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER)
    );

    Ok(())
}

#[test]
fn test_swarm_builder_normalizes_virtual_nodes_before_protocol_advertisement() -> Result<()> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key)?;
    let requested = MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER.saturating_add(1);
    let swarm = SwarmBuilder::new(7, "", Box::new(MemStorage::new()), delegatee_key)
        .dht_virtual_nodes(requested)
        .build();

    assert_eq!(
        swarm.dht_virtual_nodes(),
        MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
    assert_eq!(
        swarm.dht().storage_virtual_positions(swarm.did())?.len(),
        usize::from(MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER)
    );

    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_successor_failover_considers_active_peer_outside_topology_hints() -> Result<()> {
    let transport = default_transport()?;
    let removed = SecretKey::random().address().into();
    let replacement = SecretKey::random().address().into();

    for peer in [removed, replacement] {
        let callback = SwarmTransportCallback::new(Arc::clone(&transport));
        let (attempt, _offer) = transport
            .prepare_connection_offer_with_attempt(peer, callback)
            .await?;
        open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
        assert!(transport.activate_connection_for_test(attempt)?);
    }
    transport.dht.admit_connected(removed, None)?;
    assert_eq!(transport.dht.successors().list()?, vec![removed]);
    assert!(!transport.dht.lock_finger()?.contains(Some(replacement)));

    assert_eq!(
        transport.live_successor_fallback(removed)?,
        Some(replacement)
    );

    transport.disconnect(removed).await?;
    transport.disconnect(replacement).await?;
    Ok(())
}

/// A frame of `message` that `session` signed and sent directly to `local`, its destination and
/// next hop.
#[cfg(feature = "dummy")]
fn local_payload(
    message: impl Serialize,
    session: &DelegateeKey,
    local: Did,
) -> Result<MessagePayload> {
    MessagePayload::new_send(
        message,
        MessageSigner::new(session, TEST_NETWORK_ID),
        local,
        local,
    )
}

/// The wire bytes of [`local_payload`].
#[cfg(feature = "dummy")]
fn local_wire(message: Message, session: &DelegateeKey, local: Did) -> Result<Bytes> {
    local_payload(message, session, local)?.to_wire()
}

/// A frame of `message` for `local` whose transaction `origin` signed at `sequence`, and whose
/// last hop to `local` `carrier` signed.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn relayed_payload(
    message: Message,
    origin: &DelegateeKey,
    carrier: &DelegateeKey,
    local: Did,
    sequence: u64,
) -> Result<MessagePayload> {
    let transaction = Transaction::new(
        local,
        crate::utils::new_uuid(),
        sequence,
        None,
        message,
        MessageSigner::new(origin, TEST_NETWORK_ID),
    )?;
    MessagePayload::new(
        transaction,
        MessageSigner::new(carrier, TEST_NETWORK_ID),
        MessageRelay::new(NextHop::toward(local), local, HopBudget::MAX),
    )
}

/// A peer, a signer for its messages, and the callback its frames arrive on: bound to a
/// pending handshake with the peer, or, from [`unbound_peer`], to none.
#[cfg(feature = "dummy")]
struct PendingPeer {
    peer: Did,
    session: DelegateeKey,
    /// The handshake the callback is bound to; `None` for an unbound callback.
    attempt: Option<PendingConnectionAttempt>,
    callback: Arc<SwarmTransportCallback>,
}

#[cfg(feature = "dummy")]
async fn pending_peer(transport: &Arc<SwarmTransport>) -> Result<PendingPeer> {
    pending_peer_with_key(transport, SecretKey::random()).await
}

/// A pending handshake with the peer behind `peer_key`, on a fresh connection generation and a
/// fresh callback bound to it: called twice with one key, the second is the next generation
/// of the same peer.
#[cfg(feature = "dummy")]
async fn pending_peer_with_key(
    transport: &Arc<SwarmTransport>,
    peer_key: SecretKey,
) -> Result<PendingPeer> {
    pending_peer_with(transport, peer_key, None).await
}

/// [`pending_peer_with_key`], with the callback's inbound clock injected when `clock` is
/// given, so a test drives the session hold's and reassembly's time itself.
#[cfg(feature = "dummy")]
async fn pending_peer_with(
    transport: &Arc<SwarmTransport>,
    peer_key: SecretKey,
    clock: Option<Arc<Mutex<u128>>>,
) -> Result<PendingPeer> {
    let peer: Did = peer_key.address().into();
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let offer_callback = SwarmTransportCallback::new(Arc::clone(transport));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = match clock {
        Some(clock) => {
            SwarmTransportCallback::new_with_reassembly_clock_for_test(Arc::clone(transport), clock)
        }
        None => SwarmTransportCallback::new(Arc::clone(transport)),
    }
    .with_pending_connection_attempt(attempt);
    Ok(PendingPeer {
        peer,
        session,
        attempt: Some(attempt),
        callback: Arc::new(callback),
    })
}

/// A peer behind `peer_key` whose frames arrive on a callback bound to no handshake: on no
/// link, and admitted by no connection.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn unbound_peer(transport: &Arc<SwarmTransport>, peer_key: &SecretKey) -> Result<PendingPeer> {
    Ok(PendingPeer {
        peer: peer_key.address().into(),
        session: DelegateeKey::new_with_seckey(peer_key)?,
        attempt: None,
        callback: Arc::new(SwarmTransportCallback::new(Arc::clone(transport))),
    })
}

#[cfg(feature = "dummy")]
impl PendingPeer {
    /// The bound handshake, which an unbound peer lacks.
    fn attempt(&self) -> Result<PendingConnectionAttempt> {
        self.attempt.ok_or(Error::SwarmMissTransport(self.peer))
    }

    /// A custom message of `data` from this peer to `transport`'s node.
    fn custom_payload(&self, transport: &SwarmTransport, data: &[u8]) -> Result<MessagePayload> {
        local_payload(Message::custom(data)?, &self.session, transport.dht.did)
    }

    /// The wire bytes of [`Self::custom_payload`].
    fn custom_message_wire(&self, transport: &SwarmTransport, data: &[u8]) -> Result<Bytes> {
        self.custom_payload(transport, data)?.to_wire()
    }

    /// A custom message of `data` from `origin`, carried its last hop to `transport`'s node by
    /// this peer: a session this connection learns only if told.
    #[cfg(not(target_family = "wasm"))]
    fn relayed_custom_payload(
        &self,
        transport: &SwarmTransport,
        origin: &DelegateeKey,
        data: &[u8],
    ) -> Result<MessagePayload> {
        relayed_payload(
            Message::custom(data)?,
            origin,
            &self.session,
            transport.dht.did,
            0,
        )
    }

    /// Deliver `bytes` on this peer's connection.
    async fn receive(&self, bytes: &[u8]) -> Result<()> {
        self.callback
            .on_admitted_message_for_test(&self.peer.to_string(), bytes)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    }

    /// Admit the handshake through the production path: the data channel opens before ICE
    /// connects.
    async fn admit(&self, transport: &SwarmTransport) -> Result<()> {
        open_dummy_data_channel_before_ice_connected(transport, self.peer).await?;
        self.callback
            .on_data_channel_open(&self.peer.to_string())
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    }

    /// Activate the handshake's generation directly, without opening its data channel.
    fn activate(&self, transport: &SwarmTransport) -> Result<()> {
        assert!(transport.activate_connection_for_test(self.attempt()?)?);
        Ok(())
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_nested_reassembled_chunk_is_rejected_without_recursive_callback_entry() -> Result<()>
{
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let recorder = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.activate(&transport)?;
    let peer = pending.peer;
    let mut current = pending.custom_message_wire(&transport, b"mailbox-drained")?;
    for _ in 0..2 {
        let chunk = Chunk {
            chunk: [0, 1],
            data: current,
            meta: ChunkMeta::default(),
        };
        current = local_wire(Message::Chunk(chunk), &pending.session, transport.dht.did)?;
    }

    reset_on_message_recursion_depth_for_test();
    let error = pending
        .callback
        .on_admitted_message_for_test(&peer.to_string(), &current)
        .await
        .expect_err("nested chunks must be rejected");

    assert!(error.to_string().contains("Nested chunk"));
    assert_eq!(recorder.observed(), 1);
    assert_eq!(recorder.inbounds(), 0);
    assert_eq!(max_on_message_recursion_depth_for_test(), 1);
    assert_eq!(failed_receive_count(&measure, peer)?, 1);
    assert_eq!(
        pending
            .callback
            .inbound_for_test()
            .admitted_count_for_test(),
        0
    );
    assert_eq!(transport.reassembly_budget().buffered_cost_for_test(), 0);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_missing_peer_error_precedes_outbound_capacity_admission() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = Did::from(700_u32);
    let mut permits = Vec::with_capacity(OUTBOUND_DATA_TRANSFER_CAPACITY);
    for _ in 0..OUTBOUND_DATA_TRANSFER_CAPACITY {
        permits.push(
            transport
                .outbound_schedulers
                .reserve(peer, TransferDemand::new(MessageCategory::Application, 1))
                .await?,
        );
    }
    let payload = MessagePayload::new_send(
        Message::custom(b"missing-peer")?,
        transport.message_signer(),
        peer,
        peer,
    )?;

    let error = transport
        .send_payload_admitted(payload)
        .await
        .expect_err("missing route must be checked before capacity");

    assert!(matches!(error, Error::SwarmMissDidInTable(did) if did == peer));
    assert_eq!(
        measure.snapshot_measurements()?,
        vec![(peer, MeasurementEvent::FailedToSend)],
        "a failed local send to an explicit DID is attributable without claiming remote evidence"
    );
    drop(permits);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[traced_test]
#[tokio::test]
async fn test_invalid_inbound_log_omits_transaction_data() -> Result<()> {
    const PRIVATE_MARKER: &str = "PRIVATE-INBOUND-PAYLOAD-7f34c91a";
    let transport = default_transport()?;
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let callback = SwarmTransportCallback::new(Arc::clone(&transport));
    let mut payload = MessagePayload::new_send(
        Message::custom(PRIVATE_MARKER.as_bytes())?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?;
    payload.transaction.data.push(171);
    let expected_tx_id = payload.transaction.tx_id;
    let expected_destination = payload.transaction.destination;
    let expected_data_bytes = payload.transaction.data.len();
    let bytes = payload.to_wire()?;
    let expected_wire_bytes = bytes.len();

    callback
        .on_admitted_message_for_test(&peer.to_string(), &bytes)
        .await
        .expect_err("mutating signed transaction data must fail verification");

    let expected_fields = [
        ("peer", format!("Some({peer:?})")),
        ("destination", expected_destination.to_string()),
        ("message_kind", "\"CustomMessage\"".to_owned()),
        ("data_bytes", expected_data_bytes.to_string()),
        ("wire_bytes", expected_wire_bytes.to_string()),
    ];
    let expected_tx_id = expected_tx_id.to_string();
    let marker_debug = crate::tests::byte_debug_fragment(PRIVATE_MARKER.as_bytes());
    logs_assert(|lines: &[&str]| {
        crate::tests::assert_single_structured_log_event(
            lines,
            "rings_core::swarm::callback",
            "inbound message verification failed or expired",
            ("tx_id", &expected_tx_id),
            &expected_fields,
            &[
                PRIVATE_MARKER,
                &marker_debug,
                "transaction.data",
                "MessagePayload {",
            ],
        )
    });
    Ok(())
}

/// A `Disconnected` state before the data channel opens neither retires the handshake nor
/// reaches the application; the later admission is announced as `PeerAdmitted` alone.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_pending_disconnected_before_data_channel_open_is_not_reported() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let recorder = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    let peer = pending.peer;

    pending
        .callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Disconnected)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 1);
    assert_eq!(recorder.events(), Vec::new());
    assert_eq!(measure.snapshot_counters()?, Vec::new());
    assert!(!transport.dht.successors().contains(&peer)?);

    pending.admit(&transport).await?;

    assert!(transport.has_active_connection(peer));
    assert!(transport.get_connection(peer).is_some());
    assert_eq!(recorder.events(), vec![SwarmEvent::PeerAdmitted { peer }]);
    assert!(
        transport.dht.successors().contains(&peer)?,
        "opened data channel must promote the advertised peer into DHT successors"
    );

    transport.disconnect(peer).await?;
    Ok(())
}

/// A terminal state that retires the generation while its admission is in flight keeps the
/// peer out of the DHT, and the application reads neither the admission nor a retirement.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_terminal_event_during_pending_admission_prevents_late_dht_join() -> Result<()> {
    let measure = Arc::new(BlockingEventMeasure::gating(MeasurementEvent::Connected));
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let recorder = ApplicationRecorder::attach(&transport)?;
    let peer = SecretKey::random().address().into();
    let offer_callback = SwarmTransportCallback::new(Arc::clone(&transport));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    let opening_transport = Arc::clone(&transport);
    let opening = tokio::spawn(async move {
        let callback =
            SwarmTransportCallback::new(opening_transport).with_pending_connection_attempt(attempt);
        callback
            .on_data_channel_open(&peer.to_string())
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    measure.wait_for_started().await;
    assert!(transport.is_active_connection_attempt(attempt));

    let terminal_callback = SwarmTransportCallback::new(Arc::clone(&transport))
        .with_pending_connection_attempt(attempt);
    terminal_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    measure.release();
    opening
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;

    assert!(!transport.is_active_connection_attempt(attempt));
    assert!(!transport.dht.successors().contains(&peer)?);
    assert_eq!(recorder.events(), Vec::new());
    Ok(())
}
