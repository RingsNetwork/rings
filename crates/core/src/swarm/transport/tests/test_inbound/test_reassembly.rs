//! Reassembled messages through the inbound path: one logical receive each, order kept
//! without blocking control, and the lane shape verified before the lane transition.

use super::*;

#[tokio::test]
async fn test_chunk_reassembly_records_one_exact_logical_receive() -> Result<()> {
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
    let chunks: Vec<Chunk> = Chunk::stream(logical_payload.to_wire()?, 32).collect();
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
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
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
    let chunks: Vec<Chunk> = Chunk::stream(undecodable.to_wire()?, 32).collect();
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
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
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
    let chunks: Vec<Chunk> = Chunk::stream(first_wire, 32).collect();
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
        noop_control_message(transport.dht.did),
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
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let control = local_wire(
        noop_control_message(transport.dht.did),
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
    let chunks: Vec<Chunk> = Chunk::stream(application.clone(), 32).collect();
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
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let mut tampered = MessagePayload::new_send(
        noop_control_message(transport.dht.did),
        MessageSigner::new(&session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?;
    tampered.transaction.data.push(0);
    assert_eq!(
        crate::message::MessageKind::from_wire(&tampered.transaction.data)?.class(),
        crate::message::MessageCategory::DhtControl
    );
    let tampered_wire = tampered.to_wire()?;
    let chunks: Vec<Chunk> = Chunk::stream(tampered_wire, 32).collect();
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
