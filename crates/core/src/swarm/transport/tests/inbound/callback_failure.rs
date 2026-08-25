use std::future::pending;

use super::*;

#[derive(Default)]
struct TimeoutOnceValidateSwarmCallback {
    calls: AtomicUsize,
    dropped: AtomicBool,
}

#[async_trait]
impl SwarmCallback for TimeoutOnceValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let _drop_guard = PendingValidateDropGuard(&self.dropped);
            pending::<()>().await;
        }
        Ok(())
    }
}

#[derive(Default)]
struct TimeoutOnceInboundSwarmCallback {
    calls: AtomicUsize,
    dropped: AtomicBool,
}

#[async_trait]
impl SwarmCallback for TimeoutOnceInboundSwarmCallback {
    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let _drop_guard = PendingValidateDropGuard(&self.dropped);
            pending::<()>().await;
        }
        Ok(())
    }
}

#[derive(Default)]
struct PanickingValidateSwarmCallback {
    calls: AtomicUsize,
    started: Notify,
    release: Notify,
}

#[async_trait]
impl SwarmCallback for PanickingValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.release.notified().await;
            panic!("injected inbound actor panic");
        }
        Ok(())
    }
}

#[tokio::test]
async fn validation_deadline_drops_user_future_releases_capacity_and_unblocks_lane() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let key = SecretKey::random();
    let peer: Did = key.address().into();
    let session = SessionSk::new_with_seckey(&key)?;
    let app_callback = Arc::new(TimeoutOnceValidateSwarmCallback::default());
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let cid = peer.to_string();
    let first = local_wire(
        Message::custom(b"validation-timeout")?,
        &session,
        transport.dht.did,
    )?;

    let error = callback
        .on_admitted_message_for_test(&cid, &first)
        .await
        .expect_err("pending validation must time out");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InboundValidationTimeout { peer: Some(found), .. }) if *found == peer
    ));
    assert!(app_callback.dropped.load(Ordering::SeqCst));
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);

    let second = local_wire(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 3 }),
        &session,
        transport.dht.did,
    )?;
    callback
        .on_admitted_message_for_test(&cid, &second)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(app_callback.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn inbound_callback_deadline_drops_user_future_and_releases_capacity() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let key = SecretKey::random();
    let peer: Did = key.address().into();
    let session = SessionSk::new_with_seckey(&key)?;
    let app_callback = Arc::new(TimeoutOnceInboundSwarmCallback::default());
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let cid = peer.to_string();
    let first = local_wire(
        Message::custom(b"inbound-timeout")?,
        &session,
        transport.dht.did,
    )?;

    let error = callback
        .on_admitted_message_for_test(&cid, &first)
        .await
        .expect_err("pending inbound callback must time out");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InboundProcessingTimeout { peer: Some(found), .. }) if *found == peer
    ));
    assert!(app_callback.dropped.load(Ordering::SeqCst));
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}

#[tokio::test]
async fn actor_panic_drops_active_and_queued_capacity_and_closes_mailbox() -> Result<()> {
    const QUEUED: usize = 8;
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let key = SecretKey::random();
    let peer: Did = key.address().into();
    let session = SessionSk::new_with_seckey(&key)?;
    let app_callback = Arc::new(PanickingValidateSwarmCallback::default());
    let callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        app_callback.clone(),
    ));
    let cid = peer.to_string();
    let frame = local_wire(
        Message::custom(b"panic-release")?,
        &session,
        transport.dht.did,
    )?;
    let mut deliveries = Vec::new();

    deliveries.push(spawn_inbound_delivery(
        Arc::clone(&callback),
        cid.clone(),
        frame.clone(),
    ));
    app_callback.started.notified().await;
    for _ in 1..QUEUED {
        deliveries.push(spawn_inbound_delivery(
            Arc::clone(&callback),
            cid.clone(),
            frame.clone(),
        ));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while callback.inbound_admitted_count_for_test() != QUEUED {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("panic regression inputs were not admitted".into()))?;
    app_callback.release.notify_waiters();

    for delivery in deliveries {
        assert!(matches!(
            delivery
                .await
                .map_err(|_| Error::InvalidMessage("delivery task panicked".into()))?,
            Err(Error::InvalidMessage(_))
        ));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while callback.inbound_admitted_count_for_test() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("actor panic retained inbound capacity".into()))?;
    let error = callback
        .on_admitted_message_for_test(&cid, &frame)
        .await
        .expect_err("dead actor mailbox must reject subsequent input");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InboundMailboxClosed)
    ));
    Ok(())
}
