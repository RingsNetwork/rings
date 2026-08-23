use super::*;

#[cfg(feature = "dummy")]
#[tokio::test]
async fn terminal_event_starts_in_order_without_waiting_for_connected_callback() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let app_callback = Arc::new(BlockingEventSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    let opening_transport = Arc::clone(&transport);
    let opening_callback = app_callback.clone();
    let opening = tokio::spawn(async move {
        let callback = InnerSwarmCallback::new(opening_transport, opening_callback)
            .with_pending_connection_attempt(attempt);
        callback
            .on_data_channel_open(&peer.to_string())
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    app_callback.wait_for_connected_event_started().await;
    let terminal_transport = Arc::clone(&transport);
    let terminal_callback = app_callback.clone();
    let terminal = tokio::spawn(async move {
        let callback = InnerSwarmCallback::new(terminal_transport, terminal_callback)
            .with_pending_connection_attempt(attempt);
        callback
            .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), terminal)
        .await
        .map_err(|_| {
            Error::InvalidMessage("terminal event was blocked by application callback".to_string())
        })?
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;
    assert_eq!(connected_and_closed_events(app_callback.events()?), vec![
        WebrtcConnectionState::Connected,
        WebrtcConnectionState::Closed
    ]);
    app_callback.release_connected_event();

    opening
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;
    assert_eq!(connected_and_closed_events(app_callback.events()?), vec![
        WebrtcConnectionState::Connected,
        WebrtcConnectionState::Closed
    ]);
    assert!(!transport.is_admitted_connection_attempt(attempt));
    assert!(!transport.dht.successors().contains(&peer)?);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn slow_connected_event_for_one_peer_does_not_block_other_peer_events() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let blocked_peer: Did = SecretKey::random().address().into();
    let other_peer: Did = SecretKey::random().address().into();
    let app_callback = Arc::new(BlockingEventSwarmCallback::blocking_peer(blocked_peer));
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(blocked_peer, offer_callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, blocked_peer).await?;

    let opening_transport = Arc::clone(&transport);
    let opening_callback = app_callback.clone();
    let opening = tokio::spawn(async move {
        let callback = InnerSwarmCallback::new(opening_transport, opening_callback)
            .with_pending_connection_attempt(attempt);
        callback
            .on_data_channel_open(&blocked_peer.to_string())
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    app_callback.wait_for_connected_event_started().await;
    let other_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        other_callback.on_peer_connection_state_change(
            &other_peer.to_string(),
            WebrtcConnectionState::Connecting,
        ),
    )
    .await
    .map_err(|_| Error::InvalidMessage("unrelated peer event was blocked".to_string()))?
    .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(app_callback
        .peer_events()?
        .contains(&(other_peer, WebrtcConnectionState::Connecting)));
    app_callback.release_connected_event();
    opening
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;

    assert!(app_callback
        .peer_events()?
        .contains(&(blocked_peer, WebrtcConnectionState::Connected)));
    transport.disconnect(blocked_peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
fn connected_and_closed_events(events: Vec<WebrtcConnectionState>) -> Vec<WebrtcConnectionState> {
    events
        .into_iter()
        .filter(|state| {
            matches!(
                state,
                WebrtcConnectionState::Connected | WebrtcConnectionState::Closed
            )
        })
        .collect()
}

#[tokio::test]
async fn local_did_lifecycle_callbacks_are_ignored() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let local_cid = transport.dht.did.to_string();

    callback
        .on_data_channel_open(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_peer_connection_state_change(&local_cid, WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_data_channel_close(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 0);
    assert!(transport.get_connection(transport.dht.did).is_none());
    Ok(())
}

#[tokio::test]
async fn mismatched_pending_callback_cancels_attempt_without_admission() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    let mismatched_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
            .with_pending_connection_attempt(attempt);
    let local_cid = transport.dht.did.to_string();

    mismatched_callback
        .on_data_channel_open(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 0);
    assert!(transport.get_connection(peer).is_none());
    assert!(transport.get_connection(transport.dht.did).is_none());
    Ok(())
}

#[tokio::test]
async fn late_terminal_callback_cannot_remove_replacement_active_slot() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.retire_pending_connection(old_attempt)?);
    let current_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(current_attempt)?);
    let late_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
            .with_pending_connection_attempt(old_attempt);

    late_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_admitted_connection_attempt(current_attempt));
    assert_eq!(transport.admitted_connection_ids(), vec![peer]);
    Ok(())
}

#[test]
fn connection_offer_protocol_mode_includes_storage_redundancy() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let matching = ConnectNodeSend {
        sdp: String::new(),
        network_id: 0,
        storage_redundancy: 1,
        dht_virtual_nodes: 0,
    };
    let mismatched_redundancy = ConnectNodeSend {
        storage_redundancy: 2,
        ..matching.clone()
    };

    assert!(transport.accepts_connection_offer(&matching));
    assert!(!transport.accepts_connection_offer(&mismatched_redundancy));
    Ok(())
}

#[test]
fn connection_answer_protocol_mode_includes_storage_redundancy() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let matching = ConnectNodeReport {
        sdp: String::new(),
        network_id: 0,
        storage_redundancy: 1,
        dht_virtual_nodes: 0,
    };
    let mismatched_redundancy = ConnectNodeReport {
        storage_redundancy: 2,
        ..matching.clone()
    };

    assert!(transport.accepts_connection_answer(&matching));
    assert!(!transport.accepts_connection_answer(&mismatched_redundancy));
    Ok(())
}

#[tokio::test]
async fn disconnected_observation_is_once_per_connection_epoch() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = transport_with_measure(measure.clone())?;
    let peer: Did = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    transport.record_peer_disconnected(attempt).await;
    transport.record_peer_disconnected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_some());
    transport.record_peer_connected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_none());
    transport.record_peer_disconnected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_some());

    assert_eq!(measure.snapshot_counters()?.as_slice(), &[
        (peer, MeasureCounter::Disconnected),
        (peer, MeasureCounter::Connect),
        (peer, MeasureCounter::Disconnected),
    ]);

    Ok(())
}

#[tokio::test]
async fn dht_candidate_order_uses_peer_quality_without_dropping_candidates() -> Result<()> {
    let degraded = SecretKey::random().address().into();
    let unknown = SecretKey::random().address().into();
    let healthy = SecretKey::random().address().into();
    let measure = Arc::new(RecordingMeasure::default());
    measure.set_quality(degraded, PeerQuality::Degraded)?;
    measure.set_quality(healthy, PeerQuality::Healthy)?;
    let transport = transport_with_measure(measure)?;

    let ordered = transport
        .order_dht_candidates_by_quality([degraded, unknown, healthy])
        .await;

    assert_eq!(ordered, vec![healthy, unknown, degraded]);

    Ok(())
}

#[tokio::test]
async fn malformed_outbound_payload_is_rejected_before_connection_admission() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let mut payload = MessagePayload::new_send(
        Message::custom(b"malformed outbound payload")?,
        &transport.session_sk,
        peer,
        peer,
    )?;
    payload.transaction.data = vec![0xff];

    assert!(matches!(
        transport.send_payload(payload).await,
        Err(Error::CodecDeserialize(_))
    ));
    Ok(())
}
