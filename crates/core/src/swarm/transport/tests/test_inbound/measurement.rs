use super::*;

async fn admitted_callback(
    transport: Arc<SwarmTransport>,
    peer: Did,
) -> Result<InnerSwarmCallback> {
    let offer_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    Ok(
        InnerSwarmCallback::new(transport, Arc::new(NoopSwarmCallback))
            .with_pending_connection_attempt(attempt),
    )
}

#[tokio::test]
async fn test_malformed_unbound_transport_does_not_attribute_peer_failure() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer: Did = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(transport, Arc::new(NoopSwarmCallback));

    callback
        .on_admitted_message_for_test(&peer.to_string(), &[0xff])
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
        0
    );
    Ok(())
}

#[tokio::test]
async fn test_invalid_chunk_shape_records_peer_receive_failure() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = SessionSk::new_with_seckey(&peer_key)?;
    let callback = admitted_callback(Arc::clone(&transport), peer).await?;
    let frame = local_wire(
        Message::Chunk(Chunk {
            chunk: [0, 0],
            data: Bytes::from_static(b"invalid"),
            meta: crate::chunk::ChunkMeta::default(),
        }),
        &session,
        transport.dht.did,
    )?;

    let error = callback
        .on_admitted_message_for_test(&peer.to_string(), &frame)
        .await
        .expect_err("invalid chunk shape must fail");

    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InvalidChunkMessage)
    ));
    assert_eq!(failed_receive_count(&measure, peer)?, 1);
    Ok(())
}

#[tokio::test]
async fn test_reassembly_rejections_score_only_invalid_input_and_release_resources() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let limits = ReassemblyLimits {
        max_pending_messages: 1,
        max_chunk_data_len: 1_024,
        max_message_bytes: 1_024,
        max_chunks_per_message: 4,
        max_total_buffered_cost: 4_096,
        slot_overhead: 8,
        max_completed_ids: 4,
    };
    let transport = Arc::new(transport_with_measure_and_reassembly_limits(
        measure.clone(),
        limits,
    )?);
    let budget = transport.reassembly_budget();
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = SessionSk::new_with_seckey(&peer_key)?;
    let callback = admitted_callback(Arc::clone(&transport), peer).await?;
    let first = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"first"),
        meta: crate::chunk::ChunkMeta::default(),
    };
    let deliver = |chunk: Chunk| local_wire(Message::Chunk(chunk), &session, transport.dht.did);

    let first_frame = deliver(first.clone())?;
    callback
        .on_admitted_message_for_test(&peer.to_string(), &first_frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(budget.buffered_cost_for_test() > 0);

    let replay_frame = deliver(first.clone())?;
    callback
        .on_admitted_message_for_test(&peer.to_string(), &replay_frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    let capacity_frame = deliver(Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"second"),
        meta: crate::chunk::ChunkMeta::default(),
    })?;
    callback
        .on_admitted_message_for_test(&peer.to_string(), &capacity_frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(failed_receive_count(&measure, peer)?, 0);

    let conflict_frame = deliver(Chunk {
        data: Bytes::from_static(b"conflict"),
        ..first
    })?;
    let error = callback
        .on_admitted_message_for_test(&peer.to_string(), &conflict_frame)
        .await
        .expect_err("conflicting bytes at one position must fail");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InvalidChunkMessage)
    ));
    assert_eq!(failed_receive_count(&measure, peer)?, 1);
    assert_eq!(successful_receive_count(&measure, peer)?, 0);
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);

    callback
        .on_admitted_message_for_test(&peer.to_string(), &conflict_frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(
        failed_receive_count(&measure, peer)?,
        1,
        "repeated invalid evidence for one pending message must not double charge"
    );

    callback.close_inbound_for_test();
    drop(callback);
    budget.await_buffered_cost_for_test(|cost| cost == 0).await;
    Ok(())
}

