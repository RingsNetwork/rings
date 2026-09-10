use std::future::pending;

use super::*;
use crate::chunk::ChunkList;
use crate::message::CustomMessage;
use crate::message::FoundEntry;
use crate::message::MessageSigner;
use crate::swarm::callback::inbound_application_capacity_for_test;
use crate::swarm::callback::inbound_mailbox_capacity_for_test;
use crate::swarm::callback::inbound_peer_capacity_for_test;
use crate::swarm::callback::InboundLane;
use crate::tests::TEST_NETWORK_ID;

mod test_callback_failure;
mod test_capacity_handoff;
mod test_storage_interleave;

#[derive(Default)]
struct BlockingValidateSwarmCallback {
    validates: TestCounter,
    inbounds: TestCounter,
    validate_started: TestLatch,
    release_validate: TestLatch,
}

impl BlockingValidateSwarmCallback {
    async fn wait_for_first_validate_started(&self) {
        self.validate_started.wait().await;
    }

    async fn wait_for_validates_at_least(&self, count: usize) {
        self.validates
            .await_until(|validates| validates >= count)
            .await;
    }

    fn release_first_validate(&self) {
        self.release_validate.set();
    }

    fn validates(&self) -> usize {
        self.validates.get()
    }

    fn inbounds(&self) -> usize {
        self.inbounds.get()
    }
}

#[async_trait]
impl SwarmCallback for BlockingValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if self.validates.increment() == 0 {
            self.validate_started.set();
            self.release_validate.wait().await;
        }
        Ok(())
    }

    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.inbounds.increment();
        Ok(())
    }
}

#[derive(Default)]
struct PendingValidateSwarmCallback {
    started: TestLatch,
    dropped: TestLatch,
}

struct PendingValidateDropGuard<'a>(&'a TestLatch);

impl Drop for PendingValidateDropGuard<'_> {
    fn drop(&mut self) {
        self.0.set();
    }
}

impl PendingValidateSwarmCallback {
    async fn wait_for_started(&self) {
        self.started.wait().await;
    }
}

#[async_trait]
impl SwarmCallback for PendingValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        let _drop_guard = PendingValidateDropGuard(&self.dropped);
        self.started.set();
        pending::<()>().await;
        Ok(())
    }
}

#[derive(Default)]
struct OrderedReassemblyCallback {
    final_chunk_started: TestLatch,
    release_final_chunk: TestLatch,
    delivered: Mutex<Vec<Vec<u8>>>,
    validated: Mutex<Vec<&'static str>>,
}

impl OrderedReassemblyCallback {
    async fn wait_for_final_chunk(&self) {
        self.final_chunk_started.wait().await;
    }

    fn release_final_chunk(&self) {
        self.release_final_chunk.set();
    }

    fn delivered(&self) -> std::io::Result<Vec<Vec<u8>>> {
        self.delivered
            .lock()
            .map(|delivered| delivered.clone())
            .map_err(|_| std::io::Error::other("delivered messages poisoned"))
    }

    fn validated(&self) -> std::io::Result<Vec<&'static str>> {
        self.validated
            .lock()
            .map(|validated| validated.clone())
            .map_err(|_| std::io::Error::other("validated messages poisoned"))
    }
}

