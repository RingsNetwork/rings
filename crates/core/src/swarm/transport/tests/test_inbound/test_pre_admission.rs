use super::*;

/// A frame admitted by core while its generation was live is a verified
/// receive; if the generation is retired before the actor dispatches it, the
/// application never sees it.
#[tokio::test]
async fn test_retired_frame_queued_on_lane_is_not_delivered() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.activate(&transport)?;
    let (peer, attempt) = (pending.peer, pending.attempt()?);
    let admission_turn = pending
        .callback
        .inbound_for_test()
        .hold_application_admission_for_test()?;
    let frame = pending.custom_message_wire(&transport, b"retired-before-lane-ticket-release")?;
    let delivery = spawn_inbound_delivery(Arc::clone(&pending.callback), peer.to_string(), frame);

    // Core admission completes while the generation is live: the frame is
    // verified, counted as received, and queued behind the held lane. The
    // delivery itself completes only when the actor dispatches the frame.
    measure
        .await_recorded(|measure| {
            successful_receive_count(measure, peer).is_ok_and(|count| count == 1)
        })
        .await;
    assert_eq!(
        pending
            .callback
            .inbound_for_test()
            .admitted_count_for_test(),
        1
    );
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
    pending
        .callback
        .inbound_for_test()
        .await_admitted_count_for_test(|admitted| admitted == 0)
        .await;
    assert_eq!(successful_receive_count(&measure, peer)?, 1);
    assert_eq!(application.observed(), 0);
    assert_eq!(application.inbounds(), 0);
    Ok(())
}