#[tokio::test]
async fn test_expired_partial_reassembly_releases_shared_budget_without_more_peer_traffic(
) -> Result<()> {
    let limits = ReassemblyLimits {
        max_pending_messages: 1,
        max_chunk_data_len: 1_024,
        max_message_bytes: 1_024,
        max_chunks_per_message: 4,
        max_total_buffered_cost: 4_096,
        slot_overhead: 8,
        max_completed_ids: 4,
    };
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure_and_reassembly_limits(
        measure.clone(),
        limits,
    )?);
    let budget = transport.reassembly_budget();
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = SessionSk::new_with_seckey(&peer_key)?;
    let meta = crate::chunk::ChunkMeta {
        ttl_ms: 100,
        ..Default::default()
    };
    let expiry = meta.ts_ms.saturating_add(meta.ttl_ms as u128);
    let cleanup_now_ms = Arc::new(Mutex::new(meta.ts_ms));
    let offer_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = InnerSwarmCallback::new_with_reassembly_clock_for_test(
        Arc::clone(&transport),
        Arc::new(NoopSwarmCallback),
        Arc::clone(&cleanup_now_ms),
    )
    .with_pending_connection_attempt(attempt);
    let frame = local_wire(
        Message::Chunk(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"partial"),
            meta,
        }),
        &session,
        transport.dht.did,
    )?;

    callback
        .on_admitted_message_for_test(&peer.to_string(), &frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(budget.buffered_cost_for_test() > 0);
    // Admission and cleanup read the injected clock, so advancing it to the
    // expiry is the whole expiry event; no wall time needs to pass.
    *cleanup_now_ms
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = expiry;

    budget.await_buffered_cost_for_test(|cost| cost == 0).await;
    measure
        .await_recorded(|measure| matches!(failed_receive_count(measure, peer), Ok(1)))
        .await;

    callback
        .on_admitted_message_for_test(&peer.to_string(), &frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(
        failed_receive_count(&measure, peer)?,
        1,
        "a late chunk for an expired logical message must not charge it again"
    );

    let closing_meta = crate::chunk::ChunkMeta {
        ttl_ms: 100,
        ..Default::default()
    };
    let closing_expiry = closing_meta
        .ts_ms
        .saturating_add(closing_meta.ttl_ms as u128);
    *cleanup_now_ms
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = closing_meta.ts_ms;
    let closing_frame = local_wire(
        Message::Chunk(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"closing-partial"),
            meta: closing_meta,
        }),
        &session,
        transport.dht.did,
    )?;
    callback
        .on_admitted_message_for_test(&peer.to_string(), &closing_frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(budget.buffered_cost_for_test() > 0);
    let passes_before_close = callback.reassembly_cleanup_passes_for_test();
    callback.close_inbound_for_test();
    // The close path runs one cleanup pass immediately under the pre-expiry
    // clock. Awaiting that pass, rather than a wall-clock window, proves the
    // retention decision was taken and not merely not yet reached.
    callback
        .await_reassembly_cleanup_passes_for_test(|passes| passes > passes_before_close)
        .await;
    assert!(
        budget.buffered_cost_for_test() > 0,
        "closing before TTL must retain only bounded reassembly cleanup state"
    );
    assert_eq!(
        failed_receive_count(&measure, peer)?,
        1,
        "local close before TTL must not immediately penalize the peer"
    );
    *cleanup_now_ms
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = closing_expiry;
    budget.await_buffered_cost_for_test(|cost| cost == 0).await;
    measure
        .await_recorded(|measure| matches!(failed_receive_count(measure, peer), Ok(2)))
        .await;
    Ok(())
}

#[tokio::test]
async fn test_authenticated_partial_expiry_remains_attributable_after_disconnect() -> Result<()> {
    let limits = ReassemblyLimits {
        max_pending_messages: 1,
        max_chunk_data_len: 1_024,
        max_message_bytes: 1_024,
        max_chunks_per_message: 4,
        max_total_buffered_cost: 4_096,
        slot_overhead: 8,
        max_completed_ids: 4,
    };
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure_and_reassembly_limits(
        measure.clone(),
        limits,
    )?);
    let budget = transport.reassembly_budget();
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = SessionSk::new_with_seckey(&peer_key)?;
    let meta = crate::chunk::ChunkMeta {
        ttl_ms: 100,
        ..Default::default()
    };
    let expiry = meta.ts_ms.saturating_add(meta.ttl_ms as u128);
    let cleanup_now_ms = Arc::new(Mutex::new(meta.ts_ms));
    let offer_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = InnerSwarmCallback::new_with_reassembly_clock_for_test(
        Arc::clone(&transport),
        Arc::new(NoopSwarmCallback),
        Arc::clone(&cleanup_now_ms),
    )
    .with_pending_connection_attempt(attempt);
    let frame = local_wire(
        Message::Chunk(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"authenticated-partial"),
            meta,
        }),
        &session,
        transport.dht.did,
    )?;

    callback
        .on_admitted_message_for_test(&peer.to_string(), &frame)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(budget.buffered_cost_for_test() > 0);
    transport.disconnect(peer).await?;
    *cleanup_now_ms
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = expiry;

    budget.await_buffered_cost_for_test(|cost| cost == 0).await;
    measure
        .await_recorded(|measure| matches!(failed_receive_count(measure, peer), Ok(1)))
        .await;
    Ok(())
}

#[tokio::test]
async fn test_invalid_unbound_transport_envelope_does_not_attribute_peer_failure() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer: Did = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(transport, Arc::new(NoopSwarmCallback));

    callback
        .on_invalid_inbound_frame(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(failed_receive_count(&measure, peer)?, 0);
    Ok(())
}

#[test]
fn test_inbound_mailbox_without_runtime_returns_typed_error() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let wire = local_wire(
        Message::custom(b"valid-message")?,
        &peer_session,
        transport.dht.did,
    )?;
    let callback = InnerSwarmCallback::new(transport, Arc::new(NoopSwarmCallback));
    let is_runtime_error = futures::executor::block_on(async {
        match callback
            .on_admitted_message_for_test("not-a-did", &wire)
            .await
        {
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