#[async_trait]
impl SwarmCallback for OrderedReassemblyCallback {
    async fn on_validate(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        match payload.transaction.data::<Message>()? {
            Message::Chunk(chunk) if chunk.chunk[0].saturating_add(1) == chunk.chunk[1] => {
                self.final_chunk_started.set();
                self.release_final_chunk.wait().await;
            }
            Message::CustomMessage(_) => self
                .validated
                .lock()
                .map_err(|_| std::io::Error::other("validated messages poisoned"))?
                .push("reassembled"),
            Message::FoundEntry(_) => self
                .validated
                .lock()
                .map_err(|_| std::io::Error::other("validated messages poisoned"))?
                .push("storage"),
            _ => {}
        }
        Ok(())
    }

    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if let Message::CustomMessage(CustomMessage(body)) =
            payload.transaction.data::<Message>()?
        {
            self.delivered
                .lock()
                .map_err(|_| std::io::Error::other("delivered messages poisoned"))?
                .push(body);
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_pending_message_rechecks_admission_after_async_validation() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(BlockingValidateSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    let message = MessagePayload::new_send(
        Message::custom(b"must-not-dispatch-after-retire")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let pending_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    let cid = peer.to_string();
    let delivery = tokio::spawn(async move {
        pending_callback
            .on_admitted_message_for_test(&cid, &message)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    app_callback.wait_for_first_validate_started().await;
    assert!(matches!(
        transport.retire_active_connection_with(attempt, |_| Ok(())),
        Ok(Some(()))
    ));
    app_callback.release_first_validate();
    delivery
        .await
        .map_err(|_| Error::InvalidMessage("mailbox task panicked".to_string()))??;

    assert_eq!(app_callback.validates(), 1);
    assert_eq!(app_callback.inbounds(), 0);
    Ok(())
}

#[tokio::test]
async fn test_inbound_control_lane_progresses_while_application_validation_is_blocked() -> Result<()>
{
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(BlockingValidateSwarmCallback::default());
    let callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        app_callback.clone(),
    ));
    let application = MessagePayload::new_send(
        Message::custom(b"blocked-application")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let control = MessagePayload::new_send(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 1 }),
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();

    let blocked_callback = Arc::clone(&callback);
    let blocked_cid = cid.clone();
    let blocked = tokio::spawn(async move {
        blocked_callback
            .on_admitted_message_for_test(&blocked_cid, &application)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    app_callback.wait_for_first_validate_started().await;

    let control_callback = Arc::clone(&callback);
    let control_task = tokio::spawn(async move {
        control_callback
            .on_admitted_message_for_test(&cid, &control)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    app_callback.wait_for_validates_at_least(2).await;

    app_callback.release_first_validate();
    blocked
        .await
        .map_err(|_| Error::InvalidMessage("application mailbox task panicked".to_string()))??;
    control_task
        .await
        .map_err(|_| Error::InvalidMessage("control mailbox task panicked".to_string()))??;
    assert_eq!(app_callback.inbounds(), 2);
    Ok(())
}

#[tokio::test]
async fn test_inbound_mailbox_reserves_control_capacity_under_application_saturation() -> Result<()>
{
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(BlockingValidateSwarmCallback::default());
    let callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        app_callback.clone(),
    ));
    let peer_count = inbound_application_capacity_for_test() / inbound_peer_capacity_for_test();
    assert_eq!(
        peer_count * inbound_peer_capacity_for_test(),
        inbound_application_capacity_for_test()
    );
    let mut application_inputs = Vec::with_capacity(peer_count);
    for _ in 0..peer_count {
        let key = SecretKey::random();
        let peer: Did = key.address().into();
        let session = SessionSk::new_with_seckey(&key)?;
        let message = MessagePayload::new_send(
            Message::custom(b"bounded-inbound-mailbox")?,
            MessageSigner::new(&session, TEST_NETWORK_ID),
            transport.dht.did,
            transport.dht.did,
        )?
        .to_wire()?;
        application_inputs.push((peer.to_string(), message));
    }
    let control_key = SecretKey::random();
    let control_peer: Did = control_key.address().into();
    let control_session = SessionSk::new_with_seckey(&control_key)?;
    let overflow_message = MessagePayload::new_send(
        Message::custom(b"global-application-overflow")?,
        MessageSigner::new(&control_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let control = MessagePayload::new_send(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 2 }),
        MessageSigner::new(&control_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let control_cid = control_peer.to_string();
    let mut deliveries = Vec::new();

    for (cid, message) in application_inputs {
        for _ in 0..inbound_peer_capacity_for_test() {
            let callback = Arc::clone(&callback);
            let message = message.clone();
            let cid = cid.clone();
            deliveries.push(tokio::spawn(async move {
                callback
                    .on_admitted_message_for_test(&cid, &message)
                    .await
                    .map_err(|error| Error::InvalidMessage(error.to_string()))
            }));
        }
    }
    callback
        .await_inbound_admitted_count_for_test(|admitted| {
            admitted >= inbound_application_capacity_for_test()
        })
        .await;

    let overflow = callback
        .on_admitted_message_for_test(&control_cid, &overflow_message)
        .await
        .expect_err("work beyond active and queued capacity must be rejected");
    assert!(matches!(
        overflow.downcast_ref::<Error>(),
        Some(Error::InboundMailboxCapacityExceeded { capacity })
            if *capacity == inbound_mailbox_capacity_for_test()
    ));

    // The reserved control lane either admits this frame or fails with a typed
    // capacity error; a starved lane would surface as a hang bounded by the
    // test harness, never as a sub-second race.
    callback
        .on_admitted_message_for_test(&control_cid, &control)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(app_callback.validates() >= 2);

    app_callback.release_first_validate();
    for delivery in deliveries {
        delivery
            .await
            .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))??;
    }
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}

#[tokio::test]
async fn test_closing_inbound_mailbox_cancels_pending_callback() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(PendingValidateSwarmCallback::default());
    let callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        app_callback.clone(),
    ));
    let message = MessagePayload::new_send(
        Message::custom(b"pending-inbound-callback")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();
    let task_callback = Arc::clone(&callback);
    let delivery = tokio::spawn(async move {
        match task_callback
            .on_admitted_message_for_test(&cid, &message)
            .await
        {
            Err(error) => matches!(
                error.downcast_ref::<Error>(),
                Some(Error::InboundMailboxClosed)
            ),
            Ok(()) => false,
        }
    });

    app_callback.wait_for_started().await;
    callback.close_inbound_for_test();
    app_callback.dropped.wait().await;
    assert!(delivery
        .await
        .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))?);
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}

fn local_wire(message: Message, session: &SessionSk, local: Did) -> Result<bytes::Bytes> {
    MessagePayload::new_send(
        message,
        MessageSigner::new(session, TEST_NETWORK_ID),
        local,
        local,
    )?
    .to_wire()
}

fn failed_receive_count(measure: &RecordingMeasure, peer: Did) -> Result<usize> {
    Ok(measure
        .snapshot_counters()?
        .into_iter()
        .filter(|(did, counter)| *did == peer && *counter == MeasureCounter::FailedToReceive)
        .count())
}

fn successful_receive_count(measure: &RecordingMeasure, peer: Did) -> Result<usize> {
    Ok(measure
        .snapshot_measurements()?
        .into_iter()
        .filter(|(did, event)| *did == peer && matches!(event, MeasurementEvent::Received { .. }))
        .count())
}

fn spawn_inbound_delivery(
    callback: Arc<InnerSwarmCallback>,
    cid: String,
    frame: bytes::Bytes,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        callback
            .on_admitted_message_for_test(&cid, &frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    })
}

