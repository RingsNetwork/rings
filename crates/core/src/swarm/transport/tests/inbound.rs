use std::future::pending;

use super::*;
use crate::chunk::ChunkList;
use crate::message::CustomMessage;
use crate::swarm::callback::inbound_application_capacity_for_test;
use crate::swarm::callback::inbound_mailbox_capacity_for_test;
use crate::swarm::callback::inbound_peer_capacity_for_test;

#[derive(Default)]
struct BlockingValidateSwarmCallback {
    validates: AtomicUsize,
    inbounds: AtomicUsize,
    validate_started: AtomicBool,
    validate_started_notify: Notify,
    release_validate: Notify,
}

impl BlockingValidateSwarmCallback {
    async fn wait_for_first_validate_started(&self) {
        while !self.validate_started.load(Ordering::SeqCst) {
            self.validate_started_notify.notified().await;
        }
    }

    fn release_first_validate(&self) {
        self.release_validate.notify_waiters();
    }

    fn validates(&self) -> usize {
        self.validates.load(Ordering::SeqCst)
    }

    fn inbounds(&self) -> usize {
        self.inbounds.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SwarmCallback for BlockingValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        let previous = self.validates.fetch_add(1, Ordering::SeqCst);
        if previous == 0 {
            self.validate_started.store(true, Ordering::SeqCst);
            self.validate_started_notify.notify_waiters();
            self.release_validate.notified().await;
        }
        Ok(())
    }

    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.inbounds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct PendingValidateSwarmCallback {
    started: AtomicBool,
    started_notify: Notify,
    dropped: AtomicBool,
}

struct PendingValidateDropGuard<'a>(&'a AtomicBool);

impl Drop for PendingValidateDropGuard<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl PendingValidateSwarmCallback {
    async fn wait_for_started(&self) {
        while !self.started.load(Ordering::SeqCst) {
            self.started_notify.notified().await;
        }
    }
}

#[async_trait]
impl SwarmCallback for PendingValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        let _drop_guard = PendingValidateDropGuard(&self.dropped);
        self.started.store(true, Ordering::SeqCst);
        self.started_notify.notify_waiters();
        pending::<()>().await;
        Ok(())
    }
}

#[derive(Default)]
struct OrderedReassemblyCallback {
    final_chunk_started: AtomicBool,
    final_chunk_started_notify: Notify,
    release_final_chunk: Notify,
    delivered: Mutex<Vec<Vec<u8>>>,
}

impl OrderedReassemblyCallback {
    async fn wait_for_final_chunk(&self) {
        while !self.final_chunk_started.load(Ordering::SeqCst) {
            self.final_chunk_started_notify.notified().await;
        }
    }

    fn release_final_chunk(&self) {
        self.release_final_chunk.notify_waiters();
    }

    fn delivered(&self) -> std::io::Result<Vec<Vec<u8>>> {
        self.delivered
            .lock()
            .map(|delivered| delivered.clone())
            .map_err(|_| std::io::Error::other("delivered messages poisoned"))
    }
}

