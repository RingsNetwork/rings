use super::*;

#[tokio::test]
async fn detached_delivery_timeout_releases_transfer_capacity() -> Result<()> {
    let measure = Arc::new(FailedSendMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;
    let (node1, node2) = connect_nodes(node1, node2).await?;
    let peer = node2.did();
    let _pending_delivery = PendingDeliveryGuard::new();

    node1
        .swarm
        .send_direct_message(Message::custom(b"bounded-detached-delivery")?, peer)
        .await?;
    wait_until("detached transfer admission", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(1)
    })
    .await?;
    timeout(Duration::from_secs(2), async {
        while !outbound_capacity_released(&node1.swarm.transport, peer) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| invalid_test_state("detached delivery timeout retained capacity"))?;
    wait_for_msgs([&node1, &node2]).await;
    assert_eq!(
        measure.count(),
        0,
        "local delivery timeout must not degrade peer quality"
    );
    Ok(())
}

#[tokio::test]
async fn detached_first_frame_cleanup_is_bounded_and_retires_generation() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let paused_send = PausedIrrevocableSendGuard::new();
    let _pending_close = PendingCloseGuard::new();
    let payload = tracked_payload(&node1, peer, b"bounded-detached-cleanup")?;
    let deadline = async {
        while !dummy_controlled::irrevocable_send_gate_waiting() {
            tokio::task::yield_now().await;
        }
    };

    let error = timeout(
        Duration::from_secs(2),
        node1.swarm.transport.send_payload_detached_until_for_test(
            payload,
            Duration::from_millis(1),
            deadline,
        ),
    )
    .await
    .map_err(|_| invalid_test_state("detached cleanup exceeded its total test bound"))?
    .expect_err("stalled detached cleanup must retire its connection generation");

    assert!(matches!(
        error,
        Error::DetachedPayloadCleanupTimeout { peer: timed_out, .. } if timed_out == peer
    ));
    assert!(node1.swarm.transport.get_connection(peer).is_none());
    drop(paused_send);
    wait_until("abandoned irrevocable dummy send", || {
        !dummy_controlled::irrevocable_send_gate_waiting()
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn delivery_timeout_marks_generation_terminal_before_releasing_fifo_lane() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    let _paused_delivery = PausedDeliveryGuard::new();
    let _pending_close = PendingCloseGuard::new();
    dummy_controlled::reset_sent_count();

    node1
        .swarm
        .send_direct_message(Message::custom(b"first-stalled-delivery")?, peer)
        .await?;
    wait_until("first delivery gate", || {
        dummy_controlled::delivery_future_waiting()
    })
    .await?;

    let second_swarm = node1.swarm.clone();
    let second_send = tokio::spawn(async move {
        second_swarm
            .send_direct_message(Message::custom(b"second-fifo-transfer")?, peer)
            .await
    });
    wait_until("second FIFO transfer submission", || {
        node1
            .swarm
            .transport
            .outbound_admitted_transfer_count_for_test(peer)
            == Some(2)
    })
    .await?;
    wait_until("timed-out generation send revocation", || {
        node1.swarm.transport.get_connection(peer).is_none()
    })
    .await?;
    assert!(node1.swarm.transport.is_admitted_connection(peer));

    dummy_controlled::release_delivery_future_gate();
    let second_result = timeout(Duration::from_secs(2), second_send)
        .await
        .map_err(|_| invalid_test_state("second FIFO submission did not finish"))?
        .map_err(|error| invalid_test_state(format!("second FIFO task failed: {error}")))?;
    assert!(
        matches!(
            &second_result,
            Err(Error::OutboundFirstFrameAdmissionTimeout {
                peer: failed_peer,
                ..
            }) if *failed_peer == peer
        ),
        "unexpected terminal-generation result: {second_result:?}"
    );
    assert_eq!(dummy_controlled::sent_count(), 1);
    timeout(
        Duration::from_secs(2),
        node1.swarm.stabilizer().clean_unavailable_connections(),
    )
    .await
    .map_err(|_| invalid_test_state("terminal generation cleanup stayed pending"))??;
    assert!(!node1.swarm.transport.is_admitted_connection(peer));
    Ok(())
}