fn hold_saturated_application_capacity(callback: &InnerSwarmCallback) -> Result<Vec<impl Drop>> {
    let lane_capacity = inbound_application_capacity_for_test();
    let peer_capacity = inbound_peer_capacity_for_test();
    let mut capacity_holds = Vec::with_capacity(lane_capacity);
    while capacity_holds.len() < lane_capacity {
        let peer: Did = SecretKey::random().address().into();
        for _ in 0..peer_capacity.min(lane_capacity - capacity_holds.len()) {
            capacity_holds.push(callback.hold_application_capacity_for_test(peer)?);
        }
    }
    assert_eq!(callback.inbound_admitted_count_for_test(), lane_capacity);
    Ok(capacity_holds)
}

#[tokio::test]
async fn test_chunk_reassembly_records_one_exact_logical_receive() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let logical_payload = MessagePayload::new_send(
        Message::custom(&vec![41; 512])?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?;
    let expected_useful_bytes = u64::try_from(logical_payload.transaction.data.len())
        .map_err(|_| Error::MessageSizeOverflow)?;
    let chunks: Vec<Chunk> = ChunkList::split(&logical_payload.to_wire()?, 32).into();
    assert!(chunks.len() > 1);

    for chunk in chunks {
        let frame = local_wire(Message::Chunk(chunk), &peer_session, transport.dht.did)?;
        callback
            .on_admitted_message_for_test(&peer.to_string(), &frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    }

    let measurements = measure.snapshot_measurements()?;
    let received = measurements
        .iter()
        .filter_map(|(observed_peer, event)| match event {
            MeasurementEvent::Received { useful_bytes } if *observed_peer == peer => {
                Some(*useful_bytes)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(received, vec![expected_useful_bytes]);
    assert_eq!(
        measurements
            .iter()
            .filter(|(observed_peer, event)| {
                *observed_peer == peer && matches!(event, MeasurementEvent::FailedToReceive)
            })
            .count(),
        0
    );
    assert_eq!(app_callback.inbounds(), 1);
    Ok(())
}

#[tokio::test]
async fn test_reassembled_undecodable_message_records_one_failure_only() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let undecodable = MessagePayload::new_send(
        vec![0xff_u8; 512],
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?;
    let chunks: Vec<Chunk> = ChunkList::split(&undecodable.to_wire()?, 32).into();
    let final_index = chunks.len().saturating_sub(1);
    assert!(final_index > 0);

    for (index, chunk) in chunks.into_iter().enumerate() {
        let frame = local_wire(Message::Chunk(chunk), &peer_session, transport.dht.did)?;
        let delivery = callback
            .on_admitted_message_for_test(&peer.to_string(), &frame)
            .await;
        if index == final_index {
            assert!(delivery.is_err());
        } else {
            delivery.map_err(|error| Error::InvalidMessage(error.to_string()))?;
        }
    }

    assert_eq!(failed_receive_count(&measure, peer)?, 1);
    assert_eq!(successful_receive_count(&measure, peer)?, 0);
    assert_eq!(app_callback.inbounds(), 0);
    Ok(())
}

#[tokio::test]
async fn test_reassembly_handoff_preserves_data_order_without_blocking_control() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(OrderedReassemblyCallback::default());
    let callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        app_callback.clone(),
    ));
    let first_wire = local_wire(
        Message::custom(b"reassembled-first")?,
        &peer_session,
        transport.dht.did,
    )?;
    let chunks: Vec<Chunk> = ChunkList::split(&first_wire, 32).into();
    assert!(chunks.len() > 1);
    let cid = peer.to_string();

    for chunk in &chunks[..chunks.len() - 1] {
        let frame = local_wire(
            Message::Chunk(chunk.clone()),
            &peer_session,
            transport.dht.did,
        )?;
        callback
            .on_admitted_message_for_test(&cid, &frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    }

    let final_chunk = chunks
        .last()
        .cloned()
        .ok_or(Error::InboundActorInvariantViolation)?;
    let final_frame = local_wire(
        Message::Chunk(final_chunk),
        &peer_session,
        transport.dht.did,
    )?;
    let final_delivery = spawn_inbound_delivery(Arc::clone(&callback), cid.clone(), final_frame);
    app_callback.wait_for_final_chunk().await;

    let later = local_wire(
        Message::FoundEntry(FoundEntry {
            data: Vec::new(),
            misses: Vec::new(),
            resource: Did::from(91_u32),
            redundancy: 1,
        }),
        &peer_session,
        transport.dht.did,
    )?;
    let later_delivery = spawn_inbound_delivery(Arc::clone(&callback), cid.clone(), later);

    let control = local_wire(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 1 }),
        &peer_session,
        transport.dht.did,
    )?;
    let control_delivery = spawn_inbound_delivery(Arc::clone(&callback), cid, control);
    control_delivery
        .await
        .map_err(|_| Error::InvalidMessage("control lane task panicked".to_string()))??;
    assert!(app_callback.delivered()?.is_empty());
    assert!(app_callback.validated()?.is_empty());

    app_callback.release_final_chunk();
    final_delivery
        .await
        .map_err(|_| Error::InvalidMessage("final chunk task panicked".to_string()))??;
    let later_result = later_delivery
        .await
        .map_err(|_| Error::InvalidMessage("later storage task panicked".to_string()))?;
    assert!(
        later_result.is_err(),
        "unsolicited storage response must fail"
    );
    assert_eq!(app_callback.delivered()?, vec![
        b"reassembled-first".to_vec()
    ]);
    assert_eq!(app_callback.validated()?, vec!["reassembled", "storage"]);
    Ok(())
}

