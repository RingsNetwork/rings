use super::*;

#[tokio::test]
async fn pending_finger_update_is_applied_when_attempt_is_admitted() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let finger_index = 3;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;

    assert_eq!(
        transport.record_finger_candidate(peer, finger_index)?,
        FingerUpdateDisposition::Queued
    );
    assert_eq!(transport.dht.lock_finger()?.get(finger_index), None);
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
        .with_pending_connection_attempt(attempt);
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.dht.lock_finger()?.get(finger_index), Some(peer));
    assert!(transport.is_admitted_connection(peer));

    transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn admitting_finger_update_is_retained_until_atomic_commit() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let finger_index = 3;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    assert!(transport.begin_connection_admission_for_test(attempt)?);
    let observed = transport
        .unadmitted_attempt(peer)?
        .ok_or_else(|| Error::InvalidMessage("admitting attempt disappeared".to_string()))?;
    assert_eq!(observed, attempt);
    assert_eq!(
        transport.record_finger_candidate(peer, finger_index)?,
        FingerUpdateDisposition::Queued
    );
    assert_eq!(transport.dht.lock_finger()?.get(finger_index), None);

    assert!(transport.commit_connection_admission(attempt)?.is_some());

    assert_eq!(transport.dht.lock_finger()?.get(finger_index), Some(peer));
    assert!(transport.is_admitted_connection_attempt(attempt));
    transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn pending_finger_update_applies_if_admission_wins_queue_race() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let finger_index = 4;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;

    assert!(transport.activate_connection_for_test(attempt)?);
    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connected)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    assert_eq!(
        transport.record_finger_candidate(peer, finger_index)?,
        FingerUpdateDisposition::Applied
    );

    assert_eq!(transport.dht.lock_finger()?.get(finger_index), Some(peer));
    assert!(transport.is_admitted_connection(peer));
    Ok(())
}

#[tokio::test]
async fn finger_candidate_distinguishes_missing_and_unroutable_connections() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let missing = SecretKey::random().address().into();
    assert_eq!(
        transport.record_finger_candidate(missing, 1)?,
        FingerUpdateDisposition::Missing
    );

    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    assert_eq!(
        transport.record_finger_candidate(peer, 1)?,
        FingerUpdateDisposition::Unroutable
    );
    assert_eq!(transport.dht.lock_finger()?.get(1), None);
    transport.disconnect(peer).await?;
    Ok(())
}
