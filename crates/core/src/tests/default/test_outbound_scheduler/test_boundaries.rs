use super::*;

#[tokio::test]
async fn test_outbound_capacity_is_reserved_before_readiness_wait() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _pending_open = PendingDataChannelOpenGuard::new();
    let swarm = node1.swarm.clone();
    let send = tokio::spawn(async move {
        swarm
            .send_direct_message(Message::custom(b"reserve-before-readiness")?, peer)
            .await
    });

    wait_until("capacity reservation before readiness", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(1)
    })
    .await?;
    send.abort();
    let _ = send.await;
    wait_until("cancelled readiness wait capacity release", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(0)
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_pending_measurement_does_not_block_peer_scheduler() -> Result<()> {
    let started = Arc::new(AtomicBool::new(false));
    let measure: MeasureImpl = Arc::new(PendingMeasure {
        started: started.clone(),
    });
    let node1 = prepare_node_with_measure(SecretKey::random(), measure)?;
    let node2 = prepare_node(SecretKey::random()).await;
    let (node1, node2) = connect_nodes(node1, node2).await?;
    let peer = node2.did();

    node1
        .swarm
        .send_direct_message(Message::custom(b"first-measured-send")?, peer)
        .await?;
    wait_until("pending outbound measurement", || {
        started.load(Ordering::Acquire)
    })
    .await?;

    timeout(
        Duration::from_secs(1),
        node1
            .swarm
            .send_direct_message(Message::custom(b"scheduler-must-progress")?, peer),
    )
    .await
    .map_err(|_| invalid_test_state("measurement blocked the peer scheduler"))??;
    Ok(())
}

#[traced_test]
#[tokio::test]
async fn test_oversized_payload_log_omits_the_custom_message_body() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let marker = "private-oversized-custom-body";
    let oversized_bytes = TRANSPORT_MAX_SIZE + 8 * 1024 * 1024;
    let repeats = oversized_bytes / marker.len() + 1;
    let body = marker.repeat(repeats).into_bytes();
    let local = node1.did();
    let next_hop = node2.did();
    let destination: Did = SecretKey::random().address().into();
    let relay_destination: Did = SecretKey::random().address().into();
    let mut payload = MessagePayload::new_send(
        Message::CustomMessage(CustomMessage(body)),
        node1.swarm.transport.session_sk(),
        next_hop,
        destination,
    )?;
    payload.relay.destination = relay_destination;
    let expected_tx_id = payload.transaction.tx_id.to_string();
    let expected_wire_bytes = payload.wire_size()?;
    let error = node1
        .swarm
        .transport
        .send_payload(payload)
        .await
        .expect_err("oversized payload must be rejected before scheduling");

    assert!(matches!(error, Error::MessageTooLarge(size) if size == expected_wire_bytes));
    let expected_fields = [
        ("local", local.to_string()),
        ("next_hop", next_hop.to_string()),
        ("destination", destination.to_string()),
        ("relay_destination", relay_destination.to_string()),
        ("message_kind", "\"CustomMessage\"".to_owned()),
        ("bytes", expected_wire_bytes.to_string()),
        ("max_bytes", TRANSPORT_MAX_SIZE.to_string()),
    ];
    let marker_debug = crate::tests::byte_debug_fragment(marker.as_bytes());
    logs_assert(|lines: &[&str]| {
        crate::tests::assert_single_structured_log_event(
            lines,
            "rings_core::swarm::transport::payload_send",
            "message payload is too large",
            ("tx_id", &expected_tx_id),
            &expected_fields,
            &[marker, &marker_debug, "MessagePayload {"],
        )
    });
    Ok(())
}
