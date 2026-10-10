use std::future::pending;
use std::time::Duration;

use super::*;
use crate::message::CustomMessage;
use crate::message::FoundEntry;
use crate::message::MessageSigner;
use crate::swarm::callback::inbound_application_capacity_for_test;
use crate::swarm::callback::inbound_peer_capacity_for_test;
use crate::swarm::callback::InboundLane;
use crate::tests::TEST_NETWORK_ID;

mod test_admission_delegation;
mod test_callback_failure;
mod test_capacity_handoff;
mod test_delegation_reference;
mod test_origin_quota;
mod test_pre_admission;
mod test_reassembly;
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

    async fn wait_for_inbounds_at_least(&self, count: usize) {
        self.inbounds
            .await_until(|inbounds| inbounds >= count)
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
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
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
        transport.retire_active_connection_for_test(attempt, |_| Ok(())),
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
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
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
        noop_control_message(transport.dht.did),
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

/// The drain of pre-admission frames must not block the data-channel open, and an
/// arrival admitted after it is queued in order behind the drained frame: both reach
/// the application once the lane is released.
#[tokio::test]
async fn test_pre_admission_drain_does_not_block_open_and_orders_admitted_arrivals() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = Arc::new(
        InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
            .with_pending_connection_attempt(attempt),
    );
    let early = MessagePayload::new_send(
        Message::custom(b"early-drain-must-not-block-open")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let late = MessagePayload::new_send(
        Message::custom(b"late-admitted-arrival-queues-behind-drain")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();

    callback
        .on_admitted_message_for_test(&cid, &early)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(callback.pre_admission_held_count_for_test(), 1);
    let application_admission = callback.hold_application_admission_for_test()?;

    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connecting)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    let open_callback = Arc::clone(&callback);
    let open_cid = cid.clone();
    let open = tokio::spawn(async move {
        open_callback
            .on_data_channel_open(&open_cid)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    tokio::time::timeout(Duration::from_secs(1), open)
        .await
        .map_err(|_| Error::InvalidMessage("data-channel open waited on drain".to_string()))?
        .map_err(|_| Error::InvalidMessage("data-channel-open task panicked".to_string()))??;

    let late_callback = Arc::clone(&callback);
    let late_cid = cid.clone();
    let late_delivery = tokio::spawn(async move {
        late_callback
            .on_admitted_message_for_test(&late_cid, &late)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    // The late arrival's completion is the actor's validation, which the held
    // lane defers: nothing reaches the application until the lane is released.
    assert_eq!(app_callback.inbounds(), 0);

    drop(application_admission);
    tokio::time::timeout(Duration::from_secs(1), late_delivery)
        .await
        .map_err(|_| {
            Error::InvalidMessage("admitted arrival waited beyond the lane release".to_string())
        })?
        .map_err(|_| Error::InvalidMessage("admitted-arrival task panicked".to_string()))??;
    app_callback.wait_for_inbounds_at_least(2).await;
    assert_eq!(callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(app_callback.inbounds(), 2);

    transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn test_pre_admission_drain_returns_before_application_validation_completes() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(BlockingValidateSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = Arc::new(
        InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
            .with_pending_connection_attempt(attempt),
    );
    let message = MessagePayload::new_send(
        Message::custom(b"early-validation-must-not-block-admission")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();

    callback
        .on_admitted_message_for_test(&cid, &message)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(callback.pre_admission_held_count_for_test(), 1);
    assert_eq!(app_callback.validates(), 0);

    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connecting)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    let open_callback = Arc::clone(&callback);
    let open_cid = cid.clone();
    let open = tokio::spawn(async move {
        open_callback
            .on_data_channel_open(&open_cid)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    tokio::time::timeout(Duration::from_secs(1), open)
        .await
        .map_err(|_| Error::InvalidMessage("data-channel open waited on validation".to_string()))?
        .map_err(|_| Error::InvalidMessage("data-channel-open task panicked".to_string()))??;

    assert!(transport.has_active_connection(peer));
    assert!(transport.is_active_connection_attempt(attempt));
    tokio::time::timeout(
        Duration::from_secs(1),
        app_callback.wait_for_first_validate_started(),
    )
    .await
    .map_err(|_| {
        Error::InvalidMessage("held frame was not submitted to inbound actor".to_string())
    })?;
    assert_eq!(callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(app_callback.inbounds(), 0);
    app_callback.release_first_validate();
    app_callback.wait_for_inbounds_at_least(1).await;
    assert_eq!(app_callback.inbounds(), 1);

    transport.disconnect(peer).await?;
    Ok(())
}

/// With the application lane saturated (every permit admitted and the lane
/// held, so no frame is dispatched), work beyond the mailbox capacity is
/// rejected with the typed error while a control frame is still admitted and
/// delivered on its own lane; releasing the lane drains every held frame.
#[tokio::test]
async fn test_inbound_mailbox_reserves_control_capacity_under_application_saturation() -> Result<()>
{
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        app_callback.clone(),
    ));
    // Held for the whole saturation: the lane's front sequence never becomes
    // ready, so the actor dispatches no application frame and no permit is
    // released while the capacity laws are observed.
    let application_hold = callback.hold_application_admission_for_test()?;
    let peer_count = inbound_application_capacity_for_test() / inbound_peer_capacity_for_test();
    assert_eq!(
        peer_count * inbound_peer_capacity_for_test(),
        inbound_application_capacity_for_test()
    );
    let mut application_inputs = Vec::with_capacity(peer_count);
    for _ in 0..peer_count {
        let key = SecretKey::random();
        let peer: Did = key.address().into();
        let session = DelegateeKey::new_with_seckey(&key)?;
        let mut messages = Vec::with_capacity(inbound_peer_capacity_for_test());
        for _ in 0..inbound_peer_capacity_for_test() {
            messages.push(
                MessagePayload::new_send(
                    Message::custom(b"bounded-inbound-mailbox")?,
                    MessageSigner::new(&session, TEST_NETWORK_ID),
                    transport.dht.did,
                    transport.dht.did,
                )?
                .to_wire()?,
            );
        }
        application_inputs.push((peer.to_string(), messages));
    }
    let control_key = SecretKey::random();
    let control_peer: Did = control_key.address().into();
    let control_session = DelegateeKey::new_with_seckey(&control_key)?;
    let overflow_message = MessagePayload::new_send(
        Message::custom(b"global-application-overflow")?,
        MessageSigner::new(&control_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let control = MessagePayload::new_send(
        noop_control_message(transport.dht.did),
        MessageSigner::new(&control_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let control_cid = control_peer.to_string();
    let mut deliveries = Vec::new();

    for (cid, messages) in application_inputs {
        for message in messages {
            let callback = Arc::clone(&callback);
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

    // Work beyond the active and queued capacity is not refused: it waits, holding its
    // sender's credit, until the mailbox drains.
    let overflow_callback = Arc::clone(&callback);
    let overflow_cid = control_cid.clone();
    let overflow = tokio::spawn(async move {
        overflow_callback
            .on_admitted_message_for_test(&overflow_cid, &overflow_message)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    callback
        .await_inbound_waiting_for_test(|waiting| waiting == 1)
        .await;

    // The reserved control lane is independent of the held application lane, and of the
    // same peer's waiting application arrival: the control frame is admitted, validated, and
    // delivered while every application permit stays held and the overflow still waits.
    callback
        .on_admitted_message_for_test(&control_cid, &control)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(app_callback.validates(), 1);
    assert_eq!(
        callback.inbound_admitted_count_for_test(),
        inbound_application_capacity_for_test()
    );
    assert!(!overflow.is_finished());

    drop(application_hold);
    for delivery in deliveries {
        delivery
            .await
            .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))??;
    }
    overflow
        .await
        .map_err(|_| Error::InvalidMessage("inbound overflow task panicked".to_string()))??;
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
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
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

fn local_wire(message: Message, session: &DelegateeKey, local: Did) -> Result<bytes::Bytes> {
    MessagePayload::new_send(
        message,
        MessageSigner::new(session, TEST_NETWORK_ID),
        local,
        local,
    )?
    .to_wire()
}

fn noop_control_message(did: Did) -> Message {
    Message::FindSuccessorReport(crate::message::FindSuccessorReport {
        did,
        handler: crate::message::FindSuccessorReportHandler::None,
    })
}

fn failed_receive_count(measure: &RecordingMeasure, peer: Did) -> Result<usize> {
    Ok(measure
        .snapshot_counters()?
        .into_iter()
        .filter(|(did, counter)| *did == peer && *counter == MeasurementEvent::FailedToReceive)
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

mod measurement;