#[async_trait]
impl SwarmCallback for OrderedReassemblyCallback {
    async fn on_validate(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if let Message::Chunk(chunk) = payload.transaction.data::<Message>()? {
            if chunk.chunk[0].saturating_add(1) == chunk.chunk[1] {
                self.final_chunk_started.store(true, Ordering::SeqCst);
                self.final_chunk_started_notify.notify_waiters();
                self.release_final_chunk.notified().await;
            }
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
async fn pending_message_rechecks_admission_after_async_validation() -> Result<()> {
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
        &peer_session,
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let pending_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    let cid = peer.to_string();
    let delivery = tokio::spawn(async move {
        pending_callback
            .on_message(&cid, &message)
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
async fn inbound_control_lane_progresses_while_application_validation_is_blocked() -> Result<()> {
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
        &peer_session,
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let control = MessagePayload::new_send(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 1 }),
        &peer_session,
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();

    let blocked_callback = Arc::clone(&callback);
    let blocked_cid = cid.clone();
    let blocked = tokio::spawn(async move {
        blocked_callback
            .on_message(&blocked_cid, &application)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    app_callback.wait_for_first_validate_started().await;

    let control_callback = Arc::clone(&callback);
    let control_task = tokio::spawn(async move {
        control_callback
            .on_message(&cid, &control)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while app_callback.validates() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("control lane was starved".to_string()))?;

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
async fn inbound_mailbox_reserves_control_capacity_under_application_saturation() -> Result<()> {
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
            &session,
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
        &control_session,
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let control = MessagePayload::new_send(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 2 }),
        &control_session,
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
                    .on_message(&cid, &message)
                    .await
                    .map_err(|error| Error::InvalidMessage(error.to_string()))
            }));
        }
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while callback.inbound_admitted_count_for_test() < inbound_application_capacity_for_test() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("inbound mailbox did not fill".to_string()))?;

    let overflow = callback
        .on_message(&control_cid, &overflow_message)
        .await
        .expect_err("work beyond active and queued capacity must be rejected");
    assert!(matches!(
        overflow.downcast_ref::<Error>(),
        Some(Error::InboundMailboxCapacityExceeded { capacity })
            if *capacity == inbound_mailbox_capacity_for_test()
    ));

    tokio::time::timeout(
        Duration::from_secs(1),
        callback.on_message(&control_cid, &control),
    )
    .await
    .map_err(|_| Error::InvalidMessage("reserved control capacity was starved".to_string()))?
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
async fn closing_inbound_mailbox_cancels_pending_callback() -> Result<()> {
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
        &peer_session,
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();
    let task_callback = Arc::clone(&callback);
    let delivery = tokio::spawn(async move {
        match task_callback.on_message(&cid, &message).await {
            Err(error) => matches!(
                error.downcast_ref::<Error>(),
                Some(Error::InboundMailboxClosed)
            ),
            Ok(()) => false,
        }
    });

    app_callback.wait_for_started().await;
    callback.close_inbound_for_test();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !app_callback.dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("pending callback was not cancelled".to_string()))?;
    assert!(delivery
        .await
        .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))?);
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}

fn local_wire(message: Message, session: &SessionSk, local: Did) -> Result<bytes::Bytes> {
    MessagePayload::new_send(message, session, local, local)?.to_wire()
}

#[tokio::test]
async fn reassembly_handoff_preserves_data_order_without_blocking_control() -> Result<()> {
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
            .on_message(&cid, &frame)
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
    let final_callback = Arc::clone(&callback);
    let final_cid = cid.clone();
    let final_delivery = tokio::spawn(async move {
        final_callback
            .on_message(&final_cid, &final_frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    app_callback.wait_for_final_chunk().await;

    let later = local_wire(
        Message::custom(b"later-application")?,
        &peer_session,
        transport.dht.did,
    )?;
    let later_callback = Arc::clone(&callback);
    let later_cid = cid.clone();
    let later_delivery = tokio::spawn(async move {
        later_callback
            .on_message(&later_cid, &later)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    let control = local_wire(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 1 }),
        &peer_session,
        transport.dht.did,
    )?;
    let control_callback = Arc::clone(&callback);
    let control_delivery = tokio::spawn(async move {
        control_callback
            .on_message(&cid, &control)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    tokio::time::timeout(Duration::from_secs(1), control_delivery)
        .await
        .map_err(|_| Error::InvalidMessage("control lane was blocked by reassembly".to_string()))?
        .map_err(|_| Error::InvalidMessage("control lane task panicked".to_string()))??;
    assert!(app_callback.delivered()?.is_empty());

    app_callback.release_final_chunk();
    final_delivery
        .await
        .map_err(|_| Error::InvalidMessage("final chunk task panicked".to_string()))??;
    later_delivery
        .await
        .map_err(|_| Error::InvalidMessage("later application task panicked".to_string()))??;
    assert_eq!(app_callback.delivered()?, vec![
        b"reassembled-first".to_vec(),
        b"later-application".to_vec(),
    ]);
    Ok(())
}

#[tokio::test]
async fn malformed_preclassification_records_peer_receive_failure() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer: Did = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(transport, Arc::new(NoopSwarmCallback));

    callback
        .on_message(&peer.to_string(), &[0xff])
        .await
        .expect_err("truncated wire metadata must fail");

    assert_eq!(
        measure
            .snapshot_counters()?
            .into_iter()
            .filter(|(did, counter)| {
                *did == peer && *counter == MeasureCounter::FailedToReceive
            })
            .count(),
        1
    );
    Ok(())
}

#[test]
fn inbound_mailbox_without_runtime_returns_typed_error() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let callback = InnerSwarmCallback::new(transport, Arc::new(NoopSwarmCallback));
    let is_runtime_error = futures::executor::block_on(async {
        match callback.on_message("not-a-did", b"not-a-message").await {
            Err(error) => matches!(
                error.downcast_ref::<Error>(),
                Some(Error::InboundMailboxRuntimeUnavailable)
            ),
            Ok(()) => false,
        }
    });

    assert!(is_runtime_error);
    Ok(())
}
