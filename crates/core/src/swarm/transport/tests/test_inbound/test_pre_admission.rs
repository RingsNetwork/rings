use super::*;

#[derive(Default)]
struct FailingConnectedSwarmCallback {
    inbounds: TestCounter,
}

impl FailingConnectedSwarmCallback {
    async fn wait_for_inbounds_at_least(&self, count: usize) {
        self.inbounds
            .await_until(|inbounds| inbounds >= count)
            .await;
    }

    fn inbounds(&self) -> usize {
        self.inbounds.get()
    }
}

#[async_trait]
impl SwarmCallback for FailingConnectedSwarmCallback {
    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.inbounds.increment();
        Ok(())
    }

    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        let SwarmEvent::ConnectionStateChange { state, .. } = event else {
            return Ok(());
        };
        if *state == WebrtcConnectionState::Connected {
            return Err(std::io::Error::other("connected callback failure").into());
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_pre_admission_drain_runs_after_connected_event_error() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(FailingConnectedSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    let early = local_wire(
        Message::custom(b"held-frame-survives-connected-callback-error")?,
        &peer_session,
        transport.dht.did,
    )?;

    callback
        .on_admitted_message_for_test(&peer.to_string(), &early)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(callback.pre_admission_held_count_for_test(), 1);
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    match callback.on_data_channel_open(&peer.to_string()).await {
        Ok(()) => {
            return Err(Error::InvalidMessage(
                "connected callback error was swallowed".to_string(),
            ));
        }
        Err(error) => assert!(error.to_string().contains("connected callback failure")),
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app_callback.wait_for_inbounds_at_least(1),
    )
    .await
    .map_err(|_| Error::InvalidMessage("held frame did not drain after admission".to_string()))?;

    assert_eq!(callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(app_callback.inbounds(), 1);
    assert!(transport.is_active_connection_attempt(attempt));
    transport.disconnect(peer).await?;
    Ok(())
}

/// A frame admitted by core while its generation was live is a verified
/// receive; if the generation is retired before the actor dispatches it, the
/// application never sees it.
#[tokio::test]
async fn test_retired_frame_queued_on_lane_is_not_delivered() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = Arc::new(
        InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
            .with_pending_connection_attempt(attempt),
    );
    let admission_turn = callback.hold_application_admission_for_test()?;
    let frame = local_wire(
        Message::custom(b"retired-before-lane-ticket-release")?,
        &peer_session,
        transport.dht.did,
    )?;
    let delivery = spawn_inbound_delivery(Arc::clone(&callback), peer.to_string(), frame);

    // Core admission completes while the generation is live: the frame is
    // verified, counted as received, and queued behind the held lane. The
    // delivery itself completes only when the actor dispatches the frame.
    measure
        .await_recorded(|measure| {
            successful_receive_count(measure, peer).is_ok_and(|count| count == 1)
        })
        .await;
    assert_eq!(callback.inbound_admitted_count_for_test(), 1);
    assert_eq!(
        transport.retire_active_connection_for_test(attempt, |_| Ok(()))?,
        Some(())
    );
    drop(admission_turn);

    // The released lane dispatches the frame to a retired generation: the
    // actor drops it, completes the delivery, and releases the permit.
    delivery
        .await
        .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))??;
    callback
        .await_inbound_admitted_count_for_test(|admitted| admitted == 0)
        .await;
    assert_eq!(successful_receive_count(&measure, peer)?, 1);
    assert_eq!(app_callback.validates(), 0);
    assert_eq!(app_callback.inbounds(), 0);
    Ok(())
}