#[tokio::test]
async fn test_transport_preparation_authenticates_every_reserved_lane() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let control = local_wire(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 1 }),
        &peer_session,
        transport.dht.did,
    )?;
    let application = local_wire(
        Message::custom(b"application")?,
        &peer_session,
        transport.dht.did,
    )?;
    let storage = local_wire(
        Message::FoundEntry(FoundEntry {
            data: Vec::new(),
            misses: Vec::new(),
            resource: Did::from(7_u32),
            redundancy: 1,
        }),
        &peer_session,
        transport.dht.did,
    )?;
    let chunks: Vec<Chunk> = ChunkList::split(&application, 32).into();
    let chunk = chunks
        .into_iter()
        .next()
        .ok_or(Error::InboundActorInvariantViolation)?;
    let reassembly = local_wire(Message::Chunk(chunk), &peer_session, transport.dht.did)?;

    let control_lane =
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, &control)?;
    assert_eq!(control_lane, InboundLane::DhtControl);
    let truncated = control
        .get(..control.len().saturating_sub(1))
        .ok_or(Error::InboundActorInvariantViolation)?;
    assert!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, truncated)
            .is_err(),
        "a truncated control-shaped payload must not claim control capacity"
    );
    for (name, frame) in [
        ("control", &control),
        ("application", &application),
        ("storage", &storage),
        ("reassembly", &reassembly),
    ] {
        let mut damaged = frame.to_vec();
        let final_byte = damaged
            .last_mut()
            .ok_or(Error::InboundActorInvariantViolation)?;
        *final_byte ^= 1;
        assert!(
            crate::swarm::callback::prepare_transport_frame_lane_for_test(
                TEST_NETWORK_ID,
                &damaged
            )
            .is_err(),
            "unauthenticated {name} shape must not claim reserved capacity"
        );
    }
    assert_eq!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(
            TEST_NETWORK_ID,
            &application
        )?,
        InboundLane::Application
    );
    assert_eq!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, &storage)?,
        InboundLane::Storage
    );
    assert_eq!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(
            TEST_NETWORK_ID,
            &reassembly
        )?,
        InboundLane::Reassembly
    );
    assert!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, &[0xff])
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn test_reassembled_control_shape_is_verified_before_lane_transition() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let saturated = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        Arc::new(NoopSwarmCallback),
    ));
    let capacity_holds = hold_saturated_application_capacity(&saturated)?;

    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = SessionSk::new_with_seckey(&peer_key)?;
    let mut tampered = MessagePayload::new_send(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 4 }),
        MessageSigner::new(&session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?;
    tampered.transaction.data.push(0);
    assert_eq!(
        crate::message::MessageKind::from_wire(&tampered.transaction.data)?.class(),
        crate::message::MessageClass::DhtControl
    );
    let tampered_wire = tampered.to_wire()?;
    let chunks: Vec<Chunk> = ChunkList::split(&tampered_wire, 32).into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    for chunk in &chunks[..chunks.len() - 1] {
        let frame = local_wire(Message::Chunk(chunk.clone()), &session, transport.dht.did)?;
        callback
            .on_admitted_message_for_test(&peer.to_string(), &frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    }
    let final_frame = local_wire(
        Message::Chunk(
            chunks
                .last()
                .cloned()
                .ok_or(Error::InboundActorInvariantViolation)?,
        ),
        &session,
        transport.dht.did,
    )?;
    let error = callback
        .on_admitted_message_for_test(&peer.to_string(), &final_frame)
        .await
        .expect_err("tampered reassembled control frame must fail verification");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InvalidMessage(message))
            if message == "message verification failed or message expired"
    ));

    drop(capacity_holds);
    Ok(())
}

mod measurement;
